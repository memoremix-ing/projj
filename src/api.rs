//! HTTP surface: a JSON API under `/api` plus the embedded web client.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::FromRequestParts;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tower::ServiceExt as _;

use crate::chooser::Chooser;
use crate::files;
use crate::files::Eol;
use crate::files::FileError;
use crate::files::TextFile;
use crate::finder;
use crate::finder::Finder;
use crate::git::Git;
use crate::git::GitError;
use crate::git::Side;
use crate::git::Status;
use crate::git::StatusEntry;
use crate::guide::Guide;
use crate::jj::Jj;
use crate::jj::Session;
use crate::root;
use crate::root::Place;
use crate::root::Repo;
use crate::store;
use crate::store::StoreError;

const DEFAULT_CONTEXT: usize = 3;
const MAX_CONTEXT: usize = 100_000;
const DEFAULT_LOG_LIMIT: usize = 200;
const MAX_LOG_LIMIT: usize = 5000;

/// A request may carry a whole file twice over: as it was and as edited.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const HIGHLIGHT_JS: &str = include_str!("../web/highlight.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

/// contenTTS's modules, which read the page aloud: a copy kept in
/// `web/contentts/` by `scripts/vendor-contentts.sh`.
const CONTENTTS: &[(&str, &str)] = &[
    ("collect.js", include_str!("../web/contentts/collect.js")),
    (
        "contentts.js",
        include_str!("../web/contentts/contentts.js"),
    ),
    ("controls.js", include_str!("../web/contentts/controls.js")),
    ("reader.js", include_str!("../web/contentts/reader.js")),
    ("shortcut.js", include_str!("../web/contentts/shortcut.js")),
    ("speech.js", include_str!("../web/contentts/speech.js")),
    ("text.js", include_str!("../web/contentts/text.js")),
    ("voices.js", include_str!("../web/contentts/voices.js")),
];

/// Discovery asks git to walk the root, so its answer is reused for a moment
/// rather than recomputed for each of a page's requests.
const DISCOVERY_TTL: Duration = Duration::from_secs(2);

/// A walk of the device for repositories is redone when asked for this long
/// after the last one.
const SEARCH_FRESH: Duration = Duration::from_secs(5 * 60);
const SEARCH_HITS: usize = 60;

/// Each page names the root it is about in this header, its path written
/// with `encodeURIComponent`. Pages on different roots can be open at once,
/// so the server never has a "current" root to get wrong.
pub const ROOT_HEADER: &str = "x-projj-root";

/// The server: what every root shares, and the roots pages have opened.
pub struct App {
    pub jj: Jj,
    /// Serve the client from this directory instead of the embedded copy.
    pub web_dir: Option<PathBuf>,
    /// Unsaved editor text, settings and recent roots, shared with every
    /// other server on the device.
    pub store: store::Store,
    /// The root a page opens when it names none: the one around the folder
    /// `projj serve` was started in.
    pub default_root: Option<PathBuf>,
    /// Where projj was started, when that is in no repository: the page
    /// offers to start a root there.
    pub start_dir: Option<PathBuf>,
    finder: Arc<Finder>,
    chooser: Option<Chooser>,
    /// A folder chooser is showing; there is only ever one.
    choosing: AtomicBool,
    roots: Mutex<HashMap<PathBuf, Arc<Root>>>,
    /// Conversations with Codex, beside the review.
    guide: Guide,
}

/// One root, as pages review it.
pub struct Root {
    pub path: PathBuf,
    pub git: Git,
    /// What this root's drafts are filed under in the store.
    drafts_root: String,
    repos: Mutex<Option<(Instant, Arc<Vec<Repo>>)>>,
}

impl Root {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            git: Git::new(&path),
            drafts_root: store::root_key(&path),
            path,
            repos: Mutex::new(None),
        }
    }

    /// The jj repositories and workspaces currently under the root.
    pub(crate) async fn repos(self: &Arc<Self>) -> anyhow::Result<Arc<Vec<Repo>>> {
        if let Some((at, repos)) = &*self.repos.lock().unwrap()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(repos.clone());
        }
        let root = self.clone();
        let repos = tokio::task::spawn_blocking(move || root::discover(&root.path)).await??;
        let repos = Arc::new(repos);
        *self.repos.lock().unwrap() = Some((Instant::now(), repos.clone()));
        Ok(repos)
    }
}

impl App {
    pub fn new(jj: Jj, web_dir: Option<PathBuf>, store: store::Store) -> Self {
        Self {
            jj,
            web_dir,
            store,
            default_root: None,
            start_dir: None,
            finder: Finder::new(finder::search_dirs()),
            chooser: None,
            choosing: AtomicBool::new(false),
            roots: Mutex::default(),
            guide: Guide::new(crate::codex::locate()),
        }
    }

    /// Runs Codex from `program` for the guide, or has no Codex.
    pub fn with_codex(mut self, program: Option<PathBuf>) -> Self {
        self.guide = Guide::new(program);
        self
    }

    /// Opens `root` for pages that name no root.
    pub fn with_default_root(mut self, root: Option<PathBuf>) -> Self {
        self.default_root = root;
        self
    }

