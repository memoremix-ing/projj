//! Who may use a server started by `projj serve`.
//!
//! The server can stage changes and write files under the root, so it only
//! answers a browser that was handed a link printed in the terminal. Each
//! link carries a one-time token: the first visit trades it for a session
//! cookie, and the link stops working. Other processes and users on the
//! device, and web pages the browser has open, get nothing.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::json;
use sha2::Digest as _;
use sha2::Sha256;

/// Tokens and sessions are this many random bytes, written as hex.
const SECRET_BYTES: usize = 32;

/// Secrets are kept only as digests, which also makes looking one up take
/// the same time however much of a guess is right.
type Digest = [u8; 32];

pub struct Gate {
    addr: SocketAddr,
    /// Cookies are shared by every port of a host, so each server names its
    /// own after its port.
    cookie: String,
    /// Links printed and not yet used.
    tokens: Mutex<HashSet<Digest>>,
    sessions: Mutex<HashSet<Digest>>,
}

fn secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).expect("the operating system has no randomness to give");
    hex::encode(bytes)
}

fn digest(secret: &str) -> Digest {
    Sha256::digest(secret.as_bytes()).into()
}

impl Gate {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            cookie: format!("projj-{}", addr.port()),
            tokens: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// A new link that lets one browser in, once.
    pub fn link(&self) -> String {
        let token = secret();
        self.tokens.lock().unwrap().insert(digest(&token));
        format!("http://{}/?token={token}", self.addr)
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        let sessions = self.sessions.lock().unwrap();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name == self.cookie && sessions.contains(&digest(value)))
    }

    /// Trades a link's token for a session, or explains why it cannot.
    fn redeem(&self, token: &str, headers: &HeaderMap) -> Response {
        let fresh = self.tokens.lock().unwrap().remove(&digest(token));
        if !fresh {
            // Clicking the link again in a browser that already used it is fine.
            if self.has_session(headers) {
                return to_page();
            }
            return locked(
                StatusCode::FORBIDDEN,
                "This link has been used already, or comes from an earlier run of projj serve.",
            );
        }
        let session = secret();
        self.sessions.lock().unwrap().insert(digest(&session));
        let mut response = to_page();
        // Lax rather than Strict, so that a link followed from another app
        // still arrives signed in; writes are guarded by `is_allowed` too.
        let cookie = format!("{}={session}; Path=/; HttpOnly; SameSite=Lax", self.cookie);
        response
            .headers_mut()
            .insert(header::SET_COOKIE, cookie.parse().unwrap());
        response
    }

    /// Whether the request is one that a page this server served could have
    /// made. This shuts out other websites (cross-origin writes) and DNS
    /// rebinding (foreign `Host` names), session or not.
    fn is_allowed(&self, method: &Method, headers: &HeaderMap) -> bool {
        let port = self.addr.port();
        let allowed = [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        if !value("host").is_some_and(|host| allowed.iter().any(|a| a == host)) {
            return false;
        }
        if method == Method::GET || method == Method::HEAD {
            return true;
        }
        let origin_ok = value("origin").is_none_or(|origin| {
            allowed
                .iter()
                .any(|host| origin.strip_prefix("http://") == Some(host))
        });
        let site_ok =
            value("sec-fetch-site").is_none_or(|site| site == "same-origin" || site == "none");
        origin_ok && site_ok
    }
}

/// The token from `/?token=…`, the only place a link puts it.
fn token_param(request: &Request) -> Option<&str> {
    if request.uri().path() != "/" {
        return None;
    }
    request
        .uri()
        .query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

/// Sends the browser on to the app, leaving the token out of its address bar
/// and history.
fn to_page() -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/"), (header::CACHE_CONTROL, "no-store")],
    )
        .into_response()
}

