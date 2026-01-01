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

/// Generates a random valid multipart stream from fuzz input
fn generate_multipart_stream(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }

    let mut idx = 0;

    // Extract boundary length (1-70 bytes as per RFC)
    let boundary_len = ((data[idx] as usize) % 30) + 5; // 5-34 bytes, reasonable range
    idx += 1;

    if data.len() < idx + boundary_len {
        return None;
    }

    // Extract boundary (ensure it doesn't contain problematic characters)
    let boundary_bytes = &data[idx..idx + boundary_len];
    let boundary: String = boundary_bytes
        .iter()
        .map(|&b| match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' => b as char,
            _ => 'X',
        })
        .collect();
    idx += boundary_len;

    // Number of parts (0-10)
    if idx >= data.len() {
        return None;
    }
    let num_parts = (data[idx] % 5) + 1; // 1-5 parts
    idx += 1;

    let mut stream_data = Vec::new();

    // Optional preamble (random chance)
    if idx < data.len() && data[idx] % 3 == 0 {
        let preamble_len = ((data[idx] as usize) % 20) + 1;
        idx += 1;
        if idx + preamble_len <= data.len() {
            stream_data.extend_from_slice(&data[idx..idx + preamble_len]);
            stream_data.extend_from_slice(b"\r\n");
            idx += preamble_len;
        }
    }

    // Generate each part
    for part_idx in 0..num_parts {
        // Start boundary
        stream_data.extend_from_slice(b"--");
        stream_data.extend_from_slice(boundary.as_bytes());
        stream_data.extend_from_slice(b"\r\n");

        // Generate headers (simplified but valid)
        let has_content_disposition = idx < data.len() && data[idx] % 2 == 0;
        if has_content_disposition {
            stream_data.extend_from_slice(b"Content-Disposition: form-data; name=\"field");
            stream_data.extend_from_slice(part_idx.to_string().as_bytes());
            stream_data.extend_from_slice(b"\"\r\n");
            idx += 1;
        }

        let has_content_type = idx < data.len() && data[idx] % 3 == 0;
        if has_content_type {
            stream_data.extend_from_slice(b"Content-Type: text/plain\r\n");
            idx += 1;
        }

        // Add a random header occasionally
        if idx < data.len() && data[idx] % 5 == 0 {
            stream_data.extend_from_slice(b"X-Custom-Header: value\r\n");
            idx += 1;
        }

        // Empty line to end headers
        stream_data.extend_from_slice(b"\r\n");

        // Body content (use remaining fuzz data)
        if idx < data.len() {
            let body_len = std::cmp::min(((data[idx] as usize) % 100) + 1, data.len() - idx);
            idx += 1;
            if idx + body_len <= data.len() {
                // Inject boundary-like strings to test edge cases
                let body_data = &data[idx..idx + body_len];
                stream_data.extend_from_slice(body_data);

                // Occasionally inject something that looks like the boundary
                if part_idx % 2 == 0 && !body_data.is_empty() {
                    let boundary_like = format!(" --{} ", boundary);
                    if body_data.len() > boundary_like.len() {
                        // Note: We could inject at a specific position if needed
                        stream_data.extend_from_slice(boundary_like.as_bytes());
                    }
                }

                idx += body_len;
            }
        }

        stream_data.extend_from_slice(b"\r\n");
    }

    // End boundary
    stream_data.extend_from_slice(b"--");
    stream_data.extend_from_slice(boundary.as_bytes());
    stream_data.extend_from_slice(b"--\r\n");

    Some(stream_data)
}

fn do_fuzz(data: &[u8]) {
    // Generate a valid multipart stream structure from the fuzz input
    let Some(stream_data) = generate_multipart_stream(data) else {
        return;
    };

    // Extract boundary from the generated stream for parsing
    // The boundary is always between "--" and first "\r\n"
    let boundary_start = 2; // Skip "--"
    let boundary_end = stream_data[boundary_start..]
        .iter()
        .position(|&b| b == b'\r')
        .map(|p| boundary_start + p)
        .unwrap_or(stream_data.len());

    if boundary_end <= boundary_start {
        return;
    }

    let boundary = &stream_data[boundary_start..boundary_end];

    // Skip preamble if present
    let body_start = if stream_data.get(0..2) == Some(b"--") {
        0
    } else {
        // Find first "--"
        stream_data
            .iter()
            .position(|&b| b == b'-')
            .unwrap_or(0)
    };
    let body = &stream_data[body_start..];

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Test with different chunk sizes to stress test buffering
        for chunk_size in [1, 3, 7, 16, 32, 64, 128] {
            let s = create_stream_from_chunks(body, chunk_size);
            let mut m = MultipartStream::new(s, boundary);

            loop {
                match m.next().await {
                    Some(Ok(part)) => {
                        let _headers = part.headers();
                        // Consume the body stream completely
                        let mut body_stream = part.body();
                        while let Ok(Some(_chunk)) = body_stream.try_next().await {
                            // Just consume the data
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
    do_fuzz(data);
});
