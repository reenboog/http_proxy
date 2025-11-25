use std::str::FromStr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_HEADERS_PER_REQ: usize = 32;
const MAX_HEADER_BYTES: usize = 8 * 1024;

#[derive(Debug, PartialEq)]
enum Method {
	Connect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
	Ok = 200,
	BadRequest = 400,
	MethodNotAllowed = 405,
	BadGateway = 502,
}

impl Status {
	fn description(&self) -> &str {
		use Status::*;
		match self {
			Ok => "Ok",
			BadRequest => "Bad request",
			MethodNotAllowed => "Method not allowed",
			BadGateway => "Bad gateway",
		}
	}
}

// TODO: move to Status as impl?
fn make_status_response(status: Status) -> Vec<u8> {
	format!(
		"HTTP/1.1 {} {}\r\n\r\n",
		status as isize,
		status.description()
	)
	.into_bytes()
}

struct InvalidMethod;

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
enum ParseError {
	NoMethod,
	InvalidMethod,
	NoPath,
	Incomplete,
	Closed,
	Invalid { e: String },
	ReqTooLarge,
	Io { e: String },
}

impl From<InvalidMethod> for ParseError {
	fn from(_val: InvalidMethod) -> Self {
		ParseError::InvalidMethod
	}
}

fn status_from_parse_error(err: &ParseError) -> Option<Status> {
	use ParseError::*;
	match err {
		InvalidMethod => Some(Status::MethodNotAllowed),
		NoMethod | NoPath | Invalid { .. } | ReqTooLarge => Some(Status::BadRequest),
		// client closed/incomplete request -> nothing to send back
		Closed | Incomplete => None,
		// TODO: 400 or rather 500?
		Io { .. } => Some(Status::BadRequest),
	}
}

fn status_from_upstream_connect_error(_e: &std::io::Error) -> Status {
	// maybe handle more edge cases, but this will do for now
	Status::BadGateway
}

#[derive(Debug)]
enum ProxyError {
	Parse(ParseError),
	UpstreamConnect(std::io::Error),
	// a generic io error AFTER connect is complete
	Io(std::io::Error),
}

impl From<ParseError> for ProxyError {
	fn from(e: ParseError) -> Self {
		ProxyError::Parse(e)
	}
}

impl From<std::io::Error> for ProxyError {
	fn from(e: std::io::Error) -> Self {
		ProxyError::Io(e)
	}
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
	// TODO: add a connection pool limiter probably?
	let ip = "127.0.0.1";
	let port = 8080;
	let addr = format!("{ip}:{port}");

	println!("starting {addr}...");

	let listener = TcpListener::bind(addr).await?;

	loop {
		match listener.accept().await {
			Ok((stream, addr)) => {
				println!("accepted {addr}, handling...");

				tokio::spawn(async move {
					if let Err(er) = handle_connection(stream).await {
						// nothing can be done at this point -> log as is
						eprintln!("connect failed: {}, {:?}", addr, er);
					}
				});
			}
			Err(e) => eprintln!("accept failed: {}", e),
		}
	}
}

// returns method, path & bytes_parsed.len(), if any
fn parse_req_from_buf(
	buf: &[u8],
	max_headers: usize,
) -> Result<(Method, String, usize), ParseError> {
	let mut headers = vec![httparse::EMPTY_HEADER; max_headers];
	let mut req = httparse::Request::new(&mut headers);

	use httparse::Status::*;

	match req.parse(buf) {
		Ok(Complete(parsed_len)) => {
			// if there's an error, httparse will freak out sooner, but let's keep these two errors for readability
			let method = Method::from_str(req.method.ok_or(ParseError::NoMethod)?)?;
			// we assume port is always specified, so path is used directly, though
			// a check could be introduced for a more verbose output, in case of errors
			let path = req.path.ok_or(ParseError::NoPath)?.to_string();

			Ok((method, path, parsed_len))
		}
		Ok(Partial) => Err(ParseError::Incomplete),
		Err(e) => Err(ParseError::Invalid { e: e.to_string() }),
	}
}

