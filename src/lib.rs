use bytes::{Buf, Bytes, BytesMut};
use futures_util::{TryStream, TryStreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use httparse::{EMPTY_HEADER, Status, parse_headers};
use memchr::memmem::Finder;
use std::{mem, ops::Not, str::FromStr};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("the stream has been terminated before the end of the part")]
    EarlyTerminate,
    #[error("the stream has been terminated")]
    Eof,
    #[error("stream error: {0}")]
    StreamError(#[from] Box<dyn std::error::Error>),
    #[error("parse error: {0}")]
    ParseError(#[from] ParseError),
}

/// 表示解析器当前所处的状态
#[derive(Debug)]
enum ParserState {
    Preamble(usize),                                      // 找到头的边界，移动缓冲区指针至 hdr 初始位置
    ReadingHeaders(usize),                                // 正在读取头的内容
    ReadingBody { headers: Box<HeaderMap>, scan: usize }, // 正在读取 body
    Finished,
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error(transparent)]
    Try(#[from] httparse::Error),
    #[error("buffer no cahnge")]
    BufferNoChange,
    #[error("")]
    TryParsePartial,
}

enum ParseResult {
    Partial,
    Full(Part),
    Failed(ParseError),
    Completed,
}

impl From<httparse::Error> for ParseResult {
    fn from(err: httparse::Error) -> Self { ParseResult::Failed(ParseError::Try(err)) }
}

pub struct MultipartStream<S>
where
    S: TryStream<Ok = Bytes> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    rx: S,
    terminated: bool,
    state: ParserState,
    boundary_pattern: Box<[u8]>,
    boundary_finder: Finder<'static>,
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
        let mut buf = Vec::with_capacity(boundary.len() + 2);
        buf.extend_from_slice(b"--");
        buf.extend_from_slice(boundary);
        let boundary_pattern = buf.into_boxed_slice();
        let boundary_finder = unsafe {
            // 我们知道 'pattern' 会和 'Self' 实例活得一样久,
            // 所以将它的生命周期转换为 'static' 在这里是安全的。
            let static_pattern: &'static [u8] = mem::transmute(&*boundary_pattern);
            Finder::new(static_pattern)
        };
        const HEADER_BODY_SPLITTER: &[u8] = b"\r\n\r\n";
        Self {
            rx: stream,
            terminated: false,
            state: ParserState::Preamble(0),
            buf: BytesMut::new(),
            boundary_finder,
            header_body_splitter_finder: Finder::new(HEADER_BODY_SPLITTER),
            header_body_splitter_len: HEADER_BODY_SPLITTER.len(),
            boundary_pattern,
        }
    }

    /// 从缓冲区中解析 Part，副作用是会消耗 buf，更改内部 state
    fn parse_buf(&mut self) -> ParseResult {
        use ParseResult::*;
        use ParserState::*;
        let state = &mut self.state;
        let buf = &mut self.buf;
        match state {
            &mut Preamble(scan) => {
                if buf.len() < self.boundary_pattern.len() + scan {
                    // 还没有足够的字节来匹配边界
                    return Partial;
                }
                if let Some(pos) = self.boundary_finder.find(&buf[scan..]) {
                    let total_advance_len = scan + pos + self.boundary_pattern.len() + 2; // +2 是因为边界和 headers 间有一个 `\r\n`
                    if buf.len() < total_advance_len {
                        // 找到了 boundary，但是还需要判断后续接收是否还有两个字节
                        return Partial;
                    }
                    buf.advance(pos + self.boundary_pattern.len() + 2);
                    *state = ReadingHeaders(0);
                } else {
                    // 扫描只会进行到最后一个满足窗口大小的窗口，所以将 scan 指定到最后满足最后一个窗口的位置之后
                    let new_pos = buf.len() - self.boundary_pattern.len() + 1;
                    if new_pos == scan {
                        return Failed(ParseError::BufferNoChange);
                    }
                    *state = Preamble(new_pos);
                };
                Partial
            }
            &mut ReadingHeaders(scan) => {
                // RFC 2046 规定了使用 CRLF
                if buf.len() < self.header_body_splitter_len + scan {
                    // 还没有足够的字节来匹配边界
                    return Partial;
                }
                if let Some(pos) = self.header_body_splitter_finder.find(&buf[scan..]) {
                    let offset = scan + pos + self.header_body_splitter_len;
                    let hdrs_contnet = buf.split_to(offset).freeze();
                    let mut hdrs_buf = [EMPTY_HEADER; 64];
                    match parse_headers(&hdrs_contnet, &mut hdrs_buf) {
                        Ok(Status::Complete(_)) => {}
                        Ok(Status::Partial) => return Failed(ParseError::TryParsePartial),
                        Err(err) => return Failed(err.into()),
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
                    *state = ReadingBody { headers: Box::new(headers), scan: 0 };
                } else {
                    // 指定新的待扫描位置，依然是刚好最后一个窗口之后
                    let new_pos = buf.len() - self.header_body_splitter_len + 1;
                    if new_pos == scan {
                        return Failed(ParseError::BufferNoChange);
                    }
                    *state = ReadingHeaders(new_pos);
                };
                Partial
            }
            &mut ReadingBody { ref mut headers, scan } => {
                if buf.len() < self.boundary_pattern.len() + scan {
                    // 还没有足够的字节来匹配边界
                    return Partial;
                }
                if let Some(pos) = self.boundary_finder.find(&buf[scan..]) {
                    let body_end = scan + pos;
                    let tail = {
                        // 匹配 `--  boundary --` 最后的两根 `-`
                        let pos = body_end + self.boundary_pattern.len();
                        buf.get(pos..pos + 2)
                    };
                    let mut split_part = |buf: &mut BytesMut| {
                        let body = buf.split_to(body_end - 2).freeze(); // forget CRLF
                        Part::new(mem::take(headers), body)
                    };
                    match tail {
                        Some(b"--") => {
                            let part = split_part(buf);
                            // 匹配到结束边界以后就可以将状态设置为完成了，下次调用此函数会返回 Completed
                            *state = Finished;
                            Full(part)
                        }
                        // 没有到真正的结尾
                        Some(_) => {
                            let part = split_part(buf);
                            *state = Preamble(0);
                            Full(part)
                        }
                        // 把后面的两字节接收了再来判断
                        None => Partial,
                    }
                } else {
                    let new_pos = buf.len() - self.boundary_pattern.len() + 1;
                    if new_pos == scan {
                        return Failed(ParseError::BufferNoChange);
                    }
                    *state = ReadingBody { headers: mem::take(headers), scan: new_pos };
                    Partial
                }
            }
            Finished => Completed,
        }
    }

    pub async fn try_next(&mut self) -> Result<Part, Error> {
        loop {
            use ParseResult::*;
            // 当流没有终止时才接收，不然会阻塞
            if self.terminated.not() {
                // 尝试填充缓冲区
                match self.rx.try_next().await {
                    Ok(Some(payload)) => {
                        self.buf.extend_from_slice(&payload);
                    }
                    Ok(None) => {
                        // 流终止
                        self.terminated = true;
                    }
                    Err(err) => return Err(Error::StreamError(Box::new(err))),
                }
            }
            let prev_buf_len = self.buf.len();
            match self.parse_buf() {
                Full(part) => return Ok(part),
                Completed => {
                    return Err(Error::Eof);
                }
                Failed(err) => {
                    return Err(err.into());
                }
                Partial => {
                    if self.terminated && self.buf.len() == prev_buf_len {
                        return Err(Error::EarlyTerminate);
                    }
                } // 只知道它没解析完，但是后面可能再循环几次就处理完了
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Part {
    headers: Box<HeaderMap>,
    body: Bytes,
}

impl Part {
    #[inline(always)]
    fn new(headers: Box<HeaderMap>, body: Bytes) -> Self { Self { headers, body } }

    #[inline(always)]
    pub fn headers(&self) -> &HeaderMap { &self.headers }

    #[inline(always)]
    pub fn body(&self) -> &Bytes { &self.body }
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
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());
        // 解析第一个部分
        // let x = multipart_stream.try_next().await;
        let part = multipart_stream.try_next().await.unwrap();
        assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert_eq!(part.body(), &Bytes::from_static(b"value1\r\n"));

        // 应该已经到达流的末尾
        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::Eof)));
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
value2 with CRLF\r\n\
--X-BOUNDARY--\r\n";

        // 使用一个很小的块大小来强制测试缓冲逻辑
        let stream = create_stream_from_chunks(BODY, 5);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 解析第一部分
        let part1 = multipart_stream.try_next().await.unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert!(!part1.headers().contains_key("content-type"));
        assert_eq!(part1.body(), &Bytes::from_static(b"value1\r\n"));

        // 解析第二部分
        let part2 = multipart_stream.try_next().await.unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(part2.headers().get("content-type").unwrap(), "text/plain");
        assert_eq!(part2.body(), &Bytes::from_static(b"value2 with CRLF\r\n"));

        // 应该已经到达流的末尾
        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::Eof)));
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
        // let _ = multipart_stream.try_next().await;
        let part = multipart_stream.try_next().await.unwrap();
        assert_eq!(part.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert_eq!(part.body(), &Bytes::from_static(b"value1\r\n"));

        // 应该已经到达流的末尾
        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::Eof)));
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
        let result = multipart_stream.try_next().await;
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
        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::EarlyTerminate)));
    }

    #[tokio::test]
    async fn test_empty_stream() {
        const BOUNDARY: &str = "boundary";
        const BODY: &[u8] = b"";

        let stream = create_stream_from_chunks(BODY, 10);
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());

        // 对于空流，应该提前终止
        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::EarlyTerminate)));
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
--boundary\r\n\
Content-Disposition: form-data; name=\"field2\"\r\n\
\r\n\
value2\r\n\
--boundary--\r\n";

        let stream = create_stream_from_chunks(BODY, 15); // Usar chunks pequeños
        let mut multipart_stream = MultipartStream::new(stream, BOUNDARY.as_bytes());
        let part1 = multipart_stream.try_next().await.unwrap();
        assert_eq!(part1.headers().get("content-disposition").unwrap(), "form-data; name=\"field1\"");
        assert_eq!(part1.body(), &Bytes::from_static(b"value1\r\n"));

        let part2 = multipart_stream.try_next().await.unwrap();
        assert_eq!(part2.headers().get("content-disposition").unwrap(), "form-data; name=\"empty_field\"");
        assert!(part2.body().is_empty());

        let part3 = multipart_stream.try_next().await.unwrap();
        assert_eq!(part3.headers().get("content-disposition").unwrap(), "form-data; name=\"field2\"");
        assert_eq!(part3.body(), &Bytes::from_static(b"value2\r\n"));

        let result = multipart_stream.try_next().await;
        assert!(matches!(result, Err(Error::Eof)));
    }
}
