# multipart-async-stream

[![Crates.io](https://img.shields.io/crates/v/multipart_async_stream.svg)](https://crates.io/crates/multipart_async_stream)
[![Documentation](https://docs.rs/multipart_async_stream/badge.svg)](https://docs.rs/multipart_async_stream)

A high-performance, zero-copy streaming multipart parser for Rust.

This is a **general-purpose multipart parser** compatible with all [RFC 2046](https://datatracker.ietf.org/doc/html/rfc2046#section-5.1) multipart types (`form-data`, `byteranges`, `mixed`, `alternative`, `related`, etc.). The parser handles boundary detection and part streaming, while you handle the type-specific semantics in your application code.

## Features

- **Zero-copy parsing** - Uses `memchr` for efficient pattern matching
- **Streaming API** - Process data incrementally without buffering the entire input
- **Universal support** - Works with all standard multipart types

## Quick Start

```rust
use multipart_async_stream::{MultipartStream, LendingIterator};
use bytes::Bytes;
use futures_util::{stream, TryStreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = b"\
        --boundary\r\n\
        Content-Disposition: form-data; name=\"field1\"\r\n\
        \r\n\
        value1\r\n\
        --boundary--\r\n";

    let stream = stream::iter(vec![
        Result::<Bytes, std::convert::Infallible>::Ok(Bytes::from(&data[..]))
    ]);
    let mut multipart = MultipartStream::new(stream, b"boundary");

    while let Some(Ok(part)) = multipart.next().await {
        println!("Headers: {:?}", part.headers());
        let mut body = part.body();
        while let Some(chunk) = body.try_next().await? {
            println!("Body: {:?}", chunk);
        }
    }

    Ok(())
}
```

## Important Usage Notes

### You MUST consume each part's body before requesting the next part

This library uses a lending iterator pattern where each `Part` must be fully consumed before calling `next()`:

1. **Access headers before body**: Calling `part.body()` consumes the part, so you must access `part.headers()` first
2. **Consume the body completely**: The body stream must be fully consumed (until it returns `None`) before calling `next()` again
3. **Failure to comply**: Will result in a `BodyNotConsumed` error

#### ❌ Wrong

```rust
let part = multipart.next().await?.unwrap();
let body = part.body(); // Consumes the part
println!("{:?}", part.headers()); // ERROR: part is already consumed!
```

#### ✅ Right

```rust
while let Some(Ok(part)) = multipart.next().await {
    let headers = part.headers(); // Access headers first
    println!("{:?}", headers);
    let mut body = part.body(); // Then consume the part to get body
    while let Some(chunk) = body.try_next().await? {
        // Process chunk...
    }
}
```

## HTTP Range Request Example

```rust
use multipart_async_stream::{MultipartStream, LendingIterator, TryStreamExt, header::CONTENT_TYPE};

#[tokio::main]
async fn main() {
    let client = reqwest::Client::new();
    let response = client
        .get("https://example.com/file.bin")
        .header("Range", "bytes=0-1023,2048-3071")
        .send()
        .await
        .unwrap();

    // Extract multipart type and boundary from Content-Type header
    // Example: "multipart/byteranges; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW"
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .expect("multipart content-type");

    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .map(|s| s.trim().as_bytes())
        .expect("boundary parameter");

    let mut multipart = MultipartStream::new(response.bytes_stream(), boundary);

    while let Some(Ok(part)) = multipart.next().await {
        let headers = part.headers();
        println!("{:?}", headers);
        let mut body = part.body();
        while let Ok(Some(chunk)) = body.try_next().await {
            // Process each range...
        }
    }
}
```

## Performance

- **O(n) time** - Single pass through the input
- **O(1) space** - Fixed buffer size regardless of input size
- **Zero-copy** - Body chunks are views into the input buffer
- **memchr-optimized** - Uses SIMD-accelerated pattern matching