async fn read_and_parse_req<Stream>(
	client: &mut Stream,
	max_headers_per_req: usize,
	max_header_bytes: usize,
) -> Result<(Method, String, Option<Vec<u8>>), ParseError>
where
	Stream: AsyncRead + Unpin,
{
	let mut buf = vec![0u8; max_header_bytes];
	let mut read = 0usize;

	// read data until we have METHOD and PATH
	loop {
		// eof, but failed to complete parsing = better skip
		if read == buf.len() {
			return Err(ParseError::ReqTooLarge);
		}

		let n = client
			.read(&mut buf[read..])
			.await
			.map_err(|e| ParseError::Io { e: e.to_string() })?;
		if n == 0 {
			// connection closed before sending a request
			if read == 0 {
				return Err(ParseError::Closed);
			} else {
				// eof, but no headers/path yet? something is not right probably
				return Err(ParseError::Invalid {
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
				if ParseError::Incomplete == e {
					// not enough bytes read to parse a request
					continue;
				} else {
					return Err(e);
				}
			}
		}
	}
}

async fn handle_connection(client: TcpStream) -> std::io::Result<()> {
	handle_connection_with(client, |addr| TcpStream::connect(addr)).await
}

async fn handle_connection_with<Client, Connect, ConnectFut, Upstream>(
	client: Client,
	connect: Connect,
) -> std::io::Result<()>
where
	Client: AsyncRead + AsyncWrite + Unpin,
	Upstream: AsyncRead + AsyncWrite + Unpin,
	Connect: FnOnce(String) -> ConnectFut,
	ConnectFut: std::future::Future<Output = std::io::Result<Upstream>>,
{
	let mut client = client;

	match proxy_connect(&mut client, connect).await {
		Ok(()) => Ok(()),
		Err(ProxyError::Parse(parse_err)) => {
			if let Some(status) = status_from_parse_error(&parse_err) {
				write_status(&mut client, status).await?;
			}
			Ok(())
		}
		Err(ProxyError::UpstreamConnect(er)) => {
			let status = status_from_upstream_connect_error(&er);
			write_status(&mut client, status).await?;
			Ok(())
		}
		Err(ProxyError::Io(e)) => {
			// FIXME: reconsider
			// connection is more likely to be dead at this point, so no need to write anything back to the client
			Err(e)
		}
	}
}

async fn proxy_connect<Client, ConnectF, ConnectFut, Upstream>(
	client: &mut Client,
	connect: ConnectF,
) -> Result<(), ProxyError>
where
	Client: AsyncRead + AsyncWrite + Unpin,
	Upstream: AsyncRead + AsyncWrite + Unpin,
	ConnectF: FnOnce(String) -> ConnectFut,
	ConnectFut: std::future::Future<Output = std::io::Result<Upstream>>,
{
	// read and parse the request
	let (method, path, leftover) =
		read_and_parse_req(client, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await?;

	println!("handling {:?} to {path}", method,);

	// connect to the target host
	let mut upstream = connect(path).await.map_err(ProxyError::UpstreamConnect)?;

	// send back 200 and start piping
	write_status(client, Status::Ok).await?;

	// rare, but if parsing were to produce any levftover bytes, feed them to the upstream
	if let Some(leftover) = leftover {
		upstream
			.write_all(&leftover)
			.await
			.map_err(ProxyError::Io)?;
	}

	// tunnel the rest
	tokio::io::copy_bidirectional(client, &mut upstream)
		.await
		.map_err(ProxyError::Io)?;

	Ok(())
}

async fn write_status<S: tokio::io::AsyncWrite + Unpin>(
	stream: &mut S,
	status: Status,
) -> std::io::Result<()> {
	let bytes = make_status_response(status);
	stream.write_all(&bytes).await
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

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

		assert_eq!(err, ParseError::InvalidMethod);
	}

	#[tokio::test]
	async fn test_read_and_parse_simple_connect_no_leftover() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let (method, path, leftover) =
			read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES)
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
			read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES)
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

		let res = read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert_eq!(res.unwrap_err(), ParseError::Closed);
	}

	#[tokio::test]
	async fn test_read_and_parse_eof_mid_request() {
		// no closing \r\n\r\n -> partial parsing -> eof
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(ParseError::Invalid { e }) if e == "eof"));
	}

	#[tokio::test]
	async fn test_read_and_parse_req_too_large() {
		let raw = b"CONNECT google.com:443 HTTP/1.1\r\nHost: google.com:443\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, 16).await;
		assert_eq!(res.unwrap_err(), ParseError::ReqTooLarge);
	}

	#[tokio::test]
	async fn test_read_and_parse_invalid_method_propagates() {
		let raw = b"GET / HTTP/1.1\r\nHost: google.com\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert_eq!(res.unwrap_err(), ParseError::InvalidMethod);
	}

	#[tokio::test]
	async fn test_read_and_parse_no_path() {
		// an empty space instead of a path
		let raw = b"CONNECT  HTTP/1.1\r\nHost: google.com\r\n\r\n";
		let mut cursor = Cursor::new(&raw[..]);

		let res = read_and_parse_req(&mut cursor, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(ParseError::Invalid { e: _e })));
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

		let res = read_and_parse_req(&mut reader, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await;
		assert!(matches!(res, Err(ParseError::Io { e }) if e == "ohmy"));
	}

	#[tokio::test]
	async fn test_invalid_method_returns_405() {
		let (mut client, proxy) = duplex(1024);

		let client_task = tokio::spawn(async move {
			client
				.write_all(b"GET / HTTP/1.1\r\nHost: google.com\r\n\r\n")
				.await
				.unwrap();

			let mut buf = [0u8; 64];
			let n = client.read(&mut buf).await.unwrap();
			let resp = std::str::from_utf8(&buf[..n]).unwrap();
			assert!(resp.starts_with("HTTP/1.1 405"));
		});

		// parsing is to fail with MethodNotAllowed, so this should not be called
		let connect = |_addr: String| async {
			panic!("connect must not be called for invalid method");
			#[allow(unreachable_code)]
			{
				Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"should not happen",
				))
			}
		};

		let res = handle_connection_with::<_, _, _, TcpStream>(proxy, connect).await;

		assert!(res.is_ok());

		client_task.await.unwrap();
	}

	#[tokio::test]
	async fn test_upstream_connect_error_returns_502() {
		let (mut client, proxy) = duplex(1024);

		let client_task = tokio::spawn(async move {
			client
				.write_all(b"CONNECT google.com:443 HTTP/1.1\r\n\r\n")
				.await
				.unwrap();

			let mut buf = [0u8; 64];
			let n = client.read(&mut buf).await.unwrap();
			let resp = std::str::from_utf8(&buf[..n]).unwrap();
			assert!(resp.starts_with("HTTP/1.1 502"));
		});

		// simulate 'host unreachable' basically
		let connect = |_addr: String| async {
			Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				"no upstream",
			))
		};

		let res = handle_connection_with::<_, _, _, TcpStream>(proxy, connect).await;
		assert!(res.is_ok());
		client_task.await.unwrap();
	}

	#[tokio::test]
	async fn test_successful_connect_sends_200_leftover_and_tunnels_data() {
		let (mut client, proxy) = duplex(1024);
		let (proxy_to_upstream, mut upstream) = duplex(1024);

		let client_task = tokio::spawn(async move {
			// pretend some data ('hello') is sent alongside with CONNECT
			client
				.write_all(b"CONNECT google.com:443 HTTP/1.1\r\n\r\nhello")
				.await
				.unwrap();

			let mut buf = [0u8; 64];
			let n = client.read(&mut buf).await.unwrap();
			let resp = std::str::from_utf8(&buf[..n]).unwrap();

			assert!(resp.starts_with("HTTP/1.1 200"));

			// and then the client sends more data
			client.write_all(b" world").await.unwrap();
		});

		let data = b"hello world";
		let upstream_task = tokio::spawn(async move {
			// imitate processing by the specified remote host
			let mut buf = [0u8; 64];
			let mut read = 0;

			// read pulls SOME bytes from its source, so read until the end
			while read < data.len() {
				let n = upstream.read(&mut buf[read..]).await.unwrap();
				if n == 0 {
					break;
				}
				read += n;
			}

			assert_eq!(&buf[..read], data);
		});

		let connect = |_addr: String| async move { Ok(proxy_to_upstream) };
		let res = handle_connection_with(proxy, connect).await;

		assert!(res.is_ok());

		client_task.await.unwrap();
		upstream_task.await.unwrap();
	}
}
