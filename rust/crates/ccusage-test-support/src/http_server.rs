//! A loopback HTTP server that replays a script of responses.
//!
//! The cloud-sync code talks to GCS and to Google's token endpoints over real
//! sockets, so its tests need a server rather than a mocked client: that is the
//! only way to assert on the bytes that actually go out — the query string a
//! precondition produced, the absence of an `authorization` header on a public
//! read. One response is served per request, in order, and every request is
//! recorded verbatim.

use std::{
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long a test waits for a request before giving up, rather than hanging
/// the whole suite on a client that never connected.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ScriptedServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl ScriptedServer {
    /// Starts a server that answers the next request with the next response.
    /// Extra requests get no answer, which surfaces as a client-side network
    /// error — the same shape as the connection being dropped.
    pub fn serving(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
        listener
            .set_nonblocking(true)
            .expect("scripted server nonblocking");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("scripted server address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let Ok(mut stream) = accept(&listener) else {
                    break;
                };
                match read_request(&mut stream) {
                    Ok(request) => recorded.lock().expect("requests").push(request),
                    Err(_) => break,
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint,
            requests,
            handle: Some(handle),
        }
    }

    /// The `http://127.0.0.1:PORT` base URL, with no trailing slash.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The requests received so far, each as head plus body. Waits for the
    /// script to be exhausted first, so a test never races the server thread.
    pub fn requests(&mut self) -> Vec<String> {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.requests.lock().expect("requests").clone()
    }
}

/// Builds a response with an accurate `content-length`, which is what lets the
/// client finish the read without waiting for the socket to close.
pub fn response(status: u16, reason: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub fn json_response(status: u16, body: &str) -> String {
    response(status, "OK", "content-type: application/json\r\n", body)
}

fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "no request"));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    // A declared body has to be drained, or the client blocks on its own write.
    match content_length(&head) {
        Some(length) => {
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body)?;
            Ok(format!("{head}{}", String::from_utf8_lossy(&body)))
        }
        None => Ok(head),
    }
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split_once(':'))
        .and_then(|(_, value)| value.trim().parse().ok())
}
