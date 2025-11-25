use std::str::FromStr;

use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Debug, PartialEq)]
pub enum Method {
	Connect,
}

pub struct InvalidMethod;

impl TryFrom<&str> for Method {
	type Error = InvalidMethod;

	fn try_from(val: &str) -> Result<Self, Self::Error> {
		match val {
			"CONNECT" => Ok(Self::Connect),
			_ => Err(InvalidMethod),
		}
	}
}

impl FromStr for Method {
	type Err = InvalidMethod;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		TryFrom::try_from(s)
	}
}

#[derive(PartialEq, Debug)]
pub enum Error {
	NoMethod,
	InvalidMethod,
	NoPath,
	Incomplete,
	Closed,
	Invalid { e: String },
	ReqTooLarge,
	Io { e: String },
}

impl From<InvalidMethod> for Error {
	fn from(_val: InvalidMethod) -> Self {
		Error::InvalidMethod
	}
}

// returns method, path & bytes_parsed.len(), if any
fn parse_req_from_buf(buf: &[u8], max_headers: usize) -> Result<(Method, String, usize), Error> {
	let mut headers = vec![httparse::EMPTY_HEADER; max_headers];
	let mut req = httparse::Request::new(&mut headers);

	use httparse::Status::*;

	match req.parse(buf) {
		Ok(Complete(parsed_len)) => {
			// if there's an error, httparse will freak out sooner, but let's keep these two errors for readability
			let method = Method::from_str(req.method.ok_or(Error::NoMethod)?)?;
			// we assume port is always specified, so path is used directly, though
			// a check could be introduced for a more verbose output, in case of errors
			let path = req.path.ok_or(Error::NoPath)?.to_string();

			Ok((method, path, parsed_len))
		}
		Ok(Partial) => Err(Error::Incomplete),
		Err(e) => Err(Error::Invalid { e: e.to_string() }),
	}
}

pub async fn read_and_parse<Stream>(
	client: &mut Stream,
	max_headers_per_req: usize,
	max_header_bytes: usize,
) -> Result<(Method, String, Option<Vec<u8>>), Error>
where
	Stream: AsyncRead + Unpin,
{
	let mut buf = vec![0u8; max_header_bytes];
	let mut read = 0usize;

	// read data until we have METHOD and PATH
	loop {
		// eof, but failed to complete parsing = better skip
		if read == buf.len() {
			return Err(Error::ReqTooLarge);
		}

		let n = client
			.read(&mut buf[read..])
			.await
			.map_err(|e| Error::Io { e: e.to_string() })?;
		if n == 0 {
			// connection closed before sending a request
			if read == 0 {
				return Err(Error::Closed);
			} else {
				// eof, but no headers/path yet? something is not right probably
				return Err(Error::Invalid {
					e: "eof".to_string(),
				});
			}
		}
		read += n;

		match parse_req_from_buf(&buf[..read], max_headers_per_req) {
			Ok((method, path, parsed_len)) => {
				let leftover = if read > parsed_len {
					Some(buf[parsed_len..read].to_vec())
				} else {
					None
				};
				return Ok((method, path, leftover));
			}
			Err(e) => {
				if Error::Incomplete == e {
					// not enough bytes read to parse a request
					continue;
				} else {
					return Err(e);
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	const MAX_HEADERS_PER_REQ: usize = 32;
	const MAX_HEADER_BYTES: usize = 8 * 1024;

	#[test]
	fn test_parse_connect_ok() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n\r\n";
		let (method, path, parsed_len) = parse_req_from_buf(raw, MAX_HEADERS_PER_REQ).unwrap();

		assert_eq!(method, Method::Connect);
		assert_eq!(path, "google.com:443");
		assert_eq!(parsed_len, raw.len());
	}

	#[test]
	fn test_parse_connect_with_leftover() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\n\r\nTLSBYTES";
		let (_method, _path, parsed_len) = parse_req_from_buf(raw, MAX_HEADERS_PER_REQ).unwrap();

		assert_eq!(parsed_len, raw.len() - "TLSBYTES".len());
	}

	#[test]
	fn test_parse_incorrect_method() {
		let raw = b"GET / HTTP/1.1\r\n\r\n";
		let err = parse_req_from_buf(raw, MAX_HEADERS_PER_REQ).unwrap_err();

		assert_eq!(err, Error::InvalidMethod);
	}

	#[tokio::test]
	async fn test_read_and_parse_simple_connect_no_leftover() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let (method, path, leftover) =
			read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES)
				.await
				.unwrap();

		assert_eq!(method, Method::Connect);
		assert_eq!(path, "google.com:443");
		assert!(leftover.is_none());
	}

	#[tokio::test]
	async fn test_read_and_parse_connect_with_leftover_bytes() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\n\r\nTLSBYTES";
		let mut cursor = Cursor::new(&raw[..]);

		let (method, path, leftover) =
			read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES)
				.await
				.unwrap();

		assert_eq!(method, Method::Connect);
		assert_eq!(path, "google.com:443");

		let leftover = leftover.expect("expected leftover bytes");
		assert_eq!(leftover, b"TLSBYTES");
	}

	#[tokio::test]
	async fn test_read_and_parse_closed_before_any_data() {
		let raw = b"";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert_eq!(res.unwrap_err(), Error::Closed);
	}

	#[tokio::test]
	async fn test_read_and_parse_eof_mid_request() {
		// no closing \r\n\r\n -> partial parsing -> eof
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(Error::Invalid { e }) if e == "eof"));
	}

	#[tokio::test]
	async fn test_read_and_parse_req_too_large() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, 16).await;
		assert_eq!(res.unwrap_err(), Error::ReqTooLarge);
	}

	#[tokio::test]
	async fn test_read_and_parse_invalid_method_propagates() {
		let raw = b"GET / HTTP/1.1\r\nHost: google.com\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert_eq!(res.unwrap_err(), Error::InvalidMethod);
	}

	#[tokio::test]
	async fn test_read_and_parse_no_path() {
		// an empty space instead of a path
		let raw = b"CONNECT  HTTP/1.1\r\nHost: google.com\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(Error::Invalid { e: _e })));
	}

	use std::io;
	use std::pin::Pin;
	use std::task::{Context, Poll};
	use tokio::io::ReadBuf;

	struct BrokenPipeReader;

	impl AsyncRead for BrokenPipeReader {
		fn poll_read(
			self: Pin<&mut Self>,
			_cx: &mut Context<'_>,
			_buf: &mut ReadBuf<'_>,
		) -> Poll<io::Result<()>> {
			Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "ohmy")))
		}
	}

	#[tokio::test]
	async fn test_read_and_parse_io_error() {
		let mut reader = BrokenPipeReader;

		let res = read_and_parse(&mut reader, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(Error::Io { e }) if e == "ohmy"));
	}
}
