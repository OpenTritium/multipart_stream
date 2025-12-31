#![cfg_attr(not(test), warn(clippy::nursery, clippy::unwrap_used, clippy::todo, clippy::dbg_macro,))]
#![allow(clippy::future_not_send)]
pub use async_iterator::LendingIterator;
use bytes::{Buf, Bytes, BytesMut};
use constcat::concat_bytes;
pub use futures_util::{TryStream, TryStreamExt};
pub use http::header;
use http::{HeaderMap, HeaderName, HeaderValue};
use httparse::{EMPTY_HEADER, Status, parse_headers};
use memchr::memmem::Finder;
use std::{
    error::Error as StdError,
    mem,
    ops::Not,
    pin::Pin,
    slice,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("the stream has been terminated before the end of the part")]
    EarlyTerminate,
    #[error("stream error: {0}")]
    StreamError(#[from] Box<dyn StdError + Send + Sync>),
    #[error("parse error: {0}")]
    ParseError(#[from] ParseError),
    #[error("body stream is not consumed")]
    BodyNotConsumed,
}

/// Represents the current state of the parser
#[derive(Debug)]
enum ParserState {
    Preamble(usize),       // Found the boundary, move buffer pointer to header initial position
    ReadingHeaders(usize), // Currently reading header content
    StreamingBody(usize),  /* Move the last window's content, determine again after splicing the header next time,
                            * but this still requires copying */
    Finished,
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error(transparent)]
    Other(#[from] httparse::Error),
    #[error("buffer no change")]
    BufferNoChange,
    #[error("incomplete headers content")]
    TryParsePartial,
}

// Unpin
const CRLF: &[u8] = b"\r\n";
const DOUBLE_HYPEN: &[u8] = b"--";
pub struct MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    rx: S,
    terminated: bool,
    state: ParserState,
    pattern: Box<[u8]>,                       // Owns the pattern data
    boundary_finder: Finder<'static>,         // Finder for pattern without last 2 bytes
    boundary_finder_no_crlf: Finder<'static>, // Finder for pattern without starting \r\n
    header_body_splitter_finder: Finder<'static>,
    header_body_splitter_len: usize,
    buf: BytesMut,
}