    /// Offers to start a root in `dir`, where projj was started, if that is
    /// in no repository.
    pub fn with_start_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.start_dir = dir;
        self
    }

    pub fn with_chooser(mut self, chooser: Option<Chooser>) -> Self {
        self.chooser = chooser;
        self
    }

    /// Looks for repositories under `dirs` instead of the home folder.
    pub fn with_search_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.finder = Finder::new(dirs);
        self
    }

    /// The root at `path`, which must be the top of a git repository.
    async fn root(&self, path: &Path) -> Result<Arc<Root>, ApiError> {
        if let Some(root) = self.roots.lock().unwrap().get(path) {
            return Ok(root.clone());
        }
        if !path.is_absolute() {
            return Err(ApiError::BadRequest(format!(
                "not a full path: {}",
                path.display()
            )));
        }
        let asked = path.to_owned();
        // A root may be named through a link (`/tmp` for `/private/tmp`).
        let (canonical, resolved) = tokio::task::spawn_blocking(move || {
            let canonical = asked.canonicalize()?;
            anyhow::Ok((canonical.clone(), root::resolve(&canonical)?))
        })
        .await
        .map_err(|err| ApiError::Internal(err.into()))?
        .map_err(|err| ApiError::NotFound(format!("{err:#}")))?;
        match resolved {
            Some(resolved) if resolved.root == canonical => {
                let root = Arc::new(Root::new(resolved.root));
                let mut roots = self.roots.lock().unwrap();
                Ok(roots.entry(path.to_owned()).or_insert(root).clone())
            }
            Some(resolved) => Err(ApiError::NotFound(format!(
                "{} is not a root; the root around it is {}",
                path.display(),
                resolved.root.display()
            ))),
            None => Err(ApiError::NotFound(format!(
                "{} is in no git repository",
                path.display()
            ))),
        }
    }
}

/// The root a request names in `ROOT_HEADER`.
struct Here(Arc<Root>);

impl FromRequestParts<AppState> for Here {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, ApiError> {
        let Some(value) = parts.headers.get(ROOT_HEADER) else {
            return Err(ApiError::BadRequest("no root chosen".into()));
        };
        let path = percent_encoding::percent_decode(value.as_bytes())
            .decode_utf8()
            .map_err(|_| ApiError::BadRequest("the root is not valid UTF-8".into()))?;
        Ok(Here(app.root(Path::new(path.as_ref())).await?))
    }
}

