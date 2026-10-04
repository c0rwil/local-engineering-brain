//! DNS-rebinding guard for Engram's loopback HTTP listeners: the workspace
//! browser and the model proxy.
//!
//! A page on another site can point its own DNS name at 127.0.0.1, but the
//! browser still sends that name in `Host`, and its own origin in `Origin`.
//! The guard answers only requests addressed to the loopback listener and
//! refuses state-changing requests from another origin. Clients that send no
//! `Origin` header, such as curl, scripts, model SDKs and coding agents, pass.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Wrap `router` so it answers only requests addressed to a loopback listener
/// on `port`. A `Host` that is not `localhost`, a 127.0.0.0/8 address or
/// `[::1]` with that port gets HTTP 421. A request other than GET, HEAD or
/// OPTIONS whose `Origin` is present but not such a loopback origin gets 403.
pub fn guard_loopback(router: Router, port: u16) -> Router {
    router.layer(axum::middleware::from_fn_with_state(port, loopback_only))
}

async fn loopback_only(State(port): State<u16>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !host.is_some_and(|host| is_loopback_authority(host, port)) {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "request is not addressed to this loopback listener",
        )
            .into_response();
    }
    let changes_state = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if changes_state && let Some(origin) = headers.get(header::ORIGIN) {
        let same = origin
            .to_str()
            .ok()
            .and_then(|origin| origin.strip_prefix("http://"))
            .is_some_and(|authority| is_loopback_authority(authority, port));
        if !same {
            return (
                StatusCode::FORBIDDEN,
                "cross-origin requests cannot change state on this loopback listener",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Whether `authority` (a `Host` value, or an origin without its `http://`
/// scheme) is `localhost`, a 127.0.0.0/8 address or `[::1]`, with exactly
/// `port`.
#[must_use]
pub fn is_loopback_authority(authority: &str, port: u16) -> bool {
    if authority.is_empty() || authority.contains(['@', '/', '\\', '?', '#']) {
        return false;
    }
    let Ok(url) = url::Url::parse(&format!("http://{authority}/")) else {
        return false;
    };
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    loopback && url.port_or_known_default() == Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_authorities_with_the_listener_port_match() {
        for authority in [
            "127.0.0.1:11435",
            "localhost:11435",
            "LOCALHOST:11435",
            "[::1]:11435",
            "127.0.0.2:11435",
        ] {
            assert!(is_loopback_authority(authority, 11_435), "{authority}");
        }
        for authority in [
            "",
            "evil.example:11435",
            "localhost.evil.example:11435",
            "127.0.0.1:11436",
            "127.0.0.1",
            "0.0.0.0:11435",
            "[::]:11435",
            "user@127.0.0.1:11435",
            "127.0.0.1:11435/path",
            "localhost.:11435",
        ] {
            assert!(!is_loopback_authority(authority, 11_435), "{authority}");
        }
    }
}
