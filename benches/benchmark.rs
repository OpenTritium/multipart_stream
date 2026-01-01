// Benchmark comparing multipart_async_stream vs multipart-rs
//
// Run with: cargo bench --bench benchmark

use bytes::Bytes;
use criterion::{Criterion, Throughput, async_executor::FuturesExecutor, criterion_group, criterion_main};
use futures_util::{TryStream, TryStreamExt, stream};
use multipart_async_stream::{LendingIterator, MultipartStream};
use std::convert::Infallible;

// Helper to create a stream from chunks
fn create_stream_from_chunks(data: &[u8], chunk_size: usize) -> impl TryStream<Ok = Bytes, Error = Infallible> {
    let chunks: Vec<Result<Bytes, Infallible>> =
        data.chunks(chunk_size).map(|chunk| Ok(Bytes::from(chunk.to_vec()))).collect();
    stream::iter(chunks)
}

// Generate test multipart data
fn generate_multipart_data(boundary: &str, num_parts: usize, body_size: usize) -> Vec<u8> {
    let mut data = String::new();

    for i in 0..num_parts {
        data.push_str(&format!("--{}\r\nContent-Disposition: form-data; name=\"field{}\"\r\n\r\n", boundary, i));

        // Add body content
        for _ in 0..body_size {
            data.push('x');
        }
        data.push_str("\r\n");
    }

    data.push_str(&format!("--{}--\r\n", boundary));
    data.into_bytes()
}

// Benchmark: multipart_async_stream - parse multiple parts with small bodies
async fn bench_multipart_async_stream_small_parts(data: &[u8], boundary: &[u8]) {
    let stream = create_stream_from_chunks(data, 64);
    let mut multipart = MultipartStream::new(stream, boundary);

    while let Some(Ok(part)) = multipart.next().await {
        let _headers = part.headers();
        let mut body_stream = part.body();
        while let Ok(Some(_chunk)) = body_stream.try_next().await {
            // Consume body
        }
    }
}

// Benchmark: multipart-rs - parse multiple parts with small bodies
async fn bench_multipart_rs_small_parts(data: Vec<u8>, boundary: &str) {
    use futures_util::StreamExt;
    use multipart_rs::MultipartReader;

    let stream = stream::iter(vec![Ok::<_, Infallible>(Bytes::from(data))]);
    let mut reader = MultipartReader::from_stream_with_boundary_and_type(
        Box::pin(stream),
        boundary,
        multipart_rs::MultipartType::FormData,
    )
    .unwrap();

    while let Some(Ok(item)) = reader.next().await {
        drop(item.data);
        drop(item.headers);
    }
}

// Benchmark: multipart_async_stream - single large part
async fn bench_multipart_async_stream_large_body(data: &[u8], boundary: &[u8]) {
    let stream = create_stream_from_chunks(data, 8192);
    let mut multipart = MultipartStream::new(stream, boundary);

    while let Some(Ok(part)) = multipart.next().await {
        let _headers = part.headers();
        let mut body_stream = part.body();
        while let Ok(Some(_chunk)) = body_stream.try_next().await {
            // Consume body
        }
    }
}

// Benchmark: multipart-rs - single large part
async fn bench_multipart_rs_large_body(data: Vec<u8>, boundary: &str) {
    use futures_util::StreamExt;
    use multipart_rs::MultipartReader;

    let stream = stream::iter(vec![Ok::<_, Infallible>(Bytes::from(data))]);
    let mut reader = MultipartReader::from_stream_with_boundary_and_type(
        Box::pin(stream),
        boundary,
        multipart_rs::MultipartType::FormData,
    )
    .unwrap();

    while let Some(Ok(item)) = reader.next().await {
        drop(item.data);
        drop(item.headers);
    }
}

fn benchmark_small_parts(c: &mut Criterion) {
    let boundary = "boundary123";
    let num_parts = 100;
    let body_size = 100;

    let data = generate_multipart_data(boundary, num_parts, body_size);
    let data_len = data.len();

    let mut group = c.benchmark_group("small_parts");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(100);

    group.bench_function("multipart_async_stream", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_async_stream_small_parts(&data, boundary.as_bytes()));
    });

    group.bench_function("multipart-rs", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_rs_small_parts(data.clone(), boundary));
    });

    group.finish();
}

fn benchmark_large_body(c: &mut Criterion) {
    let boundary = "boundary123";
    let body_size = 1_000_000; // 1MB

    let data = {
        let mut d = String::new();
        d.push_str(&format!("--{}\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n", boundary));
        for _ in 0..body_size {
            d.push('x');
        }
        d.push_str(&format!("\r\n--{}--\r\n", boundary));
        d.into_bytes()
    };

    let data_len = data.len();

    let mut group = c.benchmark_group("large_body");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(50);

    group.bench_function("multipart_async_stream", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_async_stream_large_body(&data, boundary.as_bytes()));
    });

    group.bench_function("multipart-rs", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_rs_large_body(data.clone(), boundary));
    });

    group.finish();
}

fn benchmark_realistic_file_upload(c: &mut Criterion) {
    // Simulate a realistic file upload scenario with multiple fields
    let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";

    let data = {
        let mut d = String::new();

        // Text field
        d.push_str(&format!("--{}\r\nContent-Disposition: form-data; name=\"username\"\r\n\r\nuser123\r\n", boundary));

        // Email field
        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"email\"\r\n\r\nuser@example.com\r\n",
            boundary
        ));

        // File upload
        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"document.txt\"\r\nContent-Type: \
             text/plain\r\n\r\n",
            boundary
        ));

        // File content (100KB)
        for _ in 0..100_000 {
            d.push('A');
        }

        d.push_str(&format!("\r\n--{}--\r\n", boundary));
        d.into_bytes()
    };

    let data_len = data.len();

    let mut group = c.benchmark_group("realistic_upload");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(100);

    group.bench_function("multipart_async_stream", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_async_stream_large_body(&data, boundary.as_bytes()));
    });

    group.bench_function("multipart-rs", |b| {
        b.to_async(FuturesExecutor).iter(|| bench_multipart_rs_large_body(data.clone(), boundary));
    });

    group.finish();
}