type AppState = Arc<App>;

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    /// The request was valid but the repository has moved on.
    Conflict(String),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::Internal(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<GitError> for ApiError {
    fn from(err: GitError) -> Self {
        match err {
            GitError::Invalid(message) => Self::BadRequest(message),
            GitError::Stale(message) => Self::Conflict(message),
            // Most often another git process holds index.lock; retrying works.
            err @ GitError::Failed { .. } => Self::Conflict(err.to_string()),
            err @ GitError::Io(_) => Self::Internal(err.into()),
        }
    }
}

impl From<FileError> for ApiError {
    fn from(err: FileError) -> Self {
        match err {
            FileError::Unsupported(message) => Self::BadRequest(message),
            FileError::Changed(message) => Self::Conflict(message),
            FileError::Io(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Self::NotFound("no such file".into())
            }
            err @ FileError::Io(_) => Self::Internal(err.into()),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            err @ StoreError::NeedsOriginal => Self::Conflict(err.to_string()),
            err @ StoreError::Store { .. } => Self::Internal(err.into()),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Every route, open to whoever can reach it. `projj serve` puts it behind
/// `gate::guard`; the desktop app calls it in process through `Local`.
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/highlight.js", get(highlight_js))
        .route("/style.css", get(style_css))
        .route("/contentts/{name}", get(contentts_js))
        .route("/api/start", get(get_start))
        .route("/api/roots", get(roots_search))
        .route("/api/roots/inspect", get(roots_inspect))
        .route("/api/roots/init", post(roots_init))
        .route("/api/roots/choose", post(roots_choose))
        .route("/api/roots/recent", delete(roots_forget))
        .route("/api/root", get(get_root))
        .route("/api/review/status", get(review_status))
        .route("/api/review/diff", get(review_diff))
        .route("/api/review/apply", post(review_apply))
        .route("/api/review/apply-files", post(review_apply_files))
        .route(
            "/api/file",
            get(file_read).post(file_write).delete(file_delete),
        )
        .route("/api/file/create", post(file_create))
        .route("/api/file/rename", post(file_rename))
        .route("/api/file/rebase", post(file_rebase))
        .route("/api/files", get(file_list))
        .route("/api/settings", get(settings_list).post(settings_set))
        .route(
            "/api/drafts",
            get(drafts_list).post(drafts_put).delete(drafts_remove),
        )
        .route("/api/jj/workspace", get(jj_workspace))
        .route("/api/jj/log", get(jj_log))
        .route("/api/jj/file-log", get(jj_file_log))
        .route("/api/jj/diff", get(jj_diff))
        .route("/api/jj/evolution", get(jj_evolution))
        .route("/api/jj/operations", get(jj_operations))
        .route("/api/jj/snapshot", post(jj_snapshot))
        .route("/api/guide", get(guide_get))
        .route("/api/guide/events", get(guide_events))
        .route("/api/guide/send", post(guide_send))
        .route("/api/guide/stop", post(guide_stop))
        .route("/api/guide/reset", post(guide_reset))
        .route("/api/guide/answer", post(guide_answer))
        .route("/api/guide/leave", post(guide_leave))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(app)
}

/// The whole app without a network, for the desktop app, whose page reaches
/// the API through its webview's message bridge instead.
#[derive(Clone)]
pub struct Local(Router);

impl Local {
    pub fn new(app: Arc<App>) -> Self {
        Self(router(app))
    }

    /// Answers one request about `root` (as the page wrote it in
    /// `ROOT_HEADER`), or about no root. Returns the status and the body as
    /// text.
    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        root: Option<&str>,
        body: String,
    ) -> (u16, String) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(root) = root {
            request = request.header(ROOT_HEADER, root);
        }
        let request = match request.body(axum::body::Body::from(body)) {
            Ok(request) => request,
            Err(err) => return (400, json!({ "error": err.to_string() }).to_string()),
        };
        let response = match self.0.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let status = response.status().as_u16();
        match axum::body::to_bytes(response.into_body(), usize::MAX).await {
            Ok(bytes) => (status, String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) => (500, json!({ "error": err.to_string() }).to_string()),
        }
    }
}

async fn asset(app: &App, name: &str, embedded: &'static str, mime: &'static str) -> Response {
    let body = match &app.web_dir {
        Some(dir) => tokio::fs::read_to_string(dir.join(name))
            .await
            .unwrap_or_else(|err| format!("cannot read {name}: {err}")),
        None => embedded.to_owned(),
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    (headers, body).into_response()
}

async fn index(State(app): State<AppState>) -> Response {
    asset(&app, "index.html", INDEX_HTML, "text/html; charset=utf-8").await
}

async fn app_js(State(app): State<AppState>) -> Response {
    asset(&app, "app.js", APP_JS, "text/javascript; charset=utf-8").await
}

async fn highlight_js(State(app): State<AppState>) -> Response {
    asset(
        &app,
        "highlight.js",
        HIGHLIGHT_JS,
        "text/javascript; charset=utf-8",
    )
    .await
}

async fn style_css(State(app): State<AppState>) -> Response {
    asset(&app, "style.css", STYLE_CSS, "text/css; charset=utf-8").await
}

/// Only the modules named in `CONTENTTS`, so the name reaches nothing else.
async fn contentts_js(
    State(app): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    match CONTENTTS.iter().find(|(known, _)| *known == name) {
        Some((known, embedded)) => {
            asset(
                &app,
                &format!("contentts/{known}"),
                embedded,
                "text/javascript; charset=utf-8",
            )
            .await
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Serialize)]
struct RootInfo {
    path: String,
    /// The path with `~` for the home folder.
    label: String,
    name: String,
    /// The root has a `track/` directory.
    projj: bool,
    /// Which git answers: `installed` or `built-in`.
    git: crate::git::Engine,
    repos: Arc<Vec<Repo>>,
}

/// Opens a root: tells the page about it, and puts it first among the
/// recent ones.
async fn get_root(State(app): State<AppState>, Here(root): Here) -> ApiResult<RootInfo> {
    let path = root.path.display().to_string();
    app.store
        .opened_root(&path, chrono::Utc::now().timestamp_millis())
        .await?;
    Ok(Json(RootInfo {
        label: finder::tilde(&root.path),
        name: root
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone()),
        projj: root.path.join(root::TRACK_DIR).is_dir(),
        git: root.git.engine(),
        repos: root.repos().await?,
        path,
    }))
}

/// A folder someone has typed or chosen, and what opening it would do.
#[derive(Serialize)]
struct Look {
    path: String,
    label: String,
    #[serde(flatten)]
    place: Place,
    /// The root it opens, with `~` for the home folder.
    root_label: Option<String>,
}

fn look(path: PathBuf, place: Place) -> Look {
    Look {
        label: finder::tilde(&path),
        path: path.display().to_string(),
        root_label: match &place {
            Place::Root { resolved, .. } => Some(finder::tilde(&resolved.root)),
            _ => None,
        },
        place,
    }
}

async fn inspect(path: PathBuf) -> Result<Look, ApiError> {
    let (path, place) = tokio::task::spawn_blocking(move || root::inspect(&path))
        .await
        .map_err(|err| ApiError::Internal(err.into()))?
        .map_err(|err| ApiError::BadRequest(format!("{err:#}")))?;
    Ok(look(path, place))
}

#[derive(Serialize)]
struct Start {
    /// The root to open when the page names none.
    root: Option<String>,
    /// Where projj was started, when that is in no repository.
    here: Option<Look>,
    /// A folder chooser can be shown.
    chooser: bool,
    git: crate::git::Engine,
}

/// What a page that names no root should do: open the default root, or help
/// choose one.
async fn get_start(State(app): State<AppState>) -> ApiResult<Start> {
    let here = match (&app.default_root, &app.start_dir) {
        (None, Some(dir)) => inspect(dir.clone()).await.ok(),
        _ => None,
    };
    Ok(Json(Start {
        root: app
            .default_root
            .as_ref()
            .map(|root| root.display().to_string()),
        here,
        chooser: app.chooser.is_some(),
        git: crate::git::Engine::detect(),
    }))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
struct SearchResult {
    hits: Vec<finder::Hit>,
    /// The device is still being looked through; ask again for more.
    scanning: bool,
    looked_at: usize,
    /// What the query names, when it is a path.
    typed: Option<Look>,
}

/// Roots to choose from. A query that starts with `/` or `~` is a path: the
/// answer is the folders that complete it, and what is at it. Anything else
/// is searched for among recent roots and the repositories on the device.
async fn roots_search(
    State(app): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<SearchResult> {
    app.finder.look(SEARCH_FRESH);
    let progress = app.finder.progress();
    let typed = query.q.trim();
    if typed.starts_with('/') || typed.starts_with('~') {
        let completing = typed.to_owned();
        let hits = tokio::task::spawn_blocking(move || finder::complete(&completing, SEARCH_HITS))
            .await
            .map_err(|err| ApiError::Internal(err.into()))?;
        return Ok(Json(SearchResult {
            hits,
            scanning: progress.scanning,
            looked_at: progress.looked_at,
            typed: inspect(PathBuf::from(typed)).await.ok(),
        }));
    }
    let recent = app.store.recent_roots().await?;
    let words = typed.to_owned();
    let hits = tokio::task::spawn_blocking(move || {
        finder::search(&words, &progress.found, &recent, SEARCH_HITS)
    })
    .await
    .map_err(|err| ApiError::Internal(err.into()))?;
    Ok(Json(SearchResult {
        hits,
        scanning: progress.scanning,
        looked_at: progress.looked_at,
        typed: None,
    }))
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn roots_inspect(Query(query): Query<PathQuery>) -> ApiResult<Look> {
    Ok(Json(inspect(PathBuf::from(query.path)).await?))
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

/// Starts a new root: a git repository with an empty `track/`.
async fn roots_init(Json(request): Json<PathBody>) -> ApiResult<serde_json::Value> {
    let path = PathBuf::from(request.path);
    let root = tokio::task::spawn_blocking(move || root::init(&path))
        .await
        .map_err(|err| ApiError::Internal(err.into()))?
        .map_err(|err| ApiError::Conflict(format!("{err:#}")))?;
    Ok(Json(json!({ "root": root.display().to_string() })))
}

#[derive(Deserialize)]
struct ChooseRequest {
    /// The folder the chooser starts in; the home folder if left out.
    start: Option<String>,
}

/// Shows the folder chooser and waits for the person. Answers what they
/// chose, and what is there, or `null` if they cancelled.
async fn roots_choose(
    State(app): State<AppState>,
    Json(request): Json<ChooseRequest>,
) -> ApiResult<Option<Look>> {
    let Some(chooser) = app.chooser.clone() else {
        return Err(ApiError::NotFound("no folder chooser here".into()));
    };
    if app.choosing.swap(true, Ordering::AcqRel) {
        return Err(ApiError::Conflict(
            "a folder chooser is already open".into(),
        ));
    }
    let start = request
        .start
        .map(PathBuf::from)
        .filter(|start| start.is_dir())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let chosen = tokio::task::spawn_blocking(move || chooser(&start)).await;
    app.choosing.store(false, Ordering::Release);
    match chosen.map_err(|err| ApiError::Internal(err.into()))?? {
        Some(path) => Ok(Json(Some(inspect(path).await?))),
        None => Ok(Json(None)),
    }
}

async fn roots_forget(
    State(app): State<AppState>,
    Query(query): Query<PathQuery>,
) -> ApiResult<serde_json::Value> {
    app.store.forget_root(&query.path).await?;
    Ok(Json(json!({})))
}

#[derive(Serialize)]
struct ReviewEntry {
    #[serde(flatten)]
    entry: StatusEntry,
    /// Root-relative path of the jj workspace holding this file, if any.
    workspace: Option<String>,
}

#[derive(Serialize)]
struct ReviewStatus {
    branch: Option<String>,
    head: Option<String>,
    entries: Vec<ReviewEntry>,
}

async fn review_status(Here(root): Here) -> ApiResult<ReviewStatus> {
    let Status {
        branch,
        head,
        entries,
    } = root.git.status().await?;
    let repos = root.repos().await?;
    let entries = entries
        .into_iter()
        .map(|entry| ReviewEntry {
            workspace: root::workspace_for(&repos, &entry.path).map(|ws| ws.path.clone()),
            entry,
        })
        .collect();
    Ok(Json(ReviewStatus {
        branch,
        head,
        entries,
    }))
}

fn context_or_default(context: Option<usize>) -> usize {
    context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT)
}

#[derive(Deserialize)]
struct ReviewDiffQuery {
    path: String,
    side: Side,
    context: Option<usize>,
}

async fn review_diff(
    Here(root): Here,
    Query(query): Query<ReviewDiffQuery>,
) -> Result<Response, ApiError> {
    let context = context_or_default(query.context);
    let diff = root
        .git
        .highlighted_file_diff(&query.path, query.side, context)
        .await?;
    // `null` tells the client this side of the file has no changes left.
    Ok(Json(diff).into_response())
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    /// Mark as reviewed.
    Stage,
    /// Take the review mark back.
    Unstage,
}

#[derive(Deserialize)]
struct ApplyRequest {
    action: Action,
    path: String,
    hunk: String,
    /// Indexes into the hunk's lines; omit to apply the whole hunk.
    lines: Option<Vec<usize>>,
    /// Context the client used when it fetched the hunk.
    context: Option<usize>,
}

async fn review_apply(
    Here(root): Here,
    Json(request): Json<ApplyRequest>,
) -> ApiResult<serde_json::Value> {
    let side = match request.action {
        Action::Stage => Side::Unstaged,
        Action::Unstage => Side::Staged,
    };
    root.git
        .apply_hunk(
            &request.path,
            side,
            context_or_default(request.context),
            &request.hunk,
            request.lines.as_deref(),
        )
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ApplyFilesRequest {
    action: Action,
    paths: Vec<String>,
}

async fn review_apply_files(
    Here(root): Here,
    Json(request): Json<ApplyFilesRequest>,
) -> ApiResult<serde_json::Value> {
    match request.action {
        Action::Stage => root.git.stage_paths(&request.paths).await?,
        Action::Unstage => root.git.unstage_paths(&request.paths).await?,
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct FileQuery {
    /// Path relative to the root.
    path: String,
}

async fn file_read(Here(root): Here, Query(query): Query<FileQuery>) -> ApiResult<TextFile> {
    Ok(Json(files::read(&root.path, &query.path)?))
}

#[derive(Deserialize)]
struct FileWrite {
    path: String,
    /// The whole new content, with `\n` line endings.
    content: String,
    eol: Eol,
    /// The `version` the edit started from. Omit to overwrite whatever is
    /// on disk.
    base: Option<String>,
}

/// Rewrites an existing worktree file. This never stages anything: the edit
/// shows up as one more change to review.
async fn file_write(
    Here(root): Here,
    Json(request): Json<FileWrite>,
) -> ApiResult<serde_json::Value> {
    let version = files::write(
        &root.path,
        &request.path,
        &request.content,
        request.eol,
        request.base.as_deref(),
    )?;
    Ok(Json(json!({ "version": version })))
}

#[derive(Deserialize)]
struct FileCreate {
    path: String,
    kind: files::Kind,
}

/// Makes an empty file or a folder, and any folders it goes in; 409 if
/// something is there already. Nothing is staged: a new file is one more
/// change to review.
async fn file_create(
    Here(root): Here,
    Json(request): Json<FileCreate>,
) -> ApiResult<serde_json::Value> {
    files::create(&root.path, &request.path, request.kind)?;
    Ok(Json(json!({ "path": request.path })))
}

#[derive(Deserialize)]
struct FileRename {
    from: String,
    to: String,
}

/// Renames or moves a file or a folder; 409 if something is at `to`. A
/// folder holding a repository or a jj workspace is refused. Nothing is
/// staged: git sees a deletion and a new file, to review like any others.
async fn file_rename(
    Here(root): Here,
    Json(request): Json<FileRename>,
) -> ApiResult<serde_json::Value> {
    let kind = files::rename(&root.path, &request.from, &request.to)?;
    Ok(Json(json!({ "kind": kind, "path": request.to })))
}

/// Deletes a file, or a folder and everything in it, unless the folder holds
/// a repository or a jj workspace. Nothing is staged.
async fn file_delete(
    Here(root): Here,
    Query(query): Query<FileQuery>,
) -> ApiResult<serde_json::Value> {
    let kind = files::delete(&root.path, &query.path)?;
    Ok(Json(json!({ "kind": kind })))
}

#[derive(Deserialize)]
struct FileRebase {
    path: String,
    /// The content the edits started from.
    base: String,
    /// The edited content.
    content: String,
}

/// Works out what unsaved edits look like on top of the file as it is now.
/// Nothing is written; the caller decides what to do with the answer.
async fn file_rebase(
    Here(root): Here,
    Json(request): Json<FileRebase>,
) -> ApiResult<files::Rebase> {
    Ok(Json(files::rebase(
        &root.path,
        &request.path,
        &request.base,
        &request.content,
    )?))
}

/// Lists the files an explorer should offer: everything git does not ignore.
async fn file_list(Here(root): Here) -> ApiResult<serde_json::Value> {
    Ok(Json(json!({ "files": root.git.files().await? })))
}

/// Display settings, shared by every root and every server on the device.
async fn settings_list(State(app): State<AppState>) -> ApiResult<serde_json::Value> {
    Ok(Json(json!({ "settings": app.store.settings().await? })))
}

#[derive(Deserialize)]
struct SettingUpdate {
    key: String,
    /// Any JSON value; `null` removes the setting.
    value: serde_json::Value,
}

async fn settings_set(
    State(app): State<AppState>,
    Json(update): Json<SettingUpdate>,
) -> ApiResult<serde_json::Value> {
    let plain = !update.key.is_empty()
        && update.key.len() <= 64
        && update
            .key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if !plain {
        return Err(ApiError::BadRequest(format!(
            "invalid setting name: {:?}",
            update.key
        )));
    }
    if update.value.to_string().len() > 64 * 1024 {
        return Err(ApiError::BadRequest("setting too large".into()));
    }
    app.store.set_setting(&update.key, &update.value).await?;
    Ok(Json(json!({})))
}

/// The unsaved edits kept for this root, wherever they were typed.
async fn drafts_list(
    State(app): State<AppState>,
    Here(root): Here,
) -> ApiResult<serde_json::Value> {
    let drafts = app.store.drafts(&root.drafts_root).await?;
    Ok(Json(json!({
        "store": app.store.file().display().to_string(),
        "drafts": drafts,
    })))
}

/// Keeps the latest unsaved text of one file. Nothing is written to the
/// worktree, and the file need not exist any more.
async fn drafts_put(
    State(app): State<AppState>,
    Here(root): Here,
    Json(update): Json<store::Update>,
) -> ApiResult<serde_json::Value> {
    files::check_path(&update.path)?;
    app.store.put_draft(&root.drafts_root, update).await?;
    Ok(Json(json!({})))
}

async fn drafts_remove(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<FileQuery>,
) -> ApiResult<serde_json::Value> {
    app.store
        .remove_draft(&root.drafts_root, &query.path)
        .await?;
    Ok(Json(json!({})))
}

/// Resolves the `ws` parameter to a discovered workspace directory. Only
/// paths found by discovery are accepted, so the value cannot point elsewhere.
async fn workspace_dir(root: &Arc<Root>, ws: &str) -> Result<PathBuf, ApiError> {
    let known = root
        .repos()
        .await?
        .iter()
        .flat_map(|repo| &repo.workspaces)
        .any(|workspace| workspace.path == ws);
    if known {
        Ok(root.path.join(ws))
    } else {
        Err(ApiError::NotFound(format!("unknown workspace: {ws}")))
    }
}

/// Runs a jj query on a blocking thread; `jj-lib` futures are not `Send`.
async fn with_session<T, F>(app: AppState, root: Arc<Root>, ws: String, query: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: AsyncFnOnce(&Session) -> anyhow::Result<T> + Send + 'static,
{
    let dir = workspace_dir(&root, &ws).await?;
    let result = tokio::task::spawn_blocking(move || {
        let session = app.jj.open(&dir)?;
        pollster::block_on(query(&session))
    })
    .await
    .map_err(|err| ApiError::Internal(err.into()))?;
    // Bad revsets and unknown paths are the caller's doing.
    result
        .map(Json)
        .map_err(|err| ApiError::BadRequest(format!("{err:#}")))
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    ws: String,
}

async fn jj_workspace(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<WorkspaceQuery>,
) -> ApiResult<crate::jj::WorkspaceInfo> {
    with_session(app, root, query.ws, async |session| session.info().await).await
}

#[derive(Deserialize)]
struct LogQuery {
    ws: String,
    revset: Option<String>,
    limit: Option<usize>,
}

async fn jj_log(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<LogQuery>,
) -> ApiResult<crate::jj::Log> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, root, query.ws, async move |session| {
        session.log(query.revset.as_deref(), limit).await
    })
    .await
}

#[derive(Deserialize)]
struct FileLogQuery {
    ws: String,
    /// Path relative to the workspace.
    path: String,
    limit: Option<usize>,
}

async fn jj_file_log(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<FileLogQuery>,
) -> ApiResult<crate::jj::Log> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, root, query.ws, async move |session| {
        session.file_log(&query.path, limit).await
    })
    .await
}

#[derive(Deserialize)]
struct DiffQuery {
    ws: String,
    /// Old side; omit to diff `to` against its parents.
    from: Option<String>,
    to: String,
    path: Option<String>,
    context: Option<usize>,
}

async fn jj_diff(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<DiffQuery>,
) -> ApiResult<crate::jj::DiffResult> {
    let context = context_or_default(query.context);
    with_session(app, root, query.ws, async move |session| {
        session
            .diff(
                query.from.as_deref(),
                &query.to,
                query.path.as_deref(),
                context,
            )
            .await
    })
    .await
}

#[derive(Deserialize)]
struct EvolutionQuery {
    ws: String,
    rev: String,
    limit: Option<usize>,
}

async fn jj_evolution(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<EvolutionQuery>,
) -> ApiResult<Vec<crate::jj::EvolutionEntry>> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, root, query.ws, async move |session| {
        session.evolution(&query.rev, limit).await
    })
    .await
}

#[derive(Deserialize)]
struct OperationsQuery {
    ws: String,
    limit: Option<usize>,
}

async fn jj_operations(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<OperationsQuery>,
) -> ApiResult<Vec<crate::jj::OperationInfo>> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, root, query.ws, async move |session| {
        session.operations(limit).await
    })
    .await
}

/// Records the workspace's files on disk into its working-copy commit, which
/// is what any `jj` command does first. This is the one place the server
/// writes to a jj repository, and it goes through jj's own command line (the
/// one built into projj) so the user's full configuration (ignores,
/// fsmonitor, size limits) applies.
async fn jj_snapshot(
    Here(root): Here,
    Query(query): Query<WorkspaceQuery>,
) -> ApiResult<serde_json::Value> {
    let dir = workspace_dir(&root, &query.ws).await?;
    let command = crate::bundled_jj::command(["util", "snapshot"])
        .map_err(|err| ApiError::Internal(anyhow::anyhow!("cannot run jj: {err}")))?;
    let output = tokio::process::Command::from(command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| ApiError::Internal(anyhow::anyhow!("cannot run jj: {err}")))?;
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    if !output.status.success() {
        return Err(ApiError::Conflict(text(&output.stderr)));
    }
    let message = [text(&output.stdout), text(&output.stderr)].join("\n");
    Ok(Json(json!({ "ok": true, "message": message.trim() })))
}

#[derive(Deserialize)]
struct PageQuery {
    /// Names the page, which makes a name up for itself when it loads.
    #[serde(default)]
    page: String,
    #[serde(default)]
    after: u64,
}

/// The guide for this root: whether it can be used, and the conversation
/// so far. Starts Codex, if it is not running, to find out.
async fn guide_get(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<PageQuery>,
) -> ApiResult<serde_json::Value> {
    let mut answer = app.guide.conversation(&app, &root).snapshot(&query.page);
    answer["problem"] = app.guide.problem().await.into();
    answer["program"] = app.guide.program().into();
    Ok(Json(answer))
}

/// What happened in the conversation after event `after`; waits a while if
/// nothing has yet.
async fn guide_events(
    State(app): State<AppState>,
    Here(root): Here,
    Query(query): Query<PageQuery>,
) -> ApiResult<serde_json::Value> {
    let conversation = app.guide.conversation(&app, &root);
    Ok(Json(conversation.events(&query.page, query.after).await))
}

#[derive(Deserialize)]
struct GuideMessage {
    page: String,
    text: String,
    /// What the page shows, for Codex to know what "this" is.
    #[serde(default)]
    context: serde_json::Value,
    /// How hard Codex thinks; its own setting if left out.
    effort: Option<String>,
}

async fn guide_send(
    State(app): State<AppState>,
    Here(root): Here,
    Json(message): Json<GuideMessage>,
) -> ApiResult<serde_json::Value> {
    let text = message.text.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest("nothing to send".into()));
    }
    let effort = message
        .effort
        .as_deref()
        .filter(|effort| ["low", "medium", "high"].contains(effort));
    app.guide
        .conversation(&app, &root)
        .send(&message.page, text, &message.context, effort)
        .await
        .map_err(|err| ApiError::Conflict(format!("{err:#}")))?;
    Ok(Json(json!({})))
}

