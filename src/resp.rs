use crate::parse_req;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
	Ok = 200,
	BadRequest = 400,
	MethodNotAllowed = 405,
	BadGateway = 502,
}

impl Status {
	pub fn description(&self) -> &str {
		use Status::*;
		match self {
			Ok => "Ok",
			BadRequest => "Bad request",
			MethodNotAllowed => "Method not allowed",
			BadGateway => "Bad gateway",
		}
	}

	pub fn as_http_resp(self) -> Vec<u8> {
		format!("HTTP/1.1 {} {}\r\n\r\n", self as isize, self.description()).into_bytes()
	}
}

impl TryFrom<parse_req::Error> for Status {
	type Error = ();

	fn try_from(err: parse_req::Error) -> Result<Self, Self::Error> {
		use parse_req::Error::*;
		match err {
			InvalidMethod => Ok(Status::MethodNotAllowed),
			NoMethod | NoPath | Invalid { .. } | ReqTooLarge => Ok(Status::BadRequest),
			// client closed/incomplete request -> nothing to send back
			Closed | Incomplete => Err(()),
			// TODO: 400 or rather 500?
			Io { .. } => Ok(Status::BadRequest),
		}
	}
}

// upstream errors are just std::io errors now, but it would be nice to have its own type + TryFrom
pub fn status_from_upstream_connect_error(_e: &std::io::Error) -> Status {
	// maybe handle more edge cases, but this will do for now
	Status::BadGateway
}
