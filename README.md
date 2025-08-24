# Multipart Stream

![alt text](https://img.shields.io/crates/v/multipart_async_stream.svg) ![alt text](https://docs.rs/multipart_async_stream/badge.svg) ![alt text](https://github.com/OpenTritium/multipart_stream/actions/workflows/ci.yaml/badge.svg)

This library is designed as an adapter for `futures_util::TryStream`, allowing for easy parsing of an incoming byte stream (such as from an HTTP response) and splitting it into multiple parts (`Part`). It is especially useful for handling `multipart/byteranges` HTTP responses.

A common use case is sending an HTTP Range request to a server and then parsing the resulting `multipart/byteranges` response body.
The example below demonstrates how to use reqwest to download multiple ranges of a file and parse the individual parts using `multipart_stream`.

```rust
use multipart_async_stream::{LendingIterator, MultipartStream, TryStreamExt, header::CONTENT_TYPE};

#[tokio::main]
async fn main() {
    const URL: &str = "https://mat1.gtimg.com/pingjs/ext2020/newom/build/static/images/new_logo.png";
    let client = reqwest::Client::new();
    let response = client.get(URL).header("Range", "bytes=0-31,64-127").send().await.unwrap();
    let boundary = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.contains("multipart/byteranges").then_some(s))
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|s| s.trim().as_bytes().to_vec().into_boxed_slice());
    let s = response.bytes_stream();
    let mut m = MultipartStream::new(s, &boundary.unwrap());

    while let Some(Ok(part)) = m.next().await {
        println!("{:?}", part.headers());
        let mut body = part.body();
        while let Ok(Some(b)) = body.try_next().await {
            println!("{:?}", b);
        }
    }
}
```

The output of the program above is:

```bash
{"content-type": "image/png", "content-range": "bytes 0-31/10845"}
body streaming: b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\xf4\0\0\0B\x08\x06\0\0\0`\xbc\xfb"
{"content-type": "image/png", "content-range": "bytes 64-127/10845"}
body streaming: b"L:com.adobe.xmp\0\0\0\0"
body streaming: b"\0<?xpacket begin=\"\xef\xbb\xbf\" id=\"W5M0MpCehiHzreSzNT"
```

## Important Usage Note: Consuming the Part Body

When using this library, a critical point is that you must completely consume the body stream of the current Part before requesting the next one.
If you call `m.next().await` before the previous Part's body has been fully read to its end (i.e., the stream returns `None`), `MultipartStream` will return an `Error::BodyNotConsumed` error.
This is because MultipartStream can only begin parsing the boundary and headers for the next part after the current part's body data stream has ended. An instance of Part internally holds a mutable borrow of the main stream, effectively locking its state. Only when this body stream is fully consumed (and the Part is dropped) can the main stream's state advance.

### ❌ Incorrect Example

The following code will trigger a `BodyNotConsumed` error because it gets the first part but immediately tries to get the next one without consuming the first part's body.

```rust
use multipart_async_stream::{MultipartStream, LendingIterator, Error};
// ... other imports ...

#[tokio::test]
async fn test_body_not_consumed_error_example() {
    const BOUNDARY: &str = "boundary";
    const BODY: &[u8] = b"\
--boundary\r\n\
Content-Disposition: form-data; name=\"field1\"\r\n\
\r\n\
value1\r\n\
--boundary\r\n\
Content-Disposition: form-data; name=\"field2\"\r\n\
\r\n\
value2\r\n\
--boundary--\r\n";

    let stream = create_stream_from_chunks(BODY, BODY.len()); // Assuming create_stream_from_chunks is a test helper
    let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());

    // Get the first part, but we don't process its body
    let _part1 = m.next().await.unwrap().unwrap();

    // Immediately try to get the next part while part1's body is not consumed.
    // In practice, Rust's borrow checker would prevent this code from even compiling,
    // as `_part1` holds an active mutable borrow of `m`. The runtime error
    // shown here acts as a logical safeguard.
    let result = m.next().await;

    // This will fail!
    assert!(matches!(result, Some(Err(Error::BodyNotConsumed))));
    println!("Received expected error: {:?}", result.unwrap().err().unwrap());
}
```

### ✅ Correct Example

The correct approach is to use a loop to ensure the body stream is fully read. The part object is implicitly dropped at the end of the loop's iteration, which releases the lock on the main stream.

```rust
use multipart_async_stream::{MultipartStream, LendingIterator, TryStreamExt};
// ... other imports ...

async fn correct_usage_example() {
    // ... (setup for stream and boundary) ...
    # const BOUNDARY: &str = "boundary";
    # const BODY: &[u8] = b"--boundary\r\n\r\nvalue1\r\n--boundary\r\n\r\nvalue2\r\n--boundary--";
    # let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from_static(BODY))]);
    # let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());

    // Use a while let loop to iterate over all parts
    while let Some(Ok(part)) = m.next().await {
        println!("Headers: {:?}", part.headers());
        
        // Create an inner loop to consume all body chunks of the current part
        let mut body = part.body();
        while let Ok(Some(chunk)) = body.try_next().await {
            // Process the chunk...
            println!("Got a body chunk: {:?}", chunk);
        }
        // When this inner loop finishes, the body stream is exhausted.
        // The `part` will be dropped at the end of this main loop's iteration.
        // Now it's safe to proceed to the next call of `m.next().await`.
    }
}
```