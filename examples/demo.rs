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

    tokio::spawn(async move {
        while let Some(Ok(part)) = m.next().await {
            println!("{:?}", part.headers());
            let mut body = part.body();
            while let Ok(Some(b)) = body.try_next().await {
                println!("body streaming: {:?}", b);
            }
        }
    })
    .await
    .unwrap();
}
