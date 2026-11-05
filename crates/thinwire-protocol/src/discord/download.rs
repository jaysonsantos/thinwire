//! Download one attachment from the Discord CDN for the player.
//!
//! Only HTTPS addresses on the two Discord CDN hosts are read, and the body
//! stops at [`VOICE_MAX_BYTES`]. A failure has no URL, no header, and no
//! body text.

use std::time::Duration;

use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use super::api::DiscordApiError;
use crate::voice_files::VOICE_MAX_BYTES;

/// The hosts of Discord attachment files.
const CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

/// The longest wait for the answer, and again for the body.
const DOWNLOAD_WAIT: Duration = Duration::from_secs(60);

/// `true` for an HTTPS address on a Discord CDN host.
pub(super) fn is_cdn_url(uri: &hyper::Uri) -> bool {
    uri.scheme_str() == Some("https") && uri.host().is_some_and(|host| CDN_HOSTS.contains(&host))
}

pub(super) async fn fetch(url: &str) -> Result<Vec<u8>, DiscordApiError> {
    let uri: hyper::Uri = url.parse().map_err(|_| DiscordApiError::Rejected)?;
    if !is_cdn_url(&uri) {
        return Err(DiscordApiError::Rejected);
    }
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(rustls::crypto::ring::default_provider())
        .map_err(|_| DiscordApiError::Transport)?
        .https_only()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    let request = hyper::Request::get(uri)
        .header(hyper::header::USER_AGENT, "thinwire")
        .body(Empty::new())
        .map_err(|_| DiscordApiError::Rejected)?;
    let response = tokio::time::timeout(DOWNLOAD_WAIT, client.request(request))
        .await
        .map_err(|_| DiscordApiError::Transport)?
        .map_err(|_| DiscordApiError::Transport)?;
    match response.status().as_u16() {
        200 => {}
        403 => return Err(DiscordApiError::Forbidden),
        404 => return Err(DiscordApiError::NotFound),
        _ => return Err(DiscordApiError::Transport),
    }
    let body = Limited::new(response.into_body(), VOICE_MAX_BYTES);
    let collected = tokio::time::timeout(DOWNLOAD_WAIT, body.collect())
        .await
        .map_err(|_| DiscordApiError::Transport)?
        .map_err(|_| DiscordApiError::Transport)?;
    Ok(collected.to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an HTTPS address on a Discord CDN host is read.
    #[test]
    fn only_the_discord_cdn_is_read() {
        // `fetch` refuses an address that does not parse.
        let ok = |url: &str| url.parse().is_ok_and(|uri| is_cdn_url(&uri));
        assert!(ok(
            "https://cdn.discordapp.com/attachments/1/2/voice-message.ogg?ex=1"
        ));
        assert!(ok("https://media.discordapp.net/attachments/1/2/a.mp3"));
        assert!(!ok("http://cdn.discordapp.com/attachments/1/2/a.ogg"));
        assert!(!ok("https://cdn.discordapp.com.example.com/a.ogg"));
        assert!(!ok("https://example.com/a.ogg"));
        assert!(!ok("file:///etc/passwd"));
    }
}
