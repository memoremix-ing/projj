//! Who may use a server started by `projj serve`.
//!
//! The server can stage changes and write files under the root, so it only
//! answers a page that was handed a link printed in the terminal. Each link
//! carries a one-time token after `#`, where it never reaches the server as
//! part of an address, nor its logs. The page trades the token for a session,
//! keeps that in its tab, and sends it with every request; other tabs of the
//! same browser borrow it (see `web/app.js`). Other processes and users on the
//! device, and web pages the browser has open, get nothing.
//!
//! The server holds its port at both loopback addresses (`listen`). Pages
//! are served only at a name made up for this run, `projj-<random>.localhost`,
//! so each run is an origin of its own. Whatever held the port between runs
//! may have left a service worker, storage, or a page listening on the
//! origin it served; none of it reaches a name nobody knew before the run
//! began. Scripts, which send no `Sec-Fetch-Site`, may also use `localhost`,
//! its other subdomains, or either address.
//!
//! Sessions are never cookies. A browser sends a cookie to every port of the
//! host that set it, so any other server on 127.0.0.1 that the browser
//! visited would be handed it. A tab's storage, by contrast, belongs to this
//! server's port alone.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::bail;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use sha2::Digest as _;
use sha2::Sha256;
use tokio::net::TcpListener;

/// Tokens and sessions are this many random bytes, written as hex.
const SECRET_BYTES: usize = 32;

/// The header each request carries its session in.
pub const SESSION_HEADER: &str = "x-projj-session";

/// Where a page trades a link's token for a session (POST), or asks whether
/// its session is still good (GET).
const SESSION_PATH: &str = "/api/session";

/// Random bytes in the name made up for each run: too many to guess the name
/// before the run begins.
const NAME_BYTES: usize = 16;

/// How many ports to try, when asked for any, before giving up on finding one
/// free at both loopback addresses.
const PORT_TRIES: usize = 20;

/// Listens on `port` at both loopback addresses, `127.0.0.1` and `[::1]`, or
/// not at all. Port 0 picks one that is free at both.
///
/// Browsers send `localhost` and every `*.localhost` name to `[::1]` first.
/// A server holding only `127.0.0.1` would leave those names to whatever else
/// listens at `[::1]` on the same port, which could pass for projj to anyone
/// who typed one. A device without IPv6 loopback has no `[::1]` for anyone to
/// take, and is served at `127.0.0.1` alone.
pub async fn listen(port: u16) -> anyhow::Result<Vec<TcpListener>> {
    let v4 = |port| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let v6 = |port| SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
    for _ in 0..if port == 0 { PORT_TRIES } else { 1 } {
        let first = TcpListener::bind(v4(port))
            .await
            .with_context(|| format!("cannot listen on {}", v4(port)))?;
        let chosen = first.local_addr()?.port();
        match TcpListener::bind(v6(chosen)).await {
            Ok(second) => return Ok(vec![first, second]),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
                ) =>
            {
                return Ok(vec![first]);
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse && port == 0 => continue,
            Err(err) if err.kind() == ErrorKind::AddrInUse => bail!(
                "another program is listening on {}, where browsers send localhost:{chosen}; \
                 projj will not share the port, so choose another with --port",
                v6(chosen)
            ),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot listen on {}", v6(chosen)));
            }
        }
    }
    bail!("found no port free at both 127.0.0.1 and [::1] in {PORT_TRIES} tries")
}

/// Whether `host`, a `Host` header, names this server: either loopback
/// address, or `localhost` or a subdomain of it, which browsers keep on the
/// device rather than asking DNS. Any other name may be DNS rebinding.
fn is_own_host(host: &str, port: u16) -> bool {
    let Some((name, given)) = host.rsplit_once(':') else {
        return false;
    };
    if given != port.to_string() {
        return false;
    }
    let name = name.to_ascii_lowercase();
    let label = |label: &str| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    match name.as_str() {
        "127.0.0.1" | "[::1]" | "localhost" => true,
        _ => name
            .strip_suffix(".localhost")
            .is_some_and(|sub| sub.split('.').all(label)),
    }
}

