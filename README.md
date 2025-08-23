# Multipart Stream

![alt text](https://img.shields.io/crates/v/multipart_async_stream.svg)


![alt text](https://docs.rs/multipart_async_stream/badge.svg)


![alt text](https://github.com/OpenTritium/multipart_stream/actions/workflows/ci.yaml/badge.svg)

This library is designed as an adapter for `futures_util::TryStream`, allowing for easy parsing of an incoming byte stream (such as from an HTTP response) and splitting it into multiple parts (`Part`). It is especially useful for handling `multipart/byteranges` HTTP responses.

A common use case is sending an HTTP Range request to a server and then parsing the resulting `multipart/byteranges` response body.
The example below demonstrates how to use reqwest to download multiple ranges of a file and parse the individual parts using multipart_stream.

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
b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\xf4\0\0\0B\x08\x06\0\0\0`\xbc\xfb"
{"content-type": "image/png", "content-range": "bytes 64-127/10845"}
b"L:com.adobe.xmp\0\0\0\0\0<?xpacket begin=\"\xef\xbb\xbf\" id=\"W5M0MpCehiHzreSzNT"
```