async fn guide_stop(State(app): State<AppState>, Here(root): Here) -> ApiResult<serde_json::Value> {
    app.guide.conversation(&app, &root).stop().await?;
    Ok(Json(json!({})))
}

/// Starts the conversation over.
async fn guide_reset(
    State(app): State<AppState>,
    Here(root): Here,
) -> ApiResult<serde_json::Value> {
    app.guide.conversation(&app, &root).reset().await;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct GuideAnswer {
    call: String,
    #[serde(default)]
    result: serde_json::Value,
    /// Why the page could not do what was asked.
    error: Option<String>,
}

/// The page's answer to something the guide asked of it.
async fn guide_answer(
    State(app): State<AppState>,
    Here(root): Here,
    Json(answer): Json<GuideAnswer>,
) -> ApiResult<serde_json::Value> {
    let result = match answer.error {
        Some(error) => Err(error),
        None => Ok(answer.result),
    };
    let taken = app
        .guide
        .conversation(&app, &root)
        .answer(&answer.call, result);
    Ok(Json(json!({ "taken": taken })))
}

#[derive(Deserialize)]
struct PageBody {
    page: String,
}

/// The page is closing.
async fn guide_leave(
    State(app): State<AppState>,
    Here(root): Here,
    Json(body): Json<PageBody>,
) -> ApiResult<serde_json::Value> {
    app.guide.conversation(&app, &root).leave(&body.page);
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(dir: &Path) -> Local {
        let store = store::Store::new(&dir.join("data"));
        let app = App::new(Jj::load().unwrap(), None, store).with_search_dirs(vec![dir.to_owned()]);
        Local::new(Arc::new(app))
    }

    async fn call(
        local: &Local,
        method: &str,
        uri: &str,
        root: Option<&Path>,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let root = root.map(|root| {
            percent_encoding::utf8_percent_encode(
                &root.display().to_string(),
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string()
        });
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let (status, text) = local.call(method, uri, root.as_deref(), body).await;
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn serves_contentts_modules_by_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let local = local(dir.path());
        let (status, body) = local
            .call("GET", "/contentts/contentts.js", None, String::new())
            .await;
        assert_eq!(status, 200);
        assert!(body.contains("export function mountContenTTS"));
        for uri in [
            "/contentts/nope.js",
            "/contentts/SOURCE",
            "/contentts/..%2Fapp.js",
            "/contentts/%2E%2E%2Fapp.js",
        ] {
            assert_eq!(
                local.call("GET", uri, None, String::new()).await.0,
                404,
                "{uri}"
            );
        }
    }

    /// A module added to contenTTS must be served, and copied into the app
    /// by desktop/electrobun.config.ts.
    #[test]
    fn contentts_lists_every_vendored_module() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/contentts");
        let mut vendored: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".js"))
            .collect();
        vendored.sort();
        let mut served: Vec<&str> = CONTENTTS.iter().map(|(name, _)| *name).collect();
        served.sort();
        assert_eq!(vendored, served);
        let config = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("desktop/electrobun.config.ts"),
        )
        .unwrap();
        for name in served {
            let stem = name.trim_end_matches(".js");
            assert!(
                config.contains(&format!("\"{stem}\"")),
                "desktop/electrobun.config.ts does not copy {name}"
            );
        }
    }

    /// Speech Mode's shortcut is contenTTS's default, and every place that
    /// names it says the same: the app's menu, the page's help, the READMEs.
    #[test]
    fn speech_mode_shortcut_is_the_same_everywhere() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
        let shortcut = read("web/contentts/shortcut.js");
        let key = shortcut
            .split("export const DEFAULT_KEY = \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("web/contentts/shortcut.js exports DEFAULT_KEY");
        assert!(
            key.len() == 1 && key.chars().all(|c| c.is_ascii_uppercase()),
            "{key}"
        );
        let mac = format!("⌘⇧{key}");
        let other = format!("Ctrl+Shift+{key}");
        for (path, expected) in [
            (
                "desktop/src/main.rs",
                vec![format!("\"accelerator\": \"CommandOrControl+Shift+{key}\"")],
            ),
            (
                "web/index.html",
                vec![
                    format!("({mac})"),
                    format!("<kbd>{mac}</kbd>"),
                    format!("({other} off a Mac)"),
                ],
            ),
            (
                "README.md",
                vec![
                    format!("page, {mac},"),
                    format!("| `{mac}` (`{other}` off a Mac) |"),
                ],
            ),
            (
                "desktop/README.md",
                vec![format!("Edit > Speech Mode ({mac})")],
            ),
        ] {
            let text = read(path);
            for want in expected {
                assert!(text.contains(&want), "{path} should say {want}");
            }
        }
    }

    #[tokio::test]
    async fn answers_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let local = local(dir.path());

        let body = json!({ "key": "theme", "value": "dark" }).to_string();
        assert_eq!(local.call("POST", "/api/settings", None, body).await.0, 200);
        let (status, body) = local
            .call("GET", "/api/settings", None, String::new())
            .await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["settings"]["theme"],
            "dark"
        );

        let (status, body) = local.call("POST", "/api/settings", None, "{".into()).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(
            local
                .call("GET", "/api/nothing", None, String::new())
                .await
                .0,
            404
        );
        assert_eq!(
            local.call("GET", "not a path", None, String::new()).await.0,
            400
        );
    }

    #[tokio::test]
    async fn starts_opens_and_remembers_roots() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let local = local(&base);
        let null = serde_json::Value::Null;

        let (status, start) = call(&local, "GET", "/api/start", None, null.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(start["root"], null);
        assert_eq!(start["chooser"], false);

        // A page must say which root it means, and mean a root.
        let (status, _) = call(&local, "GET", "/api/review/status", None, null.clone()).await;
        assert_eq!(status, 400);
        let fresh = base.join("fresh root");
        let (status, _) = call(&local, "GET", "/api/root", Some(&fresh), null.clone()).await;
        assert_eq!(status, 404);

        let path = fresh.display().to_string();
        let uri = format!(
            "/api/roots/inspect?path={}",
            percent_encoding::utf8_percent_encode(&path, percent_encoding::NON_ALPHANUMERIC)
        );
        let (_, seen) = call(&local, "GET", &uri, None, null.clone()).await;
        assert_eq!(
            (seen["kind"].as_str(), seen["creatable"].as_bool()),
            (Some("missing"), Some(true))
        );

        let (status, made) = call(
            &local,
            "POST",
            "/api/roots/init",
            None,
            json!({ "path": path }),
        )
        .await;
        assert_eq!(status, 200, "{made}");
        assert_eq!(made["root"], path);
        let (status, again) = call(
            &local,
            "POST",
            "/api/roots/init",
            None,
            json!({ "path": path }),
        )
        .await;
        assert_eq!(status, 409, "{again}");
        let (_, seen) = call(&local, "GET", &uri, None, null.clone()).await;
        assert_eq!(
            (seen["kind"].as_str(), seen["projj"].as_bool()),
            (Some("root"), Some(true))
        );

        let (status, root) = call(&local, "GET", "/api/root", Some(&fresh), null.clone()).await;
        assert_eq!(status, 200, "{root}");
        assert_eq!(
            (root["name"].as_str(), root["projj"].as_bool()),
            (Some("fresh root"), Some(true))
        );
        let (status, review) = call(
            &local,
            "GET",
            "/api/review/status",
            Some(&fresh),
            null.clone(),
        )
        .await;
        assert_eq!(status, 200, "{review}");
        // Only the top of a repository is a root.
        std::fs::create_dir_all(fresh.join("track/inner")).unwrap();
        let (status, err) = call(
            &local,
            "GET",
            "/api/root",
            Some(&fresh.join("track/inner")),
            null.clone(),
        )
        .await;
        assert_eq!(status, 404);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("the root around it is")
        );

        // Opened roots come first in a search, with nothing typed.
        let (_, found) = call(&local, "GET", "/api/roots?q=", None, null.clone()).await;
        assert_eq!(found["hits"][0]["path"], path);
        assert!(found["hits"][0]["opened"].is_i64());
        let (_, found) = call(
            &local,
            "GET",
            "/api/roots?q=nothing+like+it",
            None,
            null.clone(),
        )
        .await;
        assert_eq!(found["hits"], json!([]));
        let (status, _) = call(
            &local,
            "DELETE",
            &format!("/api/roots/recent?{}", &uri[uri.find('?').unwrap() + 1..]),
            None,
            null.clone(),
        )
        .await;
        assert_eq!(status, 200);

        // A typed path is completed, and what it names is described.
        let typed = format!(
            "/api/roots?q={}",
            percent_encoding::utf8_percent_encode(
                &format!("{}/fr", base.display()),
                percent_encoding::NON_ALPHANUMERIC
            )
        );
        let (_, found) = call(&local, "GET", &typed, None, null.clone()).await;
        assert_eq!(found["hits"][0]["path"], path);
        assert_eq!(found["hits"][0]["kind"], "projj");
        assert_eq!(found["typed"]["kind"], "missing");

        // There is no chooser to show here.
        let (status, _) = call(&local, "POST", "/api/roots/choose", None, json!({})).await;
        assert_eq!(status, 404);
    }
}
