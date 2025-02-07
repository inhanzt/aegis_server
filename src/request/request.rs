use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, Read};
use std::mem::MaybeUninit;

pub(crate) const MAX_HEADERS: usize = 16;

use bytes::{Buf, BufMut, BytesMut};
use may::net::TcpStream;

use crate::errors::errors::RequestError;

#[derive()]
pub struct Request<'buf, 'header, 'stream> {
    pub parameters: HashMap<String, String>,
    pub url_parameters: HashMap<String, String>,
    pub(crate) req: RawRequest<'buf, 'header, 'stream>,
}

impl<'buf, 'header, 'stream> Request<'buf, 'header, 'stream> {
    pub fn method(&self) -> &str {
        self.req.method()
    }

    pub fn path(&self) -> &str {
        self.req.path()
    }

    pub fn version(&self) -> u8 {
        self.req.version()
    }

    pub fn headers(&self) -> &[httparse::Header<'_>] {
        self.req.headers()
    }

    pub fn json_body(self) -> Result<serde_json::Value, RequestError> {
        let value: serde_json::Value = serde_json::from_reader(self.body())?;
        Ok(value).map_err(|e| RequestError::JsonError(e))
    }

    pub fn body(self) -> BodyReader<'buf, 'stream> {
        self.req.body()
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(|s| s.as_str())
    }

    pub fn url_parameter(&self, name: &str) -> Option<&str> {
        self.url_parameters.get(name).map(|s| s.as_str())
    }

    pub fn keep_alive(&self) -> bool {
        return self.headers().iter().any(|header| {
            header.name.eq_ignore_ascii_case("connection")
                && std::str::from_utf8(header.value).ok() == Some("keep-alive")
        });
    }
}

pub struct BodyReader<'buf, 'stream> {
    // remaining bytes for body
    req_buf: &'buf mut BytesMut,
    // the max body length limit
    body_limit: usize,
    // total read count
    total_read: usize,
    // used to read extra body bytes
    stream: &'stream mut TcpStream,
}

impl<'buf, 'stream> BodyReader<'buf, 'stream> {
    pub fn body_limit(&self) -> usize {
        self.body_limit
    }
}

impl<'buf, 'stream> Read for BodyReader<'buf, 'stream> {
    // the user should control the body reading, don't exceeds the body!
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.total_read >= self.body_limit {
            return Ok(0);
        }

        loop {
            if !self.req_buf.is_empty() {
                let min_len = buf.len().min(self.body_limit - self.total_read);
                let n = self.req_buf.reader().read(&mut buf[..min_len])?;
                self.total_read += n;
                return Ok(n);
            }

            crate::http::http_server::reserve_buf(self.req_buf);
            let read_buf: &mut [u8] = unsafe { std::mem::transmute(self.req_buf.chunk_mut()) };
            // perform block read from the stream
            let n = self.stream.read(read_buf)?;
            self.total_read += n;
            unsafe { self.req_buf.advance_mut(n) };
        }
    }
}

impl<'buf, 'stream> BufRead for BodyReader<'buf, 'stream> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(self.req_buf.chunk())
    }

    fn consume(&mut self, amt: usize) {
        self.req_buf.advance(amt)
    }
}

impl<'buf, 'stream> fmt::Debug for BodyReader<'buf, 'stream> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<HTTP BodyReader>")
    }
}
pub struct RawRequest<'buf, 'header, 'stream> {
    req: httparse::Request<'header, 'buf>,
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut TcpStream,
}

impl<'buf, 'header, 'stream> RawRequest<'buf, 'header, 'stream> {
    pub fn method(&self) -> &str {
        self.req.method.unwrap()
    }

    pub fn path(&self) -> &str {
        self.req.path.unwrap()
    }

    pub fn version(&self) -> u8 {
        self.req.version.unwrap()
    }

    pub fn json_body(&self) -> Result<serde_json::Value, RequestError> {
        let body_slice = self.req_buf.as_ref();
        let reader = std::io::Cursor::new(body_slice);

        serde_json::from_reader(reader).map_err(RequestError::from)
    }

    pub fn headers(&self) -> &[httparse::Header<'_>] {
        self.req.headers
    }

    pub fn body(self) -> BodyReader<'buf, 'stream> {
        BodyReader {
            body_limit: self.content_length(),
            total_read: 0,
            stream: self.stream,
            req_buf: self.req_buf,
        }
    }

    fn content_length(&self) -> usize {
        let mut len = usize::MAX;
        for header in self.req.headers.iter() {
            if header.name.eq_ignore_ascii_case("content-length") {
                len = std::str::from_utf8(header.value).unwrap().parse().unwrap();
                break;
            }
        }
        len
    }
}

impl<'buf, 'header, 'stream> fmt::Debug for RawRequest<'buf, 'header, 'stream> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<HTTP Request {} {}>", self.method(), self.path())
    }
}

pub fn decode<'header, 'buf, 'stream>(
    headers: &'header mut [MaybeUninit<httparse::Header<'buf>>; MAX_HEADERS],
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut TcpStream,
) -> io::Result<Option<RawRequest<'buf, 'header, 'stream>>> {
    let mut req = httparse::Request::new(&mut []);
    // safety: don't hold the reference of req_buf
    // so we can transfer the mutable reference to Request
    let buf: &[u8] = unsafe { std::mem::transmute(req_buf.chunk()) };
    let status = match req.parse_with_uninit_headers(buf, headers) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to parse http request: {e:?}");
            let msg = format!("failed to parse http request: {e:?}");
            return Err(io::Error::new(io::ErrorKind::Other, msg));
        }
    };

    let len = match status {
        httparse::Status::Complete(amt) => amt,
        httparse::Status::Partial => return Ok(None),
    };
    req_buf.advance(len);

    Ok(Some(RawRequest {
        req,
        req_buf,
        stream,
    }))
}
