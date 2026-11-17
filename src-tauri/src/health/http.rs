//! HTTP 健康探针：`GET http://…`，不跟随重定向，只读取响应体前 64 KiB。
//!
//! 只支持 `http://`（v1 不做 TLS）。连接、读写共用一个总超时；3xx 不跟随，
//! 状态码不在 `expect_status` 中即失败。传输层可注入，测试默认使用本机
//! `TcpListener`，不依赖真实教务/网关。

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use super::state::{FailureKind, HealthProbe, ProbeReport};

/// 响应体只保留前 64 KiB（`expect_body_contains` 只看这一段）。
pub const BODY_LIMIT: usize = 64 * 1024;
const HEADER_LIMIT: usize = 16 * 1024;
const CHUNKED_RAW_LIMIT: usize = 256 * 1024;
/// 单个 chunk 允许声明的最大字节数；超过即按畸形响应处理，避免加法溢出/切片越界。
const CHUNK_SIZE_LIMIT: usize = CHUNKED_RAW_LIMIT;

/// HTTP 探针配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSpec {
    /// 必须是 `http://` 开头。
    pub url: String,
    /// 连接 + 写请求 + 读响应的总超时。
    pub timeout: Duration,
    /// 可接受的状态码；空列表按 `[200]` 处理。
    pub expect_status: Vec<u16>,
    /// 响应体前 64 KiB 必须包含的子串（区分大小写）。
    pub expect_body_contains: Option<String>,
}

/// 一次 HTTP 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub timeout: Duration,
}

/// 一次 HTTP 响应（body 已截断到 64 KiB）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub latency: Duration,
}

/// 传输层错误分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpTransportError {
    /// URL 不是 `http://` 或无法解析。
    InvalidUrl(String),
    /// 连接失败（拒绝连接、DNS 失败、地址不可达）。
    Connect(String),
    /// 连接/读写超时。
    Timeout,
    /// 连接建立后的 IO 错误。
    Io(String),
    /// 响应不符合 HTTP 基本格式。
    Protocol(String),
}

impl std::fmt::Display for HttpTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(f, "URL 无效：{message}"),
            Self::Connect(message) => write!(f, "连接失败：{message}"),
            Self::Timeout => f.write_str("请求超时"),
            Self::Io(message) => write!(f, "IO 错误：{message}"),
            Self::Protocol(message) => write!(f, "HTTP 协议错误：{message}"),
        }
    }
}

impl std::error::Error for HttpTransportError {}

/// 可注入的 HTTP 传输层。
pub trait HttpTransport: Send {
    fn get(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpTransportError>;
}

/// 默认传输层：标准库 `TcpStream`。
#[derive(Debug, Default, Clone, Copy)]
pub struct TcpHttpTransport;

impl HttpTransport for TcpHttpTransport {
    fn get(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        let started = Instant::now();
        if request.timeout.is_zero() {
            return Err(HttpTransportError::Timeout);
        }
        let target = parse_http_url(&request.url)?;
        let deadline = started + request.timeout;

        let addrs = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|error| HttpTransportError::Connect(error.to_string()))?;
        let mut last_error = None;
        let mut stream = None;
        for addr in addrs {
            let left = remaining(deadline)?;
            match TcpStream::connect_timeout(&addr, left) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let mut stream = match stream {
            Some(stream) => stream,
            None => {
                return Err(match last_error {
                    Some(error) if error.kind() == io::ErrorKind::TimedOut => {
                        HttpTransportError::Timeout
                    }
                    Some(error) => HttpTransportError::Connect(error.to_string()),
                    None => HttpTransportError::Connect("没有可用地址".to_owned()),
                });
            }
        };

        let remaining = remaining(deadline)?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(classify_io)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(classify_io)?;

        let request_text = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nUser-Agent: {}\r\nConnection: close\r\n\r\n",
            target.path,
            target.host_header,
            crate::brand::USER_AGENT
        );
        stream
            .write_all(request_text.as_bytes())
            .map_err(classify_io)?;
        stream.flush().map_err(classify_io)?;

        let (status, body) = read_response(&mut stream, deadline)?;
        Ok(HttpResponse {
            status,
            body,
            latency: started.elapsed(),
        })
    }
}

