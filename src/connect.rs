use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::parse_req;
use crate::resp;

#[derive(Debug)]
pub enum Error {
	Parse(parse_req::Error),
	UpstreamConnect(std::io::Error),
	// a generic io error *after* CONNECT is complete
	Io(std::io::Error),
}

impl From<parse_req::Error> for Error {
	fn from(e: parse_req::Error) -> Self {
		Error::Parse(e)
	}
}

impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Error::Io(e)
	}
}

pub async fn handle_connect(
	client: TcpStream,
	max_headers_per_req: usize,
	max_header_bytes: usize,
) -> std::io::Result<()> {
	handle_connect_with(client, max_headers_per_req, max_header_bytes, |addr| {
		TcpStream::connect(addr)
	})
	.await
}

async fn handle_connect_with<Client, Connect, ConnectFut, Upstream>(
	client: Client,
	max_headers_per_req: usize,
	max_header_bytes: usize,
	connect: Connect,
) -> std::io::Result<()>
where
	Client: AsyncRead + AsyncWrite + Unpin,
	Upstream: AsyncRead + AsyncWrite + Unpin,
	Connect: FnOnce(String) -> ConnectFut,
	ConnectFut: std::future::Future<Output = std::io::Result<Upstream>>,
{
	let mut client = client;

	match proxy_connect(&mut client, max_headers_per_req, max_header_bytes, connect).await {
		Ok(()) => Ok(()),
		Err(Error::Parse(err)) => {
			if let Ok(status) = resp::Status::try_from(err) {
				write_status(&mut client, status).await?;
			}
			Ok(())
		}
		Err(Error::UpstreamConnect(er)) => {
			let status = resp::status_from_upstream_connect_error(&er);
			write_status(&mut client, status).await?;
			Ok(())
		}
		Err(Error::Io(e)) => {
			// FIXME: reconsider
			// connection is more likely to be dead at this point, so no need to write anything back to the client
			Err(e)
		}
	}
}

async fn proxy_connect<Client, ConnectF, ConnectFut, Upstream>(
	client: &mut Client,
	max_headers_per_req: usize,
	max_header_bytes: usize,
	connect: ConnectF,
) -> Result<(), Error>
where
	Client: AsyncRead + AsyncWrite + Unpin,
	Upstream: AsyncRead + AsyncWrite + Unpin,
	ConnectF: FnOnce(String) -> ConnectFut,
	ConnectFut: std::future::Future<Output = std::io::Result<Upstream>>,
{
	// read and parse the request
	let (method, path, leftover) =
		parse_req::read_and_parse(client, max_headers_per_req, max_header_bytes).await?;

	println!("handling {:?} to {path}", method,);

	// connect to the target host
	let mut upstream = connect(path).await.map_err(Error::UpstreamConnect)?;

	// send back 200 and start piping
	write_status(client, resp::Status::Ok).await?;

	// rare, but if parsing were to produce any levftover bytes, feed them to the upstream
	if let Some(leftover) = leftover {
		upstream.write_all(&leftover).await.map_err(Error::Io)?;
	}

	// tunnel the rest
	tokio::io::copy_bidirectional(client, &mut upstream)
		.await
		.map_err(Error::Io)?;

	Ok(())
}

async fn write_status<S: tokio::io::AsyncWrite + Unpin>(
	stream: &mut S,
	status: resp::Status,
) -> std::io::Result<()> {
	let bytes = status.as_http_resp();
	stream.write_all(&bytes).await
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt, duplex},
		net::TcpStream,
	};

	const MAX_HEADERS_PER_REQ: usize = 32;
	const MAX_HEADER_BYTES: usize = 8 * 1024;

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

		let res = handle_connect_with::<_, _, _, TcpStream>(
			proxy,
			MAX_HEADERS_PER_REQ,
			MAX_HEADER_BYTES,
			connect,
		)
		.await;

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

		let res = handle_connect_with::<_, _, _, TcpStream>(
			proxy,
			MAX_HEADERS_PER_REQ,
			MAX_HEADER_BYTES,
			connect,
		)
		.await;
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
		let res = handle_connect_with(proxy, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES, connect).await;

		assert!(res.is_ok());

		client_task.await.unwrap();
		upstream_task.await.unwrap();
	}
}