/// Secrets are kept only as digests, which also makes looking one up take
/// the same time however much of a guess is right.
type Digest = [u8; 32];

pub struct Gate {
    addr: SocketAddr,
    /// Where pages are served this run, as a `Host`: the name and the port.
    host: String,
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

#[derive(Deserialize)]
struct Redeem {
    token: String,
}

impl Gate {
    pub fn new(addr: SocketAddr) -> Self {
        let mut bytes = [0u8; NAME_BYTES];
        getrandom::fill(&mut bytes).expect("the operating system has no randomness to give");
        Self {
            addr,
            host: format!("projj-{}.localhost:{}", hex::encode(bytes), addr.port()),
            tokens: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// A new link that lets one page in, once.
    pub fn link(&self) -> String {
        let token = secret();
        self.tokens.lock().unwrap().insert(digest(&token));
        format!("http://{}/#token={token}", self.host)
    }

    /// Where pages are served this run.
    pub fn address(&self) -> String {
        format!("http://{}/", self.host)
    }

    /// Whether a page's request went somewhere other than this run's name.
    /// Browsers send `Sec-Fetch-Site` with everything a page asks for;
    /// scripts do not.
    fn is_elsewhere(&self, headers: &HeaderMap) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        headers.contains_key("sec-fetch-site")
            && !host.is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
    }

    /// For a page opened at an old name, or at `localhost`: where this run
    /// is, but nothing more, since every link works once.
    fn elsewhere(&self) -> Response {
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>projj</title>\
             <body style=\"font:15px system-ui;max-width:36em;margin:4em auto;line-height:1.5\">\
             <h1 style=\"font-size:20px\">projj</h1>\
             <p>projj serves pages only at the address it made up when it started, \
             which is {}, so that nothing left behind at an earlier address reaches it.</p>\
             <p>Open the link that projj serve printed in its terminal. Each link works once; \
             press Enter in that terminal for another.</p>",
            self.address()
        );
        (StatusCode::MISDIRECTED_REQUEST, axum::response::Html(page)).into_response()
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        headers
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|session| self.sessions.lock().unwrap().contains(&digest(session)))
    }

    /// Trades a link's token for a session, or explains why it cannot.
    fn redeem(&self, body: &[u8]) -> Response {
        let Ok(Redeem { token }) = serde_json::from_slice(body) else {
            return refuse(
                StatusCode::BAD_REQUEST,
                "Send the link's token as {\"token\": …}.",
            );
        };
        if !self.tokens.lock().unwrap().remove(&digest(&token)) {
            return refuse(
                StatusCode::FORBIDDEN,
                "This link has been used already, or comes from an earlier run of projj serve.",
            );
        }
        let session = secret();
        self.sessions.lock().unwrap().insert(digest(&session));
        (
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({ "session": session })),
        )
            .into_response()
    }

    async fn session(&self, request: Request) -> Response {
        match *request.method() {
            Method::GET if self.has_session(request.headers()) => {
                StatusCode::NO_CONTENT.into_response()
            }
            Method::GET => signed_out(),
            Method::POST => match axum::body::to_bytes(request.into_body(), 4096).await {
                Ok(body) => self.redeem(&body),
                Err(_) => refuse(StatusCode::BAD_REQUEST, "That is not a token."),
            },
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    }

    /// Whether the request is one that a page this server served could have
    /// made. This shuts out other websites (cross-origin writes) and DNS
    /// rebinding (foreign `Host` names), session or not.
    fn is_allowed(&self, method: &Method, headers: &HeaderMap) -> bool {
        let value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let Some(host) = value("host").filter(|host| is_own_host(host, self.addr.port())) else {
            return false;
        };
        if method == Method::GET || method == Method::HEAD {
            return true;
        }
        // The very name the request was sent to: each subdomain of
        // localhost is a site of its own, and none speaks for another.
        let origin_ok =
            value("origin").is_none_or(|origin| origin.strip_prefix("http://") == Some(host));
        let site_ok =
            value("sec-fetch-site").is_none_or(|site| site == "same-origin" || site == "none");
        origin_ok && site_ok
    }
}

