pub use async_iterator::LendingIterator;
use bytes::{Buf, Bytes, BytesMut};
use constcat::concat_bytes;
pub use futures_util::{TryStream, TryStreamExt};
pub use http::header;
use http::{HeaderMap, HeaderName, HeaderValue};
use httparse::{EMPTY_HEADER, Status, parse_headers};
use memchr::memmem::Finder;
use std::{
    mem,
    ops::Not,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("the stream has been terminated before the end of the part")]
    EarlyTerminate,
    #[error("stream error: {0}")]
    StreamError(#[from] Box<dyn std::error::Error>),
    #[error("parse error: {0}")]
    ParseError(#[from] ParseError),
    #[error("body stream is not consumed")]
    BodyNotConsumed,
}

/// 表示解析器当前所处的状态
#[derive(Debug)]
enum ParserState {
    Preamble(usize),       // 找到头的边界，移动缓冲区指针至 hdr 初始位置
    ReadingHeaders(usize), // 正在读取头的内容
    StreamingBody(usize),  /* 移动最后一个窗口的内容，下次拼接头以后再判断，
                            * 但是这样还是要拷贝 */
    Finished,
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error(transparent)]
    Other(#[from] httparse::Error),
    #[error("buffer no cahnge")]
    BufferNoChange,
    #[error("incomplete headers content")]
    TryParsePartial,
}

const CRLF: &[u8] = b"\r\n";
const DOUBLE_HYPEN: &[u8] = b"--";
pub struct MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    rx: S,
    terminated: bool,
    state: ParserState,
    pattern: Box<[u8]>, // `\r\n -- boundary \r\n`
    header_body_splitter_finder: Finder<'static>,
    header_body_splitter_len: usize,
    buf: BytesMut,
}