fn benchmark_chunk_size_variations(c: &mut Criterion) {
    let boundary = "boundary123";
    let num_parts = 50;
    let body_size = 500;

    let data = generate_multipart_data(boundary, num_parts, body_size);
    let data_len = data.len();

    let mut group = c.benchmark_group("chunk_variations");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(50);

    // Test with different chunk sizes
    for chunk_size in [16, 64, 256, 1024] {
        let cs = chunk_size;
        group.bench_with_input(format!("multipart_async_stream/chunk_{}", chunk_size), &cs, |b, &_cs| {
            b.to_async(FuturesExecutor).iter(|| {
                let data = data.clone();
                async move {
                    let stream = create_stream_from_chunks(&data, cs);
                    let mut multipart = MultipartStream::new(stream, boundary.as_bytes());

                    while let Some(Ok(part)) = multipart.next().await {
                        let _headers = part.headers();
                        let mut body_stream = part.body();
                        while let Ok(Some(_chunk)) = body_stream.try_next().await {
                            // Consume body
                        }
                    }
                }
            });
        });
    }

    group.finish();
}

// Benchmark: Real-world scenario - multiple files upload with actual data consumption
fn benchmark_real_world_multi_file_upload(c: &mut Criterion) {
    let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";

    let data = {
        let mut d = String::new();

        // Multiple form fields
        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nTest Document\r\n",
            boundary
        ));
        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\nThis is a test document for \
             benchmarking\r\n",
            boundary
        ));

        // Multiple small files
        for i in 0..5 {
            d.push_str(&format!(
                "--{}\r\nContent-Disposition: form-data; name=\"file{}\"; \
                 filename=\"document{}.txt\"\r\nContent-Type: text/plain\r\n\r\n",
                boundary, i, i
            ));
            // 10KB file content
            for _ in 0..10_000 {
                d.push((b'A' + (i % 26) as u8) as char);
            }
            d.push_str("\r\n");
        }

        // One large file (1MB)
        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"large_file\"; filename=\"large.bin\"\r\nContent-Type: \
             application/octet-stream\r\n\r\n",
            boundary
        ));
        for i in 0..1_000_000 {
            d.push((b'0' + (i % 10) as u8) as char);
        }
        d.push_str("\r\n");

        d.push_str(&format!("--{}--\r\n", boundary));
        d.into_bytes()
    };

    let data_len = data.len();

    let mut group = c.benchmark_group("real_world_multi_file");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(50);

    group.bench_function("multipart_async_stream", |b| {
        b.to_async(FuturesExecutor).iter(|| {
            let data = data.clone();
            async move {
                // Use realistic 8KB chunks (typical TCP buffer size)
                let stream = create_stream_from_chunks(&data, 8192);
                let mut multipart = MultipartStream::new(stream, boundary.as_bytes());

                let mut part_count = 0;
                let mut total_bytes = 0;

                while let Some(Ok(part)) = multipart.next().await {
                    part_count += 1;
                    let _headers = part.headers();
                    let mut body_stream = part.body();

                    // Actually consume the data
                    while let Ok(Some(chunk)) = body_stream.try_next().await {
                        total_bytes += chunk.len();
                        // Simulate processing the data
                        let _sum = chunk.iter().map(|&b| b as usize).sum::<usize>();
                    }
                }

                // Ensure we processed all parts
                assert_eq!(part_count, 8); // 2 fields + 5 small files + 1 large file
                assert!(total_bytes > 1_000_000);
            }
        });
    });

    group.finish();
}

// Benchmark: Large file upload (10MB) - simulates real upload scenario
fn benchmark_large_file_upload(c: &mut Criterion) {
    let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";

    let data = {
        let mut d = String::new();

        d.push_str(&format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"large_video.mp4\"\r\nContent-Type: \
             video/mp4\r\n\r\n",
            boundary
        ));

        // 10MB file content
        for i in 0..10_000_000 {
            d.push((b'0' + (i % 10) as u8) as char);
        }

        d.push_str(&format!("\r\n--{}--\r\n", boundary));
        d.into_bytes()
    };

    let data_len = data.len();

    let mut group = c.benchmark_group("large_file_upload_10mb");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(20);

    group.bench_function("multipart_async_stream", |b| {
        b.to_async(FuturesExecutor).iter(|| {
            let data = data.clone();
            async move {
                // Realistic chunk size: 16KB (common HTTP chunk size)
                let stream = create_stream_from_chunks(&data, 16384);
                let mut multipart = MultipartStream::new(stream, boundary.as_bytes());

                let mut total_bytes = 0;

                while let Some(Ok(part)) = multipart.next().await {
                    let _headers = part.headers();
                    let mut body_stream = part.body();

                    while let Ok(Some(chunk)) = body_stream.try_next().await {
                        total_bytes += chunk.len();
                    }
                }

                assert!(total_bytes >= 10_000_000);
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_small_parts,
    benchmark_large_body,
    benchmark_realistic_file_upload,
    benchmark_chunk_size_variations,
    benchmark_real_world_multi_file_upload,
    benchmark_large_file_upload
);
criterion_main!(benches);