fn locked(status: StatusCode, why: &str) -> Response {
    let how = "Open the link that projj serve printed in its terminal. Each link works \
               once; press Enter in that terminal for another.";
    let page = format!(
        "<!doctype html><meta charset=utf-8><title>projj</title>\
         <body style=\"font:15px system-ui;max-width:36em;margin:4em auto;line-height:1.5\">\
         <h1 style=\"font-size:20px\">projj</h1><p>{why}</p><p>{how}</p>"
    );
    (status, Html(page)).into_response()
}

pub async fn guard(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    let mut response = if !gate.is_allowed(request.method(), request.headers()) {
        (StatusCode::FORBIDDEN, "forbidden").into_response()
    } else if let Some(token) = token_param(&request) {
        gate.redeem(token, request.headers())
    } else if gate.has_session(request.headers()) {
        next.run(request).await
    } else if request.uri().path().starts_with("/api/") {
        let error = "This page is no longer signed in to projj serve, which may have restarted. \
                     Open the link it printed in its terminal.";
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": error })),
        )
            .into_response()
    } else {
        locked(
            StatusCode::UNAUTHORIZED,
            "This browser has not been let in to this projj server yet.",
        )
    };
    // Nothing projj serves belongs in another site's frame, where its
    // buttons could be clicked by trickery.
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::Body;
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt as _;

    fn app() -> (Arc<Gate>, Router) {
        let gate = Arc::new(Gate::new("127.0.0.1:4747".parse().unwrap()));
        let router = Router::new()
            .route("/", get(|| async { "page" }))
            .route("/api/root", get(|| async { "{}" }))
            .layer(middleware::from_fn_with_state(gate.clone(), guard));
        (gate, router)
    }

    async fn send(router: &Router, path: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::get(path).header(header::HOST, "127.0.0.1:4747");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_link_lets_one_browser_in_once() {
        let (gate, router) = app();
        assert_eq!(
            send(&router, "/", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&router, "/api/root", None).await.status(),
            StatusCode::UNAUTHORIZED
        );

        let link = gate.link();
        let path = link.strip_prefix("http://127.0.0.1:4747").unwrap();
        assert_eq!(path.len(), "/?token=".len() + 2 * SECRET_BYTES);
        let response = send(&router, path, None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/");
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("projj-4747=") && cookie.contains("HttpOnly"));
        let cookie = cookie.split(';').next().unwrap().to_owned();

        assert_eq!(
            send(&router, "/", Some(&cookie)).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&router, "/api/root", Some(&cookie)).await.status(),
            StatusCode::OK
        );
        // The link is spent: another browser cannot use it...
        assert_eq!(
            send(&router, path, None).await.status(),
            StatusCode::FORBIDDEN
        );
        // ...but the one that did may follow it again.
        assert_eq!(
            send(&router, path, Some(&cookie)).await.status(),
            StatusCode::SEE_OTHER
        );
        // A made-up session or another port's cookie gets nowhere.
        let forged = format!("projj-4747={}", "0".repeat(2 * SECRET_BYTES));
        assert_eq!(
            send(&router, "/api/root", Some(&forged)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let other_port = cookie.replace("projj-4747", "projj-4748");
        assert_eq!(
            send(&router, "/api/root", Some(&other_port)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn each_link_is_its_own() {
        let (gate, router) = app();
        let first = gate.link();
        let second = gate.link();
        assert_ne!(first, second);
        for link in [first, second] {
            let path = link.strip_prefix("http://127.0.0.1:4747").unwrap();
            assert_eq!(
                send(&router, path, None).await.status(),
                StatusCode::SEE_OTHER
            );
        }
    }

    #[tokio::test]
    async fn foreign_hosts_are_refused_even_with_a_token() {
        let (gate, router) = app();
        let path = gate.link().replace("http://127.0.0.1:4747", "");
        let request = Request::get(&path)
            .header(header::HOST, "evil.example:4747")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // The refused request did not spend the link.
        assert_eq!(
            send(&router, &path, None).await.status(),
            StatusCode::SEE_OTHER
        );
    }
}