impl<S> MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    pub fn new(stream: S, boundary: &[u8]) -> Self {
        let pre_alloc_size = boundary.len() + 2 * CRLF.len() + 2 * DOUBLE_HYPEN.len();
        let mut pattern = Vec::with_capacity(pre_alloc_size);
        pattern.extend_from_slice(CRLF);
        pattern.extend_from_slice(DOUBLE_HYPEN);
        pattern.extend_from_slice(boundary);
        pattern.extend_from_slice(CRLF);
        const HEADER_BODY_SPLITTER: &[u8] = concat_bytes!(CRLF, CRLF);
        let pattern = pattern.into_boxed_slice();
        let pattern_ptr = pattern.as_ptr();
        let boundary_finder = Finder::new(unsafe { slice::from_raw_parts(pattern_ptr, pattern.len() - 2) });
        let boundary_finder_no_crlf = Finder::new(unsafe {
            let p = pattern_ptr.add(CRLF.len());
            slice::from_raw_parts(p, pattern.len() - CRLF.len())
        });
        Self {
            rx: stream,
            terminated: false,
            state: ParserState::Preamble(0),
            buf: BytesMut::new(),
            pattern,
            boundary_finder,
            boundary_finder_no_crlf,
            header_body_splitter_finder: Finder::new(HEADER_BODY_SPLITTER),
            header_body_splitter_len: HEADER_BODY_SPLITTER.len(),
        }
    }

    #[inline]
    fn update_scan(&mut self, new_scan: usize) {
        use ParserState::*;
        match &mut self.state {
            Preamble(scan) | ReadingHeaders(scan) | StreamingBody(scan) => *scan = new_scan,
            Finished => unreachable!("cannot invoke add_scan on finished state"),
        }
    }

    // Always return none when not in StreamingBody state
    fn poll_next_body_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>> {
        use ParserState::*;
        use Poll::*;
        let pattern_len = self.pattern.len();
        let sub_pattern_len = pattern_len - 2; // check tail soon
        loop {
            let prev_buf_len = self.buf.len();
            let scan = match self.state {
                Preamble(_) | ReadingHeaders(_) | Finished => return Ready(None),
                StreamingBody(scan) => scan,
            };
            if prev_buf_len >= pattern_len + scan {
                // \r\n--boundary (\r\n | --)
                // Find the pattern without the last 2 bytes, then check what follows
                if let Some(pos) = self.boundary_finder.find(&self.buf[scan..]) {
                    let pattern_start = scan + pos;
                    let pattern_tail = {
                        let pos = pattern_start + sub_pattern_len;
                        self.buf.get(pos..pos + 2) // 2 means `\r\n` or `--`
                    };
                    match pattern_tail {
                        Some(CRLF) => {
                            // multipart stream has not ended, start parsing the next part's headers,
                            // calling this function immediately will only return none
                            self.state = Preamble(0); // Don't clear boundary immediately, help preamble positioning
                            let chunk = self.buf.split_to(pattern_start).freeze();
                            return Ready(Some(Ok(chunk)));
                        }
                        Some(DOUBLE_HYPEN) => {
                            // multipart stream has ended, which also means there will be no more body stream
                            // calling this function next time will only return none
                            self.state = Finished;
                            let chunk = self.buf.split_to(pattern_start).freeze();
                            self.buf.clear(); // Skip `-- boundary --`
                            return Ready(Some(Ok(chunk)));
                        }
                        Some(_) => {
                            // Content in body that exactly matches the pattern
                            let last_window_end = self.buf.len() - sub_pattern_len;
                            let chunk = self.buf.split_to(last_window_end).freeze();
                            self.update_scan(0);
                            return Ready(Some(Ok(chunk)));
                        }
                        // Continue receiving to determine the next two bytes
                        None => {}
                    }
                } else {
                    // Return preceding bytes (because boundary cannot be matched at all), keep window -1 bytes, then
                    // update scan to 0
                    let last_window_end = self.buf.len() - sub_pattern_len + 1;
                    let chunk = self.buf.split_to(last_window_end).freeze();
                    self.update_scan(0);
                    return Ready(Some(Ok(chunk)));
                }
            }

            // In streaming body state, return EarlyTerminate if terminated
            if self.terminated && self.buf.len() == prev_buf_len {
                return Ready(Some(Err(Error::EarlyTerminate)));
            }
            return match self.rx.try_poll_next_unpin(cx) {
                Ready(Some(Ok(chunk))) => {
                    self.buf.extend_from_slice(&chunk);
                    continue;
                }
                Ready(Some(Err(err))) => Ready(Some(Err(Error::StreamError(Box::new(err))))),
                Ready(None) => {
                    self.terminated = true;
                    continue;
                }
                Pending => Pending,
            };
        }
    }

    fn poll_next_part(&'_ mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Part<'_, S>, Error>>> {
        loop {
            use ParserState::*;
            use Poll::*;
            let prev_buf_len = self.buf.len();
            let pattern_no_crlf_len = self.pattern.len() - 2;
            match self.state {
                // --boundary\r\n
                Preamble(scan) if prev_buf_len >= pattern_no_crlf_len + scan => {
                    if let Some(pos) = self.boundary_finder_no_crlf.find(&self.buf[scan..]) {
                        let total_advance_len = scan + pos + pattern_no_crlf_len;
                        // If advance length is greater than current buffer length, continue receiving
                        if self.buf.len() >= total_advance_len {
                            self.buf.advance(total_advance_len);
                            self.state = ReadingHeaders(0);
                        }
                    } else {
                        // Scan only proceeds to the last window that satisfies the window size, so specify scan to
                        // after the position of the last satisfying window
                        let new_scan = prev_buf_len - pattern_no_crlf_len + 1;
                        if new_scan == scan {
                            return Ready(Some(Err(ParseError::BufferNoChange.into())));
                        }
                        self.update_scan(new_scan);
                    }
                }
                // CRLFCRLF
                ReadingHeaders(scan) if prev_buf_len >= self.header_body_splitter_len + scan => {
                    if let Some(pos) = self.header_body_splitter_finder.find(&self.buf[scan..]) {
                        let hdrs_end = scan + pos + self.header_body_splitter_len;
                        let hdrs_contnet = &self.buf[..hdrs_end]; // Include both CRLFs in parsing
                        let mut hdrs_buf = [EMPTY_HEADER; 64];
                        match parse_headers(hdrs_contnet, &mut hdrs_buf) {
                            Ok(Status::Complete(_)) => {}
                            Ok(Status::Partial) => return Ready(Some(Err(ParseError::TryParsePartial.into()))),
                            Err(err) => return Ready(Some(Err(ParseError::Other(err).into()))),
                        }
                        let headers = hdrs_buf
                            .iter()
                            .take_while(|hdr| hdr.name.is_empty().not())
                            .filter_map(|hdr| {
                                let name = HeaderName::from_str(hdr.name);
                                let value = HeaderValue::from_bytes(hdr.value);
                                name.ok().zip(value.ok())
                            })
                            .collect::<HeaderMap>();
                        self.buf.advance(hdrs_end);
                        self.state = StreamingBody(0);
                        return Ready(Some(Ok(Part::new(self, headers.into()))));
                    } else {
                        // Specify new scan position, still just after the last window
                        let new_scan = self.buf.len() - self.header_body_splitter_len + 1;
                        if new_scan == scan {
                            return Ready(Some(Err(ParseError::BufferNoChange.into())));
                        }
                        self.update_scan(new_scan);
                    };
                }
                Finished => return Ready(None),
                StreamingBody(_) => return Ready(Some(const { Err(Error::BodyNotConsumed) })),
                _ => {}
            }
            if self.terminated && self.buf.len() == prev_buf_len {
                return Ready(Some(Err(Error::EarlyTerminate)));
            }
            return match self.rx.try_poll_next_unpin(cx) {
                Ready(Some(Ok(chunk))) => {
                    self.buf.extend_from_slice(&chunk);
                    continue;
                }
                Ready(Some(Err(err))) => Ready(Some(Err(Error::StreamError(Box::new(err))))),
                Ready(None) => {
                    self.terminated = true;
                    continue;
                }
                Pending => Pending,
            };
        }
    }
}

