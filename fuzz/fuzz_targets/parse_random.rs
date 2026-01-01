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

/// Original completely random fuzzer - tests the parser's robustness against invalid/random input
fn do_fuzz_random(data: &[u8]) {
    if data.len() < 20 {
        return;
    }
    // Use the first 16 bytes as the boundary. The API wants `&[u8]`, so this is perfect.
    let boundary = &data[..16].to_vec().into_boxed_slice();
    // The rest of the data is our body.
    let body = &data[16..];

    let rt = Builder::new_current_thread().enable_all().build().unwrap();

    rt.block_on(async {
        // Test with various chunk sizes to find buffering issues
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
    do_fuzz_random(data);
});
