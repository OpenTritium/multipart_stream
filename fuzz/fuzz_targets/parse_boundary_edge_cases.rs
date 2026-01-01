#![no_main]
use bytes::Bytes;
use futures_util::stream;
use libfuzzer_sys::fuzz_target;
use multipart_async_stream::{LendingIterator, MultipartStream, TryStream, TryStreamExt};
use std::convert::Infallible;
use tokio::runtime::Builder;

/// Creates a stream from chunks
fn create_stream_from_chunks(data: &[u8], chunk_size: usize) -> impl TryStream<Ok = Bytes, Error = Infallible> {
    let chunks: Vec<Result<Bytes, Infallible>> =
        data.chunks(chunk_size).map(|chunk| Ok(Bytes::from(chunk.to_vec()))).collect();
    stream::iter(chunks)
}

/// Generate edge case boundaries
fn do_fuzz_boundary_edge_cases(data: &[u8]) {
    if data.len() < 5 {
        return;
    }

    let idx = 0;

    // Determine boundary test type
    let test_type = data[idx] % 8;
    let boundary = match test_type {
        // Very short boundary (1 byte)
        0 => "a".repeat(1),
        // Very long boundary (70 bytes - RFC limit)
        1 => "a".repeat(70),
        // Boundary with special characters
        2 => {
            let chars = data[1..].iter().take(20).map(|&b| b as char).collect::<String>();
            format!("{}-boundary-{}", chars, "x".repeat(5))
        }
        // Boundary with only special chars
        3 => "---___---".to_string(),
        // Boundary with CRLF-like sequences
        4 => "a\r\nb".to_string(),
        // Boundary starting/ending with hyphen
        5 => "-boundary-".to_string(),
        // Boundary with repeating patterns
        6 => "abcabcabc".to_string(),
        // Boundary with mixed case
        7 => "BoUnDaRy123".to_string(),
        _ => "boundary".to_string(),
    };

    // Number of parts (0-5)
    let num_parts = if data.len() > 1 {
        (data[1] % 6) as usize
    } else {
        1
    };

    let mut stream_data = Vec::new();

    // Generate parts
    for i in 0..num_parts {
        stream_data.extend_from_slice(b"--");
        stream_data.extend_from_slice(boundary.as_bytes());
        stream_data.extend_from_slice(b"\r\n");

        // Add basic header
        stream_data.extend_from_slice(b"Content-Disposition: form-data; name=\"field");
        stream_data.extend_from_slice(i.to_string().as_bytes());
        stream_data.extend_from_slice(b"\"\r\n");

        // Body content that might include boundary-like strings
        if data.len() > 10 + i * 10 {
            let body_data = &data[10 + i * 10..];
            let body_len = std::cmp::min(50, body_data.len());

            // Occasionally inject boundary-like patterns
            if i % 2 == 0 && body_len > boundary.len() {
                stream_data.extend_from_slice(&body_data[..body_len / 2]);
                stream_data.extend_from_slice(b" --");
                stream_data.extend_from_slice(boundary.as_bytes());
                stream_data.extend_from_slice(b" ");
                if body_len > body_len / 2 + boundary.len() + 3 {
                    stream_data.extend_from_slice(&body_data[body_len / 2..body_len]);
                }
            } else {
                stream_data.extend_from_slice(&body_data[..body_len]);
            }
        } else {
            stream_data.extend_from_slice(b"default body");
        }

        stream_data.extend_from_slice(b"\r\n");
    }

    // End boundary
    stream_data.extend_from_slice(b"--");
    stream_data.extend_from_slice(boundary.as_bytes());
    stream_data.extend_from_slice(b"--\r\n");

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Test with various chunk sizes to stress boundary detection
        for chunk_size in [1, 2, 3, 4, 5, 7, 8, 16, 32, 64] {
            let s = create_stream_from_chunks(&stream_data, chunk_size);
            let mut m = MultipartStream::new(s, boundary.as_bytes());

            loop {
                match m.next().await {
                    Some(Ok(part)) => {
                        let _headers = part.headers();
                        // Consume body
                        let mut body_stream = part.body();
                        while let Ok(Some(_chunk)) = body_stream.try_next().await {
                            // Just consume
                        }
                    }
                    Some(Err(_)) => break,
                    None => break,
                }
            }
        }
    });
}

fuzz_target!(|data: &[u8]| {
    do_fuzz_boundary_edge_cases(data);
});