/// HTTP 探针。
pub struct HttpProbe {
    spec: HttpSpec,
    transport: Box<dyn HttpTransport>,
}

impl HttpProbe {
    pub fn new(spec: HttpSpec) -> Self {
        Self {
            spec,
            transport: Box::new(TcpHttpTransport),
        }
    }

    pub fn with_transport(spec: HttpSpec, transport: Box<dyn HttpTransport>) -> Self {
        Self { spec, transport }
    }

    pub fn spec(&self) -> &HttpSpec {
        &self.spec
    }
}

impl HealthProbe for HttpProbe {
    fn probe(&mut self, _now: Instant) -> ProbeReport {
        let started = Instant::now();
        let request = HttpRequest {
            url: self.spec.url.clone(),
            timeout: self.spec.timeout,
        };
        let response = match self.transport.get(&request) {
            Ok(response) => response,
            Err(error) => {
                let kind = match &error {
                    HttpTransportError::InvalidUrl(_) => FailureKind::HttpInvalidUrl,
                    HttpTransportError::Connect(_) => FailureKind::HttpConnect,
                    HttpTransportError::Timeout => FailureKind::HttpTimeout,
                    HttpTransportError::Io(_) => FailureKind::HttpIo,
                    HttpTransportError::Protocol(_) => FailureKind::HttpProtocol,
                };
                return ProbeReport::failed(kind, error.to_string(), started.elapsed());
            }
        };

        let latency = started.elapsed();
        let expected = if self.spec.expect_status.is_empty() {
            response.status == 200
        } else {
            self.spec.expect_status.contains(&response.status)
        };
        if !expected {
            return ProbeReport::failed(
                FailureKind::HttpStatus,
                format!(
                    "HTTP {} 不在 expect_status {:?} 中",
                    response.status, self.spec.expect_status
                ),
                latency,
            )
            .with_http_status(Some(response.status));
        }

        if let Some(needle) = &self.spec.expect_body_contains {
            let body = &response.body[..response.body.len().min(BODY_LIMIT)];
            if !contains_bytes(body, needle.as_bytes()) {
                return ProbeReport::failed(
                    FailureKind::HttpBodyMismatch,
                    format!("响应体前 64 KiB 不包含 {needle:?}"),
                    latency,
                )
                .with_http_status(Some(response.status));
            }
        }

        ProbeReport::healthy(latency).with_http_status(Some(response.status))
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

struct Target {
    host: String,
    port: u16,
    path: String,
    host_header: String,
}

fn parse_http_url(url: &str) -> Result<Target, HttpTransportError> {
    let url = url.trim();
    let prefix = url
        .get(..7)
        .ok_or_else(|| HttpTransportError::InvalidUrl("URL 太短".to_owned()))?;
    if !prefix.eq_ignore_ascii_case("http://") {
        return Err(HttpTransportError::InvalidUrl(
            "仅支持 http://（v1 不做 TLS）".to_owned(),
        ));
    }
    let rest = &url[7..];
    if rest.is_empty() {
        return Err(HttpTransportError::InvalidUrl("缺少主机名".to_owned()));
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let after = &rest[authority_end..];
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        return Err(HttpTransportError::InvalidUrl("缺少主机名".to_owned()));
    }

    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped
            .find(']')
            .ok_or_else(|| HttpTransportError::InvalidUrl("IPv6 地址缺少 ]".to_owned()))?;
        let host = &stripped[..end];
        let remainder = &stripped[end + 1..];
        let port = match remainder {
            "" => 80,
            _ => {
                let port = remainder.strip_prefix(':').ok_or_else(|| {
                    HttpTransportError::InvalidUrl("IPv6 地址后只能是 :端口".to_owned())
                })?;
                parse_port(port)?
            }
        };
        (host.to_owned(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err(HttpTransportError::InvalidUrl(
                "IPv6 地址必须用 [] 包裹".to_owned(),
            ));
        }
        (host.to_owned(), parse_port(port)?)
    } else {
        (authority.to_owned(), 80)
    };
    if host.is_empty() {
        return Err(HttpTransportError::InvalidUrl("缺少主机名".to_owned()));
    }

    let mut path = if after.is_empty() {
        "/".to_owned()
    } else if after.starts_with('/') {
        after.to_owned()
    } else {
        format!("/{after}")
    };
    if let Some(fragment) = path.find('#') {
        path.truncate(fragment);
    }
    if path.is_empty() {
        path.push('/');
    }

    let host_for_header = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let host_header = if port == 80 {
        host_for_header
    } else {
        format!("{host_for_header}:{port}")
    };

    Ok(Target {
        host,
        port,
        path,
        host_header,
    })
}

fn parse_port(text: &str) -> Result<u16, HttpTransportError> {
    let port = text
        .parse::<u16>()
        .map_err(|_| HttpTransportError::InvalidUrl(format!("端口无效：{text}")))?;
    if port == 0 {
        return Err(HttpTransportError::InvalidUrl("端口不能为 0".to_owned()));
    }
    Ok(port)
}

fn remaining(deadline: Instant) -> Result<Duration, HttpTransportError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(HttpTransportError::Timeout)
    } else {
        Ok(left)
    }
}

