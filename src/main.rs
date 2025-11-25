use tokio::net::TcpListener;

use crate::connect::handle_connect;

mod connect;
mod parse_req;
mod resp;

const MAX_HEADERS_PER_REQ: usize = 32;
const MAX_HEADER_BYTES: usize = 8 * 1024;

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
					if let Err(er) =
						handle_connect(stream, MAX_HEADERS_PER_REQ, MAX_HEADER_BYTES).await
					{
						// nothing can be done at this point -> log as is
						eprintln!("connect failed: {}, {:?}", addr, er);
					}
				});
			}
			Err(e) => eprintln!("accept failed: {}", e),
		}
	}
}