fn refuse(status: StatusCode, why: &str) -> Response {
    (status, axum::Json(json!({ "error": why }))).into_response()
}

fn signed_out() -> Response {
    refuse(
        StatusCode::UNAUTHORIZED,
        "This page is no longer signed in to projj serve, which may have restarted. \
         Open the link it printed in its terminal.",
    )
}

/// The page itself (`/`, its script and styles) is open to anyone: it holds
/// no secrets, and shows how to get in. Everything under `/api/` but
/// `SESSION_PATH` needs a session.
pub async fn guard(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let mut response = if !gate.is_allowed(request.method(), request.headers()) {
        (StatusCode::FORBIDDEN, "forbidden").into_response()
    } else if gate.is_elsewhere(request.headers()) {
        gate.elsewhere()
    } else if path == SESSION_PATH {
        gate.session(request).await
    } else if !path.starts_with("/api/") || gate.has_session(request.headers()) {
        next.run(request).await
    } else {
        signed_out()
    };
    // Nothing projj serves belongs in another site's frame, where its
    // buttons could be clicked by trickery, and nothing it serves is to be
    // taken for something other than what it says it is.
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
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
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
    use axum::routing::post;
    use tower::ServiceExt as _;

    fn app() -> (Arc<Gate>, Router) {
        let gate = Arc::new(Gate::new("127.0.0.1:4747".parse().unwrap()));
        let router = Router::new()
            .route("/", get(|| async { "page" }))
            .route("/api/root", get(|| async { "{}" }))
            .route("/api/settings", post(|| async { "{}" }))
            .layer(middleware::from_fn_with_state(gate.clone(), guard));
        (gate, router)
    }

    async fn send(
        router: &Router,
        method: Method,
        path: &str,
        session: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:4747");
        if let Some(session) = session {
            request = request.header(SESSION_HEADER, session);
        }
        router
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    fn token_of(link: &str) -> String {
        let (_, token) = link.split_once("/#token=").unwrap();
        assert_eq!(token.len(), 2 * SECRET_BYTES);
        token.to_owned()
    }

    async fn redeem(router: &Router, token: &str) -> Response {
        let body = json!({ "token": token }).to_string();
        send(router, Method::POST, SESSION_PATH, None, &body).await
    }

    async fn session_of(response: Response) -> String {
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
        answer["session"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_link_lets_one_page_in_once() {
        let (gate, router) = app();
        // The page is anyone's to load; it is the API that is shut.
        assert_eq!(
            send(&router, Method::GET, "/", None, "").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&router, Method::GET, "/api/root", None, "")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&router, Method::GET, SESSION_PATH, None, "")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let token = token_of(&gate.link());
        let response = redeem(&router, &token).await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let session = session_of(response).await;

        for (method, path) in [(Method::GET, "/api/root"), (Method::POST, "/api/settings")] {
            assert_eq!(
                send(&router, method, path, Some(&session), "")
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            send(&router, Method::GET, SESSION_PATH, Some(&session), "")
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        // The link is spent.
        assert_eq!(
            redeem(&router, &token).await.status(),
            StatusCode::FORBIDDEN
        );
        // A made-up session, or the token itself, gets nowhere.
        for wrong in ["0".repeat(2 * SECRET_BYTES), token] {
            assert_eq!(
                send(&router, Method::GET, "/api/root", Some(&wrong), "")
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[tokio::test]
    async fn each_link_is_its_own() {
        let (gate, router) = app();
        let first = token_of(&gate.link());
        let second = token_of(&gate.link());
        assert_ne!(first, second);
        let first = session_of(redeem(&router, &first).await).await;
        let second = session_of(redeem(&router, &second).await).await;
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn foreign_hosts_and_sites_are_refused_even_with_a_token() {
        let (gate, router) = app();
        let token = token_of(&gate.link());
        let body = json!({ "token": token }).to_string();
        // A page on another port of 127.0.0.1 is same-site, though not
        // same-origin.
        let foreign = [
            ("host", "evil.example:4747"),
            ("origin", "http://127.0.0.1:3000"),
            ("sec-fetch-site", "same-site"),
        ];
        for (name, value) in foreign {
            let host = if name == "host" {
                value
            } else {
                "127.0.0.1:4747"
            };
            let mut request = Request::post(SESSION_PATH).header(header::HOST, host);
            if name != "host" {
                request = request.header(name, value);
            }
            let request = request.body(Body::from(body.clone())).unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}: {value}");
        }
        // A page on one subdomain of localhost does not speak for another.
        let request = Request::post(SESSION_PATH)
            .header(header::HOST, "b.localhost:4747")
            .header("origin", "http://a.localhost:4747")
            .body(Body::from(body.clone()))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // The refused requests did not spend the link.
        session_of(redeem(&router, &token).await).await;
    }

    #[tokio::test]
    async fn pages_are_served_only_at_the_runs_own_name() {
        let (gate, router) = app();
        let link = gate.link();
        let name = gate.host.strip_suffix(":4747").unwrap();
        assert!(link.starts_with(&format!("http://{name}:4747/#token=")));
        let label = name
            .strip_prefix("projj-")
            .unwrap()
            .strip_suffix(".localhost")
            .unwrap();
        assert_eq!(label.len(), 2 * NAME_BYTES);
        assert!(is_own_host(&gate.host, 4747));
        // Each run makes up a name of its own.
        assert_ne!(Gate::new("127.0.0.1:4747".parse().unwrap()).host, gate.host);

        let page = |host: &str| {
            Request::get("/")
                .header(header::HOST, host)
                .header("sec-fetch-site", "none")
                .body(Body::empty())
                .unwrap()
        };
        let response = router.clone().oneshot(page(&gate.host)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // A browser at any other name, which something may have served
        // before this run, is told where projj is, and given nothing else.
        let upper = gate.host.to_ascii_uppercase();
        assert_eq!(
            router.clone().oneshot(page(&upper)).await.unwrap().status(),
            StatusCode::OK
        );
        for host in ["127.0.0.1:4747", "localhost:4747", "projj-0.localhost:4747"] {
            let response = router.clone().oneshot(page(host)).await.unwrap();
            assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{host}");
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains(&gate.address()) && !body.contains("token="));
        }
        // Scripts may use the loopback addresses.
        assert_eq!(
            send(&router, Method::GET, "/", None, "").await.status(),
            StatusCode::OK
        );
    }

    #[test]
    fn answers_to_loopback_and_localhost_names_only() {
        for host in [
            "127.0.0.1:4747",
            "[::1]:4747",
            "localhost:4747",
            "LocalHost:4747",
            "review.localhost:4747",
            "a.b-c.localhost:4747",
        ] {
            assert!(is_own_host(host, 4747), "{host}");
        }
        for host in [
            "localhost:4748",
            "localhost",
            "evil.example:4747",
            "localhost.evil.example:4747",
            "evil-localhost:4747",
            ".localhost:4747",
            "a..localhost:4747",
            "localhost.:4747",
            "127.0.0.2:4747",
            "0.0.0.0:4747",
            "localhost:4747.evil",
        ] {
            assert!(!is_own_host(host, 4747), "{host}");
        }
    }

    /// Whether this device has IPv6 loopback to try.
    async fn has_ipv6_loopback() -> bool {
        TcpListener::bind("[::1]:0").await.is_ok()
    }

    #[tokio::test]
    async fn holds_both_loopback_addresses_or_none() {
        if !has_ipv6_loopback().await {
            return;
        }
        let held = listen(0).await.unwrap();
        assert_eq!(held.len(), 2);
        let port = held[0].local_addr().unwrap().port();
        assert_eq!(held[1].local_addr().unwrap().port(), port);
        // Nothing else gets either address on that port.
        for rival in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            assert!(TcpListener::bind(&rival).await.is_err(), "{rival}");
        }

        // A port whose [::1] is taken already is refused, not half held.
        let rival = TcpListener::bind("[::1]:0").await.unwrap();
        let port = rival.local_addr().unwrap().port();
        if TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
            let err = listen(port).await.unwrap_err().to_string();
            assert!(err.contains("will not share the port"), "{err}");
            assert!(TcpListener::bind(("127.0.0.1", port)).await.is_ok());
        }
    }
}