fn classify_io(error: io::Error) -> HttpTransportError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => HttpTransportError::Timeout,
        _ => HttpTransportError::Io(error.to_string()),
    }
}

fn read_some(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, HttpTransportError> {
    let left = remaining(deadline)?;
    stream.set_read_timeout(Some(left)).map_err(classify_io)?;
    stream.read(buffer).map_err(classify_io)
}

fn read_response(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<(u16, Vec<u8>), HttpTransportError> {
    let mut raw = Vec::with_capacity(4096);
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find_header_end(&raw) {
            break position;
        }
        if raw.len() > HEADER_LIMIT {
            return Err(HttpTransportError::Protocol("响应头超过 16 KiB".to_owned()));
        }
        let read = read_some(stream, &mut buffer, deadline)?;
        if read == 0 {
            return Err(HttpTransportError::Protocol(
                "响应头未完成连接已关闭".to_owned(),
            ));
        }
        raw.extend_from_slice(&buffer[..read]);
    };

    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| HttpTransportError::Protocol("响应头不是 UTF-8".to_owned()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpTransportError::Protocol("缺少状态行".to_owned()))?;
    let mut parts = status_line.split_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| HttpTransportError::Protocol("状态行不完整".to_owned()))?;
    if !version.starts_with("HTTP/") {
        return Err(HttpTransportError::Protocol(format!(
            "不是 HTTP 状态行：{status_line}"
        )));
    }
    let status = parts
        .next()
        .ok_or_else(|| HttpTransportError::Protocol("缺少状态码".to_owned()))?
        .parse::<u16>()
        .map_err(|_| HttpTransportError::Protocol("状态码不是数字".to_owned()))?;
    if !(100..=599).contains(&status) {
        return Err(HttpTransportError::Protocol(format!(
            "状态码超出范围：{status}"
        )));
    }

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpTransportError::Protocol(format!("响应头无效：{line}")))?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                content_length =
                    Some(value.trim().parse::<usize>().map_err(|_| {
                        HttpTransportError::Protocol("Content-Length 无效".to_owned())
                    })?);
            }
            "transfer-encoding" => {
                chunked = value.to_ascii_lowercase().contains("chunked");
            }
            _ => {}
        }
    }

    // RFC 9110：1xx/204/304 没有消息体。304 的 Content-Length 描述选中表示的大小，
    // 1xx/204 也可能被错误地带上；这些状态不能按长度等待 body，否则会误报不完整。
    if status_allows_no_body(status) {
        return Ok((status, Vec::new()));
    }

    let mut body = raw[header_end..].to_vec();
    if chunked {
        body = read_chunked(stream, body, deadline)?;
    } else if let Some(length) = content_length {
        let want = length.min(BODY_LIMIT);
        while body.len() < want {
            let read = read_some(stream, &mut buffer, deadline)?;
            if read == 0 {
                return Err(HttpTransportError::Protocol(format!(
                    "响应体不完整：Content-Length 声明 {length} 字节（至少应读到 {want} 字节），实际只收到 {} 字节",
                    body.len()
                )));
            }
            body.extend_from_slice(&buffer[..read]);
        }
        body.truncate(want);
    } else {
        while body.len() < BODY_LIMIT {
            let read = read_some(stream, &mut buffer, deadline)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
        }
    }
    body.truncate(BODY_LIMIT);
    Ok((status, body))
}