impl<S> LendingIterator for MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    type Item<'a>
        = Result<Part<'a, S>, Error>
    where
        S: 'a;

    #[inline]
    fn next(&mut self) -> impl futures_util::Future<Output = Option<<Self as LendingIterator>::Item<'_>>> {
        NextFuture { stream: self }
    }
}

pub struct NextFuture<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    stream: &'a mut MultipartStream<S>,
}

impl<'a, S> Future for NextFuture<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    type Output = Option<Result<Part<'a, S>, Error>>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let stream: &mut MultipartStream<S> = self.get_mut().stream;
        // SAFETY: The reference `stream` is derived from `self.get_mut()` and is guaranteed to be valid
        // for the lifetime of `self`. We are creating a Pin from a mutable reference which is safe
        // because the data is not moved and the pointer remains valid.
        let mut stream_pin = unsafe { Pin::new_unchecked(stream) };
        // SAFETY: This transmute extends the lifetime of the returned Poll value.
        // This is safe because:
        // 1. The returned Part<'a, S> contains a reference with lifetime 'a that is bound to NextFuture<'a, S>
        // 2. The Part only holds a reference to the stream, which lives for at least 'a
        // 3. We're not actually extending any lifetimes unsafely - the lending iterator pattern requires the returned
        //    value to be tied to the future's lifetime
        // 4. The actual data (MultipartStream) outlives the future, so the reference is valid
        unsafe { mem::transmute(stream_pin.poll_next_part(cx)) }
    }
}

pub struct Part<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    body: &'a mut MultipartStream<S>,
    headers: Box<HeaderMap>,
}

