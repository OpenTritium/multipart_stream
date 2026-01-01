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

/// Generate multipart with various header combinations
fn do_fuzz_headers(data: &[u8]) {
    if data.len() < 10 {
        return;
    }

    // Extract boundary (5-20 bytes)
    let boundary_len = ((data[0] as usize) % 16) + 5;
    if data.len() < 1 + boundary_len {
        return;
    }

    let boundary_bytes = &data[1..=boundary_len];
    let boundary: String = boundary_bytes
        .iter()
        .map(|&b| match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' => b as char,
            _ => 'X',
        })
        .collect();

    // Number of headers to add (0-10)
    let num_headers = if data.len() > boundary_len + 1 {
        (data[boundary_len + 1] % 11) as usize
    } else {
        0
    };

    let mut stream_data = Vec::new();
    stream_data.extend_from_slice(b"--");
    stream_data.extend_from_slice(boundary.as_bytes());
    stream_data.extend_from_slice(b"\r\n");

    // Add various headers
    for i in 0..num_headers {
        let header_idx = boundary_len + 2 + i * 2;
        if header_idx + 2 >= data.len() {
            break;
        }

        let header_type = data[header_idx] % 5;
        let header_name = match header_type {
            0 => "Content-Disposition",
            1 => "Content-Type",
            2 => "X-Custom-Header",
            3 => "Cache-Control",
            4 => "Content-Transfer-Encoding",
            _ => "X-Unknown",
        };

        let header_value_len = ((data[header_idx + 1] as usize) % 100) + 1;

        stream_data.extend_from_slice(header_name.as_bytes());
        stream_data.extend_from_slice(b": ");

        // Add header value from fuzz data
        let value_start = header_idx + 2;
        let value_end = std::cmp::min(value_start + header_value_len, data.len());

        // Sanitize header value to avoid invalid characters
        for &b in &data[value_start..value_end] {
            match b {
                b'\r' => stream_data.extend_from_slice(b"\r\n "), // Wrap to next line
                b'\n' => stream_data.extend_from_slice(b"\n "),
                0x00..=0x1F | 0x7F => stream_data.extend_from_slice(b"?"), // Replace control chars
                _ => stream_data.push(b),
            }
        }

        stream_data.extend_from_slice(b"\r\n");
    }

    // Empty line and minimal body
    stream_data.extend_from_slice(b"\r\n");
    stream_data.extend_from_slice(b"body\r\n");
    stream_data.extend_from_slice(b"--");
    stream_data.extend_from_slice(boundary.as_bytes());
    stream_data.extend_from_slice(b"--\r\n");

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Test with different chunk sizes
        for chunk_size in [1, 3, 7, 16, 32, 64, 128, 256] {
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
    do_fuzz_headers(data);
});
