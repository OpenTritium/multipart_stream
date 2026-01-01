#![cfg_attr(not(test), warn(clippy::nursery, clippy::unwrap_used, clippy::todo, clippy::dbg_macro,))]
#![allow(clippy::future_not_send)]

//! A streaming multipart/form-data parser for Rust.
//!
//! This crate provides a zero-copy (or minimal-copy) streaming parser for multipart data,
//! optimized for performance and memory efficiency. It uses the lending iterator pattern
//! to yield parts as they arrive, without buffering the entire input.
//!
//! # Examples
//!
//! ```rust,no_run
//! use multipart_async_stream::{MultipartStream, LendingIterator};
//! use bytes::Bytes;
//! use futures_util::stream;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let data = b"--boundary\r\n\
//!     Content-Disposition: form-data; name=\"field1\"\r\n\
//!     \r\n\
//!     value1\r\n\
//!     --boundary--\r\n";
//!
//! let stream = stream::iter(vec![Result::<Bytes, std::convert::Infallible>::Ok(Bytes::from(&data[..]))]);
//! let mut multipart = MultipartStream::new(stream, b"boundary");
//!
//! while let Some(Ok(part)) = multipart.next().await {
//!     let _headers = part.headers();
//!     // Consume part.body() stream...
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Safety Note
//!
//! The returned [`Part`] holds a mutable reference to the underlying [`MultipartStream`].
//! You must fully consume the part's body stream before calling [`next()`](LendingIterator::next)
//! again. Failing to do so will result in a [`BodyNotConsumed`](Error::BodyNotConsumed) error.

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
    marker::PhantomPinned,
    mem,
    ops::Not,
    pin::Pin,
    slice,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;

/// Errors that can occur during multipart parsing.
#[derive(Debug, Error)]
pub enum Error {
    /// The stream terminated before reaching the end of the current part.
    ///
    /// This occurs when the underlying stream ends unexpectedly
    /// without encountering a boundary marker.
    #[error("the stream has been terminated before the end of the part")]
    EarlyTerminate,