impl<'a, S> Part<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    const fn new(stream: &'a mut MultipartStream<S>, headers: Box<HeaderMap>) -> Self { Self { body: stream, headers } }

    pub fn headers(&self) -> &HeaderMap { &self.headers }

    pub fn into_headers(self) -> HeaderMap { *self.headers }

    pub fn body(self) -> impl TryStream<Ok = Bytes, Error = Error> + 'a {
        futures_util::stream::poll_fn(move |cx| self.body.poll_next_body_chunk(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::convert::Infallible;

    // A helper function to convert a byte array slice into a Bytes stream
    // This simulates data arriving in chunks
    fn create_stream_from_chunks(data: &[u8], chunk_size: usize) -> impl TryStream<Ok = Bytes, Error = Infallible> {
        let chunks: Vec<Result<Bytes, Infallible>> =
            data.chunks(chunk_size).map(|chunk| Ok(Bytes::from(chunk.to_vec()))).collect();
        stream::iter(chunks)
    }

    async fn concat_body(s: impl TryStream<Ok = Bytes, Error = Error>) -> Vec<u8> {
        s.try_fold(vec![], |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_single_part_full_chunk() {
        const BOUNDARY: &str = "boundary";
        const CONTENT: &[u8] = b"\
--boundary\r\n\
Content-Disposition: form-data; name=\"field1\"\r\n\
\r\n\
value1\r\n\
--boundary--\r\n";
        let stream = create_stream_from_chunks(CONTENT, CONTENT.len());
        let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());
        while let Some(Ok(part)) = m.next().await {
            assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
            assert_eq!(&concat_body(part.body()).await, b"value1")
        }
    }

    #[tokio::test]
    async fn test_multiple_parts_small_chunks() {
        const BOUNDARY: &str = "X-BOUNDARY";
        const BODY: &[u8] = b"\
    --X-BOUNDARY\r\n\
    Content-Disposition: form-data; name=\"field1\"\r\n\
    \r\n\
    value1\r\n\
    --X-BOUNDARY\r\n\
    Content-Disposition: form-data; name=\"field2\"\r\n\
    Content-Type: text/plain\r\n\
    \r\n\
    value2 with CRLF\r\n\r\n\
    --X-BOUNDARY--\r\n";

        // Use a very small chunk size to force test buffer logic
        let stream = create_stream_from_chunks(BODY, 5);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // Parse the first part
        let part1 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert!(!part1.headers().contains_key("content-type"));
        assert_eq!(&concat_body(part1.body()).await, b"value1");

        // Parse the second part
        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(part2.headers().get("content-type").unwrap(), "text/plain");
        let body = concat_body(part2.body()).await;
        assert_eq!(&body, b"value2 with CRLF\r\n");
        // Should have reached the end of the stream
        let result = multipart_stream.next().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_with_preamble_and_no_final_crlf() {
        const BOUNDARY: &str = "boundary";
        const BODY: &[u8] = b"\
    This is a preamble and should be ignored.\r\n\
    --boundary\r\n\
    Content-Disposition: form-data; name=\"field1\"\r\n\
    \r\n\
    value1\r\n\
    --boundary--"; // 注意：末尾没有 `\r\n`

        let stream = create_stream_from_chunks(BODY, BODY.len());
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 解析第一个部分
        let part = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        let body = concat_body(part.body()).await;
        assert_eq!(&body, b"value1");
        // Should have reached the end of the stream
        let result = multipart_stream.next().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    #[should_panic(expected = "EarlyTerminate")]
    async fn test_early_terminate_in_body() {
        const BOUNDARY: &str = "boundary";
        // 消息在 body 中被截断，没有结束边界
        const BODY: &[u8] = b"\
    --boundary\r\n\
    Content-Disposition: form-data; name=\"field1\"\r\n\
    \r\n\
    value1 is not complete";

        let stream = create_stream_from_chunks(BODY, BODY.len());
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 解析应该会失败，因为流在找到下一个边界前就终止了
        let part = multipart_stream.next().await.unwrap().unwrap();

        let _ = concat_body(part.body()).await;
    }

    #[tokio::test]
    async fn test_early_terminate_in_headers() {
        const BOUNDARY: &str = "boundary";
        // 消息在 headers 中被截断
        const BODY: &[u8] = b"\
    --boundary\r\n\
    Content-Disposition: form-data; na";

        let stream = create_stream_from_chunks(BODY, BODY.len());
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 解析应该会失败，因为流在 headers 结束前就终止了
        let result = multipart_stream.next().await;
        assert!(matches!(result, Some(Err(Error::EarlyTerminate))));
    }

    #[tokio::test]
    async fn test_empty_stream() {
        const BOUNDARY: &str = "boundary";
        const BODY: &[u8] = b"";

        let stream = create_stream_from_chunks(BODY, 10);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 对于空流，应该提前终止
        let result = multipart_stream.next().await;
        assert!(matches!(result, Some(Err(Error::EarlyTerminate))));
    }

    #[tokio::test]
    async fn test_part_with_empty_body() {
        const BOUNDARY: &str = "boundary";
        const BODY: &[u8] = b"\
    --boundary\r\n\
    Content-Disposition: form-data; name=\"field1\"\r\n\
    \r\n\
    value1\r\n\
    --boundary\r\n\
    Content-Disposition: form-data; name=\"empty_field\"\r\n\
    \r\n\
    \r\n\
    --boundary\r\n\
    Content-Disposition: form-data; name=\"field2\"\r\n\
    \r\n\
    value2\r\n\
    --boundary--\r\n";

        let stream = create_stream_from_chunks(BODY, 15); // Use small chunks
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());
        let part1 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert_eq!(&concat_body(part1.body()).await, b"value1");

        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"empty_field\"");
        let body = concat_body(part2.body()).await;
        assert!(body.is_empty());

        let part3 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part3.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(&concat_body(part3.body()).await, b"value2");

        let result = multipart_stream.next().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_body_not_consumed_error() {
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

        let stream = create_stream_from_chunks(BODY, BODY.len());
        let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // Get the first part, but don't consume its body
        let _part1 = m.next().await.unwrap().unwrap();

        // Trying to get the next part immediately should fail
        let result = m.next().await;
        assert!(matches!(result, Some(Err(Error::BodyNotConsumed))));
    }

    #[tokio::test]
    async fn test_boundary_like_string_in_body() {
        const BOUNDARY: &str = "boundary";
        const BODY: &[u8] = b"\
--boundary\r\n\
Content-Disposition: form-data; name=\"field1\"\r\n\
\r\n\
value1 contains --boundary text\r\n\
--boundary--\r\n";

        let stream = create_stream_from_chunks(BODY, 20);
        let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let body = concat_body(part.body()).await;
        assert_eq!(&body, b"value1 contains --boundary text");

        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_malformed_headers() {
        const BOUNDARY: &str = "boundary";
        // "Invalid Header" contains a space, which is illegal for a header name.
        const BODY: &[u8] = b"\
--boundary\r\n\
Invalid Header: value\r\n\
\r\n\
body\r\n\
--boundary--\r\n";

        let stream = create_stream_from_chunks(BODY, BODY.len());
        let mut m = MultipartStream::new(stream, BOUNDARY.as_bytes());

        let result = m.next().await.unwrap();
        assert!(matches!(result, Err(Error::ParseError(_))));
        if let Err(Error::ParseError(ParseError::Other(e))) = result {
            assert_eq!(e, httparse::Error::HeaderName);
        } else {
            panic!("Expected a ParseError::Other with InvalidHeaderName");
        }
    }
    #[tokio::test]
    async fn test_streaming_body() {
        const BOUNDARY: &str = "boundary";
        const PART1_BODY: &[u8] = b"This is the first part's body, which is quite long to demonstrate streaming.";
        const PART2_BODY: &[u8] = b"This is the second part, which is also streamed.";
        // 使用 format! 来构建 body，避免手动拼接字符串可能引入的错误
        let body_content = format!(
            "--{boundary}\r\nContent-Disposition: form-data; \
             name=\"field1\"\r\n\r\n{part1_body}\r\n--{boundary}\r\nContent-Disposition: form-data; \
             name=\"field2\"\r\n\r\n{part2_body}\r\n--{boundary}--\r\n",
            boundary = BOUNDARY,
            part1_body = std::str::from_utf8(PART1_BODY).unwrap(),
            part2_body = std::str::from_utf8(PART2_BODY).unwrap(),
        );
        let body_bytes = body_content.as_bytes();
        let chunk_size = 10;
        // Use a very small chunk size to ensure the body is processed in a streaming manner
        let stream = create_stream_from_chunks(body_bytes, chunk_size);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // --- Process the first part ---
        let part1 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");

        // Manually read data from the body stream chunk by chunk
        let mut body_stream1 = part1.body();
        let mut collected_body1 = Vec::new();
        let mut i = 0;
        while let Some(chunk_result) = body_stream1.try_next().await.unwrap() {
            i += 1;
            // Assert that we indeed received non-empty chunks
            assert!(!chunk_result.is_empty());
            collected_body1.extend_from_slice(&chunk_result);
        }
        drop(body_stream1);
        assert_eq!(i, PART1_BODY.len().div_ceil(chunk_size));
        // Verify that the complete received body content is correct
        assert_eq!(collected_body1, PART1_BODY);
        i = 0;
        // --- Process the second part ---
        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");

        let mut body_stream2 = part2.body();
        let mut collected_body2 = Vec::new();
        while let Some(chunk_result) = body_stream2.try_next().await.unwrap() {
            i += 1;
            assert!(!chunk_result.is_empty());
            collected_body2.extend_from_slice(&chunk_result);
        }
        assert_eq!(collected_body2, PART2_BODY);
        drop(body_stream2);
        assert_eq!(i, PART2_BODY.len().div_ceil(chunk_size));

        // --- Confirm the stream has ended ---
        assert!(multipart_stream.next().await.is_none());
    }
}
