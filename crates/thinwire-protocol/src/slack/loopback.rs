//! One-shot loopback listener for the Slack OAuth v2 redirect.
//!
//! Binds `127.0.0.1` only. Runs on the tokio worker, never on the UI thread.
//! Requests to other paths get 404 and the listener keeps waiting. A request
//! with a wrong or missing `state` gets 400 and does not end the install.
//! The request line is never logged, because it carries the one-time `code`.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::install::{SLACK_OAUTH_CALLBACK_PATH, SlackCallbackError, parse_loopback_callback};

/// Largest request head the listener reads. Slack's redirect is far smaller.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// Time one browser connection has to send its request head.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>thinwire</title><p>Slack workspace connected. Close this tab and go back to thinwire.</p>";
const PAGE_DECLINED: &str = "<!doctype html><meta charset=utf-8><title>thinwire</title><p>The Slack install was cancelled. Close this tab and go back to thinwire.</p>";
const PAGE_BAD_REQUEST: &str = "<!doctype html><meta charset=utf-8><title>thinwire</title><p>This link is not for the current Slack install.</p>";
const PAGE_NOT_FOUND: &str =
    "<!doctype html><meta charset=utf-8><title>thinwire</title><p>Not found.</p>";

/// Bound loopback socket for one install attempt.
#[derive(Debug)]
pub struct SlackLoopback {
    listener: TcpListener,
}

impl SlackLoopback {
    /// Bind the redirect port. Only loopback addresses are accepted.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "slack oauth listener binds loopback only",
            ));
        }
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Wait for the redirect that carries `expected_state` and return its `code`.
    ///
    /// The caller puts a time limit on this future and drops it to cancel.
    pub async fn wait_for_code(self, expected_state: &str) -> Result<String, SlackCallbackError> {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                continue;
            };
            match serve_one(stream, expected_state).await {
                Outcome::Keep => {}
                Outcome::Done(result) => return result,
            }
        }
    }
}

enum Outcome {
    Keep,
    Done(Result<String, SlackCallbackError>),
}

async fn serve_one(mut stream: TcpStream, expected_state: &str) -> Outcome {
    let Ok(Ok(Some(target))) =
        tokio::time::timeout(REQUEST_READ_TIMEOUT, read_target(&mut stream)).await
    else {
        let _ = respond(&mut stream, "400 Bad Request", PAGE_BAD_REQUEST).await;
        return Outcome::Keep;
    };
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path != SLACK_OAUTH_CALLBACK_PATH {
        let _ = respond(&mut stream, "404 Not Found", PAGE_NOT_FOUND).await;
        return Outcome::Keep;
    }
    match parse_loopback_callback(query, expected_state) {
        Ok(code) => {
            let _ = respond(&mut stream, "200 OK", PAGE_DONE).await;
            Outcome::Done(Ok(code))
        }
        Err(SlackCallbackError::Declined) => {
            let _ = respond(&mut stream, "200 OK", PAGE_DECLINED).await;
            Outcome::Done(Err(SlackCallbackError::Declined))
        }
        Err(
            SlackCallbackError::StateMismatch
            | SlackCallbackError::MissingState
            | SlackCallbackError::Malformed,
        ) => {
            let _ = respond(&mut stream, "400 Bad Request", PAGE_BAD_REQUEST).await;
            Outcome::Keep
        }
        // `MissingCode` is returned only after `state` matches.
        Err(SlackCallbackError::MissingCode) => {
            let _ = respond(&mut stream, "400 Bad Request", PAGE_BAD_REQUEST).await;
            Outcome::Done(Err(SlackCallbackError::MissingCode))
        }
    }
}

/// Read the request head and return the `GET` target. `None` for other methods.
async fn read_target(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() >= MAX_REQUEST_HEAD {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let Ok(text) = std::str::from_utf8(&head) else {
        return Ok(None);
    };
    let Some(line) = text.lines().next() else {
        return Ok(None);
    };
    let mut parts = line.split(' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(target), Some(version)) if version.starts_with("HTTP/1.") => {
            Ok(Some(target.to_string()))
        }
        _ => Ok(None),
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Send one `GET` to the listener and return the status line.
    pub(in crate::slack) async fn get(addr: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        response.lines().next().unwrap_or_default().to_string()
    }

    async fn loopback() -> (SlackLoopback, SocketAddr) {
        let listener = SlackLoopback::bind("127.0.0.1:0".parse().expect("addr"))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        (listener, addr)
    }

    #[tokio::test]
    async fn returns_code_after_ignoring_other_paths_and_forged_state() {
        let (listener, addr) = loopback().await;
        let waiter = tokio::spawn(async move { listener.wait_for_code("state-test").await });

        assert!(get(addr, "/favicon.ico").await.contains("404"));
        assert!(
            get(addr, "/slack/oauth/callback?code=forged&state=other")
                .await
                .contains("400")
        );
        assert!(
            get(
                addr,
                "/slack/oauth/callback?code=code-test&state=state-test"
            )
            .await
            .contains("200")
        );
        let code = waiter.await.expect("join").expect("code");
        assert_eq!(code, "code-test");
    }

    #[tokio::test]
    async fn decline_with_matching_state_ends_the_install() {
        let (listener, addr) = loopback().await;
        let waiter = tokio::spawn(async move { listener.wait_for_code("state-test").await });
        assert!(
            get(
                addr,
                "/slack/oauth/callback?error=access_denied&state=state-test"
            )
            .await
            .contains("200")
        );
        assert_eq!(
            waiter.await.expect("join"),
            Err(SlackCallbackError::Declined)
        );
    }

    #[tokio::test]
    async fn callback_without_state_does_not_end_the_install() {
        let (listener, addr) = loopback().await;
        let waiter = tokio::spawn(async move { listener.wait_for_code("state-test").await });

        assert!(get(addr, "/slack/oauth/callback").await.contains("400"));
        assert!(
            get(addr, "/slack/oauth/callback?code=code-test")
                .await
                .contains("400")
        );
        assert!(
            get(
                addr,
                "/slack/oauth/callback?code=code-test&state=state-test"
            )
            .await
            .contains("200")
        );
        assert_eq!(waiter.await.expect("join").expect("code"), "code-test");
    }

    #[tokio::test]
    async fn matching_state_without_code_ends_the_install() {
        let (listener, addr) = loopback().await;
        let waiter = tokio::spawn(async move { listener.wait_for_code("state-test").await });
        assert!(
            get(addr, "/slack/oauth/callback?state=state-test")
                .await
                .contains("400")
        );
        assert_eq!(
            waiter.await.expect("join"),
            Err(SlackCallbackError::MissingCode)
        );
    }

    #[tokio::test]
    async fn refuses_a_non_loopback_address() {
        let error = SlackLoopback::bind("0.0.0.0:0".parse().expect("addr"))
            .await
            .expect_err("public bind");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