    /// An error occurred while reading from the underlying stream.
    #[error("stream error: {0}")]
    StreamError(#[from] Box<dyn StdError + Send + Sync>),

    /// An error occurred during parsing of multipart data.
    #[error("parse error: {0}")]
    ParseError(#[from] ParseError),

    /// Attempted to fetch the next part without consuming the previous part's body.
    ///
    /// Each part's body stream must be fully consumed before requesting the next part.
    #[error("body stream is not consumed")]
    BodyNotConsumed,
}

/// Internal parser state machine.
///
/// # Variants
///
/// * `Preamble` - Scanning for the initial boundary marker
/// * `ReadingHeaders` - Parsing part headers
/// * `StreamingBody` - Streaming part body data
/// * `Finished` - Multipart stream has been fully consumed
#[derive(Debug)]
enum ParserState {
    Preamble(usize),
    ReadingHeaders(usize),
    StreamingBody(usize),
    Finished,
}

/// Errors that can occur during multipart header parsing.
#[derive(Error, Debug)]
pub enum ParseError {
    /// An error from the underlying `httparse` crate.
    #[error(transparent)]
    Other(#[from] httparse::Error),

    /// Buffer made no progress during parsing.
    ///
    /// This typically indicates malformed input where the parser
    /// cannot make forward progress.
    #[error("buffer no change")]
    BufferNoChange,

    /// Header data is incomplete.
    ///
    /// More data is needed to complete header parsing.
    #[error("incomplete headers content")]
    TryParsePartial,
}

const CRLF: &[u8] = b"\r\n";
const DOUBLE_HYPHEN: &[u8] = b"--";

/// A streaming multipart/form-data parser.
///
/// This type implements the lending iterator pattern, yielding [`Part`] values
/// as they are parsed from the input stream. It provides zero-copy (or minimal-copy)
/// parsing by leveraging memchr for efficient pattern matching.
///
/// # Type Parameters
///
/// * `S` - A stream yielding `Bytes` chunks
///
/// # Examples
///
/// ```rust,no_run
/// use multipart_async_stream::{MultipartStream, LendingIterator};
/// use bytes::Bytes;
/// use futures_util::stream;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let stream = stream::iter(vec![Result::<Bytes, std::convert::Infallible>::Ok(Bytes::from("data"))]);
/// let mut multipart = MultipartStream::new(stream, b"boundary");
///
/// while let Some(Ok(part)) = multipart.next().await {
///     let _headers = part.headers();
/// }
/// # Ok(())
/// # }
/// ```
pub struct MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    rx: S,
    terminated: bool,
    state: ParserState,
    pattern: Box<[u8]>,
    boundary_finder: Finder<'static>,
    boundary_finder_no_crlf: Finder<'static>,
    header_body_splitter_finder: Finder<'static>,
    header_body_splitter_len: usize,
    buf: BytesMut,
    _pin: PhantomPinned,
}

impl<S> MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    /// Creates a new multipart parser from the given stream and boundary.
    ///
    /// # Arguments
    ///
    /// * `stream` - A stream yielding `Bytes` chunks containing the multipart data
    /// * `boundary` - The multipart boundary string (without leading `--`)
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use multipart_async_stream::MultipartStream;
    /// use bytes::Bytes;
    /// use futures_util::stream;
    ///
    /// let stream = stream::iter(vec![Result::<Bytes, std::convert::Infallible>::Ok(Bytes::from("data"))]);
    /// let multipart = MultipartStream::new(stream, b"my-boundary");
    /// ```
    pub fn new(stream: S, boundary: &[u8]) -> Self {
        let pre_alloc_size = boundary.len() + 2 * CRLF.len() + 2 * DOUBLE_HYPHEN.len();
        let mut pattern = Vec::with_capacity(pre_alloc_size);
        pattern.extend_from_slice(CRLF);
        pattern.extend_from_slice(DOUBLE_HYPHEN);
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
            _pin: PhantomPinned,
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
                        Some(DOUBLE_HYPHEN) => {
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
                        let hdrs_content = &self.buf[..hdrs_end]; // Include both CRLFs in parsing
                        let mut hdrs_buf = [EMPTY_HEADER; 64];
                        match parse_headers(hdrs_content, &mut hdrs_buf) {
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

/// Future for the next part in the multipart stream.
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

        // # Safety
        //
        // This transmute extends the lifetime of the returned `Poll` value from
        // the anonymous lifetime of `poll_next_part` to `'a`.
        //
        // ## Why this is safe in practice:
        //
        // 1. The returned `Part<'a, S>` holds a reference with lifetime `'a`, which is bound to `NextFuture<'a, S>`.
        // 2. `NextFuture` holds a mutable reference to the stream for lifetime `'a`.
        // 3. Users must consume the `Part` (and thus its body) before the next call to `next()`, enforced at runtime by
        //    the parser state machine.
        //
        // ## Important invariant:
        //
        // Callers MUST fully consume the previous part's body before calling
        // `next()` again. Violating this will result in a runtime error
        // (`BodyNotConsumed`), preventing use-after-free scenarios.
        //
        // This pattern is known as a "lending iterator" and is currently
        // undergoing standardization in Rust. See:
        // https://rust-lang.github.io/rfcs/2956-lending-iterator.html
        unsafe { mem::transmute(stream.poll_next_part(cx)) }
    }
}

/// A single part in a multipart stream.
///
/// Contains parsed headers and provides access to the body stream.
///
/// # Important
///
/// The body stream must be fully consumed before the parent `MultipartStream`
/// can advance to the next part. Failure to consume the body will result in
/// a `BodyNotConsumed` error on the next call to `next()`.
///
/// # Lifetime
///
/// The lifetime `'a` is tied to the `MultipartStream` from which this part was yielded.
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

    /// Returns a reference to the headers of this part.
    pub fn headers(&self) -> &HeaderMap { &self.headers }

    /// Consumes this part and returns the headers.
    ///
    /// This is useful when you want to take ownership of the headers
    /// and no longer need the body.
    pub fn into_headers(self) -> HeaderMap { *self.headers }

    /// Returns a stream for the body of this part.
    ///
    /// The returned stream yields chunks of `Bytes` as they arrive from the
    /// underlying multipart stream.
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

    async fn concat_body(s: impl TryStream<Ok = Bytes, Error = Error>) -> Result<Vec<u8>, Error> {
        s.try_fold(vec![], |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
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
            assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1")
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
        assert_eq!(&concat_body(part1.body()).await.unwrap(), b"value1");

        // Parse the second part
        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(part2.headers().get("content-type").unwrap(), "text/plain");
        let body = concat_body(part2.body()).await.unwrap();
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
        let body = concat_body(part.body()).await.unwrap();
        assert_eq!(&body, b"value1");
        // Should have reached the end of the stream
        let result = multipart_stream.next().await;
        assert!(result.is_none());
    }

    #[tokio::test]
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

        let result = concat_body(part.body()).await;
        assert!(matches!(result, Err(Error::EarlyTerminate)));
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
        assert_eq!(&concat_body(part1.body()).await.unwrap(), b"value1");

        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"empty_field\"");
        let body = concat_body(part2.body()).await.unwrap();
        assert!(body.is_empty());

        let part3 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part3.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(&concat_body(part3.body()).await.unwrap(), b"value2");

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
        let body = concat_body(part.body()).await.unwrap();
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
        drop(body_stream2);
        assert_eq!(collected_body2, PART2_BODY);
        assert_eq!(i, PART2_BODY.len().div_ceil(chunk_size));

        // --- Confirm the stream has ended ---
        assert!(multipart_stream.next().await.is_none());
    }

    // ========== Boundary Value Tests ==========

    #[tokio::test]
    async fn test_boundary_max_length_70_bytes() {
        // RFC 2046: boundary must not exceed 70 characters
        let boundary = "a".repeat(70);
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue1\r\n--{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_boundary_min_length_1_byte() {
        // Minimum practical boundary length
        let boundary = "a";
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue1\r\n--{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 5);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_boundary_with_special_chars() {
        // Test boundary with allowed special characters
        let boundary = "this_-boundary_123_ABC";
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue1\r\n--{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_very_long_header_value() {
        // Test with very long header value (common in real-world scenarios)
        let boundary = "boundary";
        let long_value = "a".repeat(8192); // 8KB header value
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"field1\"\r\nX-Long-Header: {}\r\n\r\nvalue1\r\n--{}--\r\n",
            boundary, long_value, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 100);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("x-long-header").unwrap().len(), 8192);
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_many_small_parts() {
        // Test with many small parts (stress test the parser)
        let boundary = "boundary";
        let num_parts = 100;
        let mut body = String::new();

        for i in 0..num_parts {
            body.push_str(&format!(
                "--{}\r\nContent-Disposition: form-data; name=\"field{}\"\r\n\r\nvalue{}\r\n",
                boundary, i, i
            ));
        }
        body.push_str(&format!("--{}--\r\n", boundary));

        let stream = create_stream_from_chunks(body.as_bytes(), 50);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        for i in 0..num_parts {
            let part = m.next().await.unwrap().unwrap();
            let expected_name = format!("form-data; name=\"field{}\"", i);
            assert_eq!(part.headers().get("content-disposition").unwrap().to_str().unwrap(), expected_name);
            let expected_value = format!("value{}", i);
            assert_eq!(&concat_body(part.body()).await.unwrap(), expected_value.as_bytes());
        }
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_single_very_large_body() {
        // Test with a single very large body (memory efficiency test)
        let boundary = "boundary";
        let large_body = "x".repeat(1_000_000); // 1MB body
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n{}\r\n--{}--\r\n",
            boundary, large_body, boundary
        );

        // Use small chunks to ensure streaming
        let stream = create_stream_from_chunks(body.as_bytes(), 8192);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let received_body = concat_body(part.body()).await.unwrap();
        assert_eq!(received_body.len(), 1_000_000);
        assert_eq!(received_body, large_body.as_bytes());
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_empty_part_names() {
        // Test edge case: empty field name
        let boundary = "boundary";
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"\"\r\n\r\nvalue1\r\n--{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"\"");
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_part_with_no_headers() {
        // Test edge case: part with minimal headers
        let boundary = "boundary";
        let body = format!(
            "--{}\r\nContent-Disposition: form-data\r\n\r\nbody value\r\n--{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert!(part.headers().get("content-disposition").is_some());
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"body value");
        assert!(m.next().await.is_none());
    }

    // ========== Performance and Stress Tests ==========

    #[tokio::test]
    async fn test_chunk_size_variations() {
        // Test all chunk sizes from 1 to 512 bytes
        let boundary = "boundary";
        let body = b"\
            --boundary\r\n\
            Content-Disposition: form-data; name=\"field1\"\r\n\
            \r\n\
            value1\r\n\
            --boundary--\r\n";

        for chunk_size in [1, 2, 3, 4, 5, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256, 511, 512] {
            let stream = create_stream_from_chunks(body, chunk_size);
            let mut m = MultipartStream::new(stream, boundary.as_bytes());

            let part = m.next().await.unwrap().unwrap();
            assert_eq!(&concat_body(part.body()).await.unwrap(), b"value1");
            assert!(m.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn test_memory_efficiency_large_file() {
        // This test verifies that we don't buffer the entire input in memory
        // The parser should only keep a small window in memory regardless of input size
        let boundary = "boundary";
        let large_body = "x".repeat(10_000_000); // 10MB body
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n{}\r\n--{}--\r\n",
            boundary, large_body, boundary
        );

        // Use very small chunks to maximize stress on buffering logic
        let stream = create_stream_from_chunks(body.as_bytes(), 100);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();

        // Stream the body chunk by chunk and count chunks
        let mut body_stream = part.body();
        let mut chunk_count = 0;
        let mut total_bytes = 0;

        while let Some(chunk) = body_stream.try_next().await.unwrap() {
            chunk_count += 1;
            total_bytes += chunk.len();
            // Each chunk should be relatively small
            assert!(chunk.len() <= 10000, "Chunk too large: {}", chunk.len());
        }

        drop(body_stream); // Explicitly drop to release borrow

        assert_eq!(total_bytes, 10_000_000);
        assert!(chunk_count > 100, "Should have received multiple chunks, got {}", chunk_count);
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_concurrent_streams() {
        // Test multiple MultipartStream instances running concurrently
        let boundary = "boundary";

        async fn parse_stream(boundary: &str, idx: usize) {
            let body = format!(
                "--{}\r\nContent-Disposition: form-data; name=\"field{}\"\r\n\r\nvalue{}\r\n--{}--\r\n",
                boundary, idx, idx, boundary
            );

            let stream = create_stream_from_chunks(body.as_bytes(), 10);
            let mut m = MultipartStream::new(stream, boundary.as_bytes());

            let part = m.next().await.unwrap().unwrap();
            let expected = format!("value{}", idx);
            assert_eq!(&concat_body(part.body()).await.unwrap(), expected.as_bytes());
            assert!(m.next().await.is_none());
        }

        // Run 10 concurrent streams
        let handles: Vec<_> = (0..10).map(|i| {
            tokio::spawn(parse_stream(boundary, i))
        }).collect();

        for handle in handles {
            handle.await.unwrap();
        }
    }

    // ========== Protocol Compatibility Tests ==========

    #[tokio::test]
    async fn test_file_upload_with_filename() {
        // Test real-world file upload scenario with filename
        let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
        let body = format!(
            "--{}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             This is file content\r\n\
             --{}--\r\n",
            boundary, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 50);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let content_disp = part.headers().get("content-disposition").unwrap().to_str().unwrap();
        assert!(content_disp.contains("filename=\"test.txt\""));
        assert_eq!(part.headers().get("content-type").unwrap(), "text/plain");
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"This is file content");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_unicode_filename_rfc2231() {
        // Test RFC 2231 encoded filename (UTF-8)
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"file\"; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             content\r\n\
             --boundary--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 30);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert!(part.headers().get("content-disposition").is_some());
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"content");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_multiple_content_disposition_parameters() {
        // Test Content-Disposition with multiple parameters
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"; filename=\"file.txt\"; size=\"1024\"\r\n\
             \r\n\
             value\r\n\
             --boundary--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 30);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let content_disp = part.headers().get("content-disposition").unwrap().to_str().unwrap();
        assert!(content_disp.contains("name=\"field\""));
        assert!(content_disp.contains("filename=\"file.txt\""));
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_various_content_types() {
        // Test various common content types
        let boundary = "boundary";

        for content_type in [
            "text/plain",
            "application/json",
            "application/octet-stream",
            "image/jpeg",
            "application/pdf",
        ] {
            let body = format!(
                "--{}\r\n\
                 Content-Disposition: form-data; name=\"file\"\r\n\
                 Content-Type: {}\r\n\
                 \r\n\
                 dummy content\r\n\
                 --{}--\r\n",
                boundary, content_type, boundary
            );

            let stream = create_stream_from_chunks(body.as_bytes(), 50);
            let mut m = MultipartStream::new(stream, boundary.as_bytes());

            let part = m.next().await.unwrap().unwrap();
            assert_eq!(part.headers().get("content-type").unwrap(), content_type);
            concat_body(part.body()).await.unwrap();
            assert!(m.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn test_content_transfer_encoding() {
        // Test Content-Transfer-Encoding header (though rarely used in multipart/form-data)
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             Content-Transfer-Encoding: binary\r\n\
             \r\n\
             value\r\n\
             --boundary--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 30);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("content-transfer-encoding").unwrap(), "binary");
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_case_insensitive_headers() {
        // Test that header names are case-insensitive (HTTP standard)
        let boundary = "boundary";
        let body = "--boundary\r\n\
             content-disposition: form-data; name=\"field\"\r\n\
             content-type: text/plain\r\n\
             \r\n\
             value\r\n\
             --boundary--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 30);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        // Headers should be accessible regardless of case
        assert!(part.headers().get("content-disposition").is_some());
        assert!(part.headers().get("Content-Disposition").is_some());
        assert!(part.headers().get("CONTENT-DISPOSITION").is_some());
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value");
        assert!(m.next().await.is_none());
    }

    // ========== Error Recovery Tests ==========

    #[tokio::test]
    async fn test_stream_error_during_body() {
        // Test stream error during body reading
        // We simulate this by using early termination which is similar
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             \r\n\
             partial content"; // Stream ends without proper boundary

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let result = concat_body(part.body()).await;
        // Should get EarlyTerminate error
        assert!(matches!(result, Err(Error::EarlyTerminate)));
    }

    #[tokio::test]
    async fn test_incomplete_boundary_at_end() {
        // Test incomplete boundary at end of stream
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             \r\n\
             value\r\n\
             --bound"; // Incomplete boundary

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let result = concat_body(part.body()).await;
        assert!(matches!(result, Err(Error::EarlyTerminate)));
    }

    #[tokio::test]
    async fn test_malformed_boundary_missing_crlf() {
        // Test boundary missing required CRLF
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             \r\n\
             value\r\n\
             --boundary--"; // Missing final \r\n

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"value");
        assert!(m.next().await.is_none()); // Should still complete successfully
    }

    #[tokio::test]
    async fn test_missing_final_boundary() {
        // Test completely missing final boundary
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             \r\n\
             value"; // No ending boundary at all

        let stream = create_stream_from_chunks(body.as_bytes(), 20);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let result = concat_body(part.body()).await;
        assert!(matches!(result, Err(Error::EarlyTerminate)));
    }

    #[tokio::test]
    async fn test_boundary_injection_attack() {
        // Test boundary injection attack: boundary string appears in body
        let boundary = "abc";
        let body = "--abc\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             \r\n\
             value contains --abc text inside\r\n\
             --abc--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 10);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        let received = concat_body(part.body()).await.unwrap();
        assert_eq!(&received, b"value contains --abc text inside");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_header_injection_attempt() {
        // Test potential header injection (should be handled correctly)
        let boundary = "boundary";
        let body = "--boundary\r\n\
             Content-Disposition: form-data; name=\"field\"\r\n\
             X-Custom: value\r\n\
             \r\n\
             body\r\n\
             --boundary--\r\n";

        let stream = create_stream_from_chunks(body.as_bytes(), 30);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        let part = m.next().await.unwrap().unwrap();
        assert_eq!(part.headers().get("x-custom").unwrap(), "value");
        assert_eq!(&concat_body(part.body()).await.unwrap(), b"body");
        assert!(m.next().await.is_none());
    }

    #[tokio::test]
    async fn test_extremely_long_line_in_headers() {
        // Test extremely long header line (buffer limits)
        let boundary = "boundary";
        let long_value = "a".repeat(100_000); // 100KB header value
        let body = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"field\"\r\nX-Long: {}\r\n\r\nbody\r\n--{}--\r\n",
            boundary, long_value, boundary
        );

        let stream = create_stream_from_chunks(body.as_bytes(), 1000);
        let mut m = MultipartStream::new(stream, boundary.as_bytes());

        // Should either succeed or fail gracefully (not panic)
        match m.next().await {
            Some(Ok(part)) => {
                concat_body(part.body()).await.unwrap();
                assert!(m.next().await.is_none());
            }
            Some(Err(Error::ParseError(_))) => {
                // Acceptable: header too long to parse
            }
            _ => panic!("Unexpected result"),
        }
    }
}
