# Multipart Stream

![alt text](https://img.shields.io/crates/v/multipart_stream.svg)


![alt text](https://docs.rs/multipart_stream/badge.svg)


![alt text](https://github.com/OpenTritium/multipart_stream/actions/workflows/ci.yaml/badge.svg)

This library is designed as an adapter for `futures_util::TryStream`, allowing for easy parsing of an incoming byte stream (such as from an HTTP response) and splitting it into multiple parts (`Part`). It is especially useful for handling `multipart/byteranges` HTTP responses.

A common use case is sending an HTTP Range request to a server and then parsing the resulting `multipart/byteranges` response body.
The example below demonstrates how to use reqwest to download multiple ranges of a file and parse the individual parts using multipart_stream.

```rust
use http::header::CONTENT_TYPE;

#[tokio::main]
async fn main() {
    const URL: &str = "https://mat1.gtimg.com/pingjs/ext2020/newom/build/static/images/new_logo.png";
    let client = reqwest::Client::new();
    let response = client
        .get(URL)
        .header("Range", "bytes=0-32,64-128")
        .send()
        .await.unwrap();
    let boundary = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.contains("multipart/byteranges").then_some(s))
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|s| s.trim().as_bytes().to_vec().into_boxed_slice());
    let s = response.bytes_stream();
    let mut m = multipart_async_stream::MultipartStream::new(s, &boundary.unwrap());
    while let Ok(x) = m.try_next().await {
        println!("Part: {x:?}");
    }
}
```