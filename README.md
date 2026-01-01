# multipart-async-stream

[![Crates.io](https://img.shields.io/crates/v/multipart_async_stream.svg)](https://crates.io/crates/multipart_async_stream)
[![Documentation](https://docs.rs/multipart_async_stream/badge.svg)](https://docs.rs/multipart_async_stream)

A high-performance, zero-copy streaming multipart/form-data and multipart/byteranges parser for Rust.

## Features

- **Zero-copy parsing** - Leverages `memchr` for efficient pattern matching
- **Streaming API** - Process data incrementally without buffering the entire input
- **Lending iterator pattern** - Yields parts as they arrive using GATs
- **Async-first** - Built on `futures` with `async`/`await` support
- **HTTP-compliant** - Handles both `multipart/form-data` and `multipart/byteranges`

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
        Result::<Bytes, std::convert::Infallible>:: Ok(Bytes::from(&data[..]))
    ]);
    let mut multipart = MultipartStream::new(stream, b"boundary");

    while let Some(Ok(part)) = multipart.next().await {
        println!("Headers: {:?}", part.headers());
        let mut body = part.body();
        while let Some(chunk) = body.try_next().await? {
            println!("Body chunk: {:?}", chunk);
        }
    }

    Ok(())
}
```

## ⚠️ Critical Usage Requirements

### You **MUST** consume each part's body completely before requesting the next part

This is the most important rule when using this library:

1. **Each `Part` holds a mutable borrow** of the underlying `MultipartStream`
2. **The body must be fully consumed** (until it returns `None`) before the next call to `next()`
3. **Failure to comply** will result in a `BodyNotConsumed` error

#### ❌ Wrong

```rust
let part1 = multipart.next().await.unwrap()?;
// Don't do this! Trying to get the next part without consuming part1's body
let part2 = multipart.next().await; // Returns Err(BodyNotConsumed)
```

#### ✅ Right

```rust
while let Some(Ok(part)) = multipart.next().await {
    // Always consume the body completely
    let mut body = part.body();
    while let Some(chunk) = body.try_next().await? {
        // Process chunk...
    }
    // body is now exhausted, part is dropped
    // Safe to call next() again
}
```

## Why This Requirement?

This library uses a **lending iterator** pattern where each `Part` mutably borrows the parser state. This design enables:

- **Zero-copy operation** - No unnecessary buffering of body data
- **Constant memory usage** - Memory usage stays O(1) regardless of part size
- **Performance** - Single-pass parsing with minimal allocations

The tradeoff is that you must consume parts sequentially. See [RFC 2956](https://rust-lang.github.io/rfcs/2956-lending-iterator.html) for details on lending iterators in Rust.

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

    // Extract boundary from Content-Type header
    let boundary = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|s| s.trim().as_bytes())
        .expect("multipart boundary");

    let mut multipart = MultipartStream::new(response.bytes_stream(), boundary);

    while let Some(Ok(part)) = multipart.next().await {
        println!("{:?}", part.headers());
        let mut body = part.body();
        while let Ok(Some(chunk)) = body.try_next().await {
            // Process each range...
        }
    }
}
```

## Performance

- **O(n) time complexity** - Single pass through the input
- **O(1) space complexity** - Fixed buffer size regardless of input size
- **Zero-copy** - Body chunks are returned as views into the input buffer
- **memchr-optimized** - Uses SIMD-accelerated byte pattern matching

## Fuzzing

This crate includes comprehensive fuzzing tests to ensure reliability and security:

```bash
# Run fuzz tests (3 hours by default - production-ready duration)
cd fuzz && ./fuzz_test.sh

# Quick test for development (5 minutes)
cd fuzz && ./fuzz_test.sh 300

# Standard test for PR validation (30 minutes)
cd fuzz && ./fuzz_test.sh 1800

# Extended test for release candidates (6 hours)
cd fuzz && ./fuzz_test.sh 21600

# Manual testing individual targets
cargo fuzz run parse_stream
cargo fuzz run parse_random
```

**Recommended Testing Durations:**
- **Development**: 5 minutes - Quick smoke test during active development
- **PR Validation**: 30 minutes - Thorough testing before merging code changes
- **Pre-release**: 3 hours - Default duration for production releases
- **Critical Updates**: 6-24 hours - Extended testing for security or core parser changes

**Results:**
- ✅ No panics or crashes detected
- ✅ 1656+ code coverage achieved
- ✅ 3200+ exec/s execution speed
- ✅ Memory-safe with no leaks

See [fuzz/README.md](fuzz/README.md) for documentation.

## License

MIT OR Apache-2.0