/// 1xx/204/304 按 RFC 9110 不携带消息体；304 的 Content-Length 不是响应体长度。
fn status_allows_no_body(status: u16) -> bool {
    (100..=199).contains(&status) || status == 204 || status == 304
}

/// chunked 解码的一轮结果。
enum ChunkedDecode {
    /// 已读到终止块（含 trailer 形式）。
    Complete(Vec<u8>),
    /// 原始数据还不够，需要继续读取。
    Incomplete,
    /// 已解码到 BODY_LIMIT，健康断言只需要前 64 KiB，无需再读。
    BodyLimit(Vec<u8>),
}

fn read_chunked(
    stream: &mut TcpStream,
    mut raw: Vec<u8>,
    deadline: Instant,
) -> Result<Vec<u8>, HttpTransportError> {
    let mut buffer = [0u8; 4096];
    loop {
        match decode_chunked(&raw)? {
            ChunkedDecode::Complete(body) | ChunkedDecode::BodyLimit(body) => return Ok(body),
            ChunkedDecode::Incomplete => {
                if raw.len() >= CHUNKED_RAW_LIMIT {
                    return Err(HttpTransportError::Protocol(format!(
                        "chunked 响应体超过 {CHUNKED_RAW_LIMIT} 字节原始上限仍未读到终止块"
                    )));
                }
                let read = read_some(stream, &mut buffer, deadline)?;
                if read == 0 {
                    return Err(HttpTransportError::Protocol(
                        "chunked 响应体在终止块前连接已关闭".to_owned(),
                    ));
                }
                raw.extend_from_slice(&buffer[..read]);
            }
        }
    }
}

/// 解析 chunked 编码；只解码、不读取。畸形编码返回 Protocol 错误。
///
/// 块大小先校验 CHUNK_SIZE_LIMIT 上限再做 checked_add：超大声明
/// （如 ffffffffffffffff）必须在做加法/切片前拒绝，否则 debug 下溢出
/// panic、release 下切片越界 panic。
fn decode_chunked(raw: &[u8]) -> Result<ChunkedDecode, HttpTransportError> {
    let mut decoded = Vec::new();
    let mut position = 0usize;
    loop {
        let Some(line_end) = find_crlf(&raw[position..]).map(|index| position + index) else {
            return Ok(ChunkedDecode::Incomplete);
        };
        let size_part = raw[position..line_end]
            .split(|byte| *byte == b';')
            .next()
            .unwrap_or_default();
        let size_text = std::str::from_utf8(size_part)
            .map_err(|_| protocol("chunk 块大小不是 ASCII"))?
            .trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| protocol(format!("chunk 块大小无效：{size_text:?}")))?;
        if size > CHUNK_SIZE_LIMIT {
            return Err(protocol(format!(
                "chunk 块大小 {size_text} 超过上限 {CHUNK_SIZE_LIMIT} 字节"
            )));
        }
        position = line_end
            .checked_add(2)
            .ok_or_else(|| protocol("chunk 偏移溢出"))?;

        if size == 0 {
            let rest = &raw[position..];
            if rest.starts_with(b"\r\n") || find_header_end(rest).is_some() {
                return Ok(ChunkedDecode::Complete(decoded));
            }
            return Ok(ChunkedDecode::Incomplete);
        }

        let data_end = position
            .checked_add(size)
            .ok_or_else(|| protocol("chunk 偏移溢出"))?;
        if data_end > raw.len() {
            return Ok(ChunkedDecode::Incomplete);
        }
        decoded.extend_from_slice(&raw[position..data_end]);
        let crlf_end = data_end
            .checked_add(2)
            .ok_or_else(|| protocol("chunk 偏移溢出"))?;
        if crlf_end > raw.len() {
            return Ok(ChunkedDecode::Incomplete);
        }
        if &raw[data_end..crlf_end] != b"\r\n" {
            return Err(protocol("chunk 数据后缺少 CRLF"));
        }
        position = crlf_end;
        if decoded.len() >= BODY_LIMIT {
            return Ok(ChunkedDecode::BodyLimit(decoded));
        }
    }
}

fn protocol(message: impl Into<String>) -> HttpTransportError {
    HttpTransportError::Protocol(message.into())
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}
