#![no_main]

use bytes::Bytes;
use futures_util::{TryStream, stream};
use libfuzzer_sys::fuzz_target;
use multipart_stream::MultipartStream;
use std::convert::Infallible;
use tokio::runtime::Builder;

fn create_stream_from_chunks(data: &[u8], chunk_size: usize) -> impl TryStream<Ok = Bytes, Error = Infallible> {
    let chunks: Vec<Result<Bytes, Infallible>> =
        data.chunks(chunk_size).map(|chunk| Ok(Bytes::from(chunk.to_vec()))).collect();
    stream::iter(chunks)
}

fn do_fuzz(data: &[u8]) {
    if data.len() < 20 {
        return;
    }
    // Use the first 16 bytes as the boundary. The API wants `&[u8]`, so this is perfect.
    let boundary = &data[..16].to_vec().into_boxed_slice();
    // The rest of the data is our body.
    let body = &data[16..];

    let rt = Builder::new_current_thread().enable_all().build().unwrap();

    rt.block_on(async {
        let s = create_stream_from_chunks(body, 32);
        let mut stream = MultipartStream::new(s, boundary);
        loop {
            if stream.try_next().await.is_err() {
                break;
            }
        }
    });
}

fuzz_target!(|data: &[u8]| {
    do_fuzz(data);
});