impl<S> MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    pub fn new(stream: S, boundary: &[u8]) -> Self {
        let mut pattern = Vec::with_capacity(boundary.len() + 2 * CRLF.len() + 2 * DOUBLE_HYPEN.len());
        pattern.extend_from_slice(CRLF);
        pattern.extend_from_slice(DOUBLE_HYPEN);
        pattern.extend_from_slice(boundary);
        pattern.extend_from_slice(CRLF);
        const HEADER_BODY_SPLITTER: &[u8] = concat_bytes!(CRLF, CRLF);
        Self {
            rx: stream,
            terminated: false,
            state: ParserState::Preamble(0),
            buf: BytesMut::new(),
            header_body_splitter_finder: Finder::new(HEADER_BODY_SPLITTER),
            header_body_splitter_len: HEADER_BODY_SPLITTER.len(),
            pattern: pattern.into(),
        }
    }

    fn update_scan(&mut self, new_scan: usize) {
        use ParserState::*;
        match &mut self.state {
            Preamble(scan) | ReadingHeaders(scan) | StreamingBody(scan) => {
                debug_assert!(new_scan > *scan);
                *scan = new_scan
            }
            Finished => unreachable!("cannot invoke add_scan on finished state"), /* 几乎不可能会在完成状态继续update
                                                                                   * scan */
        }
    }

    // 在处于 非 Streaming body 状态下均返回 none
    fn poll_next_body_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>> {
        use ParserState::*;
        use Poll::*;
        let pattern_len = self.pattern.len();
        let sub_pattern_len = pattern_len - 2;
        loop {
            let prev_buf_len = self.buf.len();
            let scan = match self.state {
                Preamble(_) | ReadingHeaders(_) | Finished => return Ready(None),
                StreamingBody(scan) => scan,
            };
            if prev_buf_len >= pattern_len + scan {
                // \r\n--boundary (\r\n | --)
                if let Some(pos) = Finder::new(&self.pattern[..sub_pattern_len]).find(&self.buf[scan..]) {
                    let pattern_start = scan + pos;
                    let pattern_tail = {
                        let pos = pattern_start + sub_pattern_len;
                        self.buf.get(pos..pos + 2)
                    };
                    match pattern_tail {
                        Some(CRLF) => {
                            // multipart 的流没有结束，开始下一个 part headers 的解析,
                            // 此时立刻调用此函数只会返回 none
                            self.state = Preamble(0);
                            let chunk = self.buf.split_to(pattern_start).freeze();
                            return Ready(Some(Ok(chunk)));
                        }
                        Some(DOUBLE_HYPEN) => {
                            // multipart 流已经结束，同时意味着也不会有 body 流了
                            // 下次调用此函数只会返回 none
                            self.state = Finished;
                            let chunk = self.buf.split_to(pattern_start).freeze();
                            self.buf.clear(); // 跳过 `-- boundary --`
                            return Ready(Some(Ok(chunk)));
                        }
                        Some(_) => {
                            // 恰好有和模式一样的内容在 body 中
                            let new_scan = self.buf.len() - sub_pattern_len + 1;
                            if new_scan == scan {
                                return Ready(Some(Err(ParseError::BufferNoChange.into())));
                            }
                            self.update_scan(new_scan);
                        }
                        // 继续接收来判断后两个字节
                        None => {}
                    }
                } else {
                    let new_scan = self.buf.len() - sub_pattern_len + 1;
                    if new_scan == scan {
                        return Ready(Some(Err(ParseError::BufferNoChange.into())));
                    }
                    self.update_scan(new_scan);
                }
            }

            // streaming body 状态下，终止则返回早终止
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
            let pattern_no_start_crlf = &self.pattern[2..];
            match self.state {
                // --boundary\r\n
                Preamble(scan) if prev_buf_len >= pattern_no_crlf_len + scan => {
                    if let Some(pos) = Finder::new(pattern_no_start_crlf).find(&self.buf[scan..]) {
                        let total_advance_len = scan + pos + pattern_no_crlf_len;
                        // 如果 advance 长度大于 当前缓冲区长度就继续接收
                        if self.buf.len() >= total_advance_len {
                            self.buf.advance(total_advance_len);
                            self.state = ReadingHeaders(0);
                        }
                    } else {
                        // 扫描只会进行到最后一个满足窗口大小的窗口，所以将 scan 指定到最后满足最后一个窗口的位置之后
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
                        let hdrs_contnet = &self.buf[..hdrs_end]; // 两个 CRLF 也要纳入解析
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
                        // 指定新的待扫描位置，依然是刚好最后一个窗口之后
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
    S::Error: std::error::Error + Send + Sync + 'static,
{
    type Item<'a>
        = Result<Part<'a, S>, Error>
    where
        S: 'a;

    async fn next(&mut self) -> Option<Self::Item<'_>> {
        let this = self as *mut Self;
        let result = futures_util::future::poll_fn(move |cx| {
            let this = unsafe { &mut *this };
            this.poll_next_part(cx)
        })
        .await;
        unsafe { mem::transmute::<Option<Result<Part<'_, S>, Error>>, Option<Result<Part<'_, S>, Error>>>(result) }
    }
}

pub struct Part<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    body: &'a mut MultipartStream<S>,
    headers: Box<HeaderMap>,
}

impl<'a, S> Part<'a, S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    fn new(stream: &'a mut MultipartStream<S>, headers: Box<HeaderMap>) -> Self { Self { body: stream, headers } }

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

    // 一个辅助函数，用于将字节数组切片转换为 Bytes 流
    // 这样可以模拟数据以块（chunk）的形式到达
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

        // 使用一个很小的块大小来强制测试缓冲逻辑
        let stream = create_stream_from_chunks(BODY, 5);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 解析第一部分
        let part1 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert!(!part1.headers().contains_key("content-type"));
        assert_eq!(&concat_body(part1.body()).await, b"value1");

        // 解析第二部分
        let part2 = multipart_stream.next().await.unwrap().unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(part2.headers().get("content-type").unwrap(), "text/plain");
        let body = concat_body(part2.body()).await;
        assert_eq!(&body, b"value2 with CRLF\r\n");
        // 应该已经到达流的末尾
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
        // 应该已经到达流的末尾
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

        let stream = create_stream_from_chunks(BODY, 15); // Usar chunks pequeños
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

        // Obtener la primera parte, pero no consumir su cuerpo
        let _part1 = m.next().await.unwrap().unwrap();

        // Intentar obtener la siguiente parte inmediatamente debe fallar
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
        // "Invalid Header" contiene un espacio, lo cual es ilegal para un nombre de cabecera.
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
}
