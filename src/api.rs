//! HTTP surface: a JSON API under `/api` plus the embedded web client.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tower::ServiceExt as _;

use crate::files;
use crate::files::Eol;
use crate::files::FileError;
use crate::files::TextFile;
use crate::git::Git;
use crate::git::GitError;
use crate::git::Side;
use crate::git::Status;
use crate::git::StatusEntry;
use crate::jj::Jj;
use crate::jj::Session;
use crate::root;
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

/// Discovery asks git to walk the root, so its answer is reused for a moment
/// rather than recomputed for each of a page's requests.
const DISCOVERY_TTL: Duration = Duration::from_secs(2);

pub struct App {
    pub root: PathBuf,
    pub git: Git,
    pub jj: Jj,
    /// Serve the client from this directory instead of the embedded copy.
    pub web_dir: Option<PathBuf>,
    /// Unsaved editor text and settings, shared with every other server on
    /// the device.
    pub store: store::Store,
    /// What this root's drafts are filed under in that store.
    drafts_root: String,
    repos: Mutex<Option<(Instant, Arc<Vec<Repo>>)>>,
}

impl App {
    pub fn new(root: PathBuf, jj: Jj, web_dir: Option<PathBuf>, store: store::Store) -> Self {
        Self {
            git: Git::new(&root),
            drafts_root: store::root_key(&root),
            root,
            jj,
            web_dir,
            store,
            repos: Mutex::new(None),
        }
    }

    /// The jj repositories and workspaces currently under the root.
    async fn repos(self: &Arc<Self>) -> Result<Arc<Vec<Repo>>, ApiError> {
        if let Some((at, repos)) = &*self.repos.lock().unwrap()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(repos.clone());
        }
        let app = self.clone();
        let repos = tokio::task::spawn_blocking(move || root::discover(&app.root))
            .await
            .map_err(|err| ApiError::Internal(err.into()))??;
        let repos = Arc::new(repos);
        *self.repos.lock().unwrap() = Some((Instant::now(), repos.clone()));
        Ok(repos)
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
        .route("/api/root", get(get_root))
        .route("/api/review/status", get(review_status))
        .route("/api/review/diff", get(review_diff))
        .route("/api/review/apply", post(review_apply))
        .route("/api/review/apply-files", post(review_apply_files))
        .route("/api/file", get(file_read).post(file_write))
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

    /// Answers one request. Returns the status and the body as text.
    pub async fn call(&self, method: &str, uri: &str, body: String) -> (u16, String) {
        let request = match Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
        {
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

#[derive(Serialize)]
struct RootInfo {
    path: String,
    name: String,
    /// Which git answers: `installed` or `built-in`.
    git: crate::git::Engine,
    repos: Arc<Vec<Repo>>,
}

async fn get_root(State(app): State<AppState>) -> ApiResult<RootInfo> {
    Ok(Json(RootInfo {
        path: app.root.display().to_string(),
        name: app
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        git: app.git.engine(),
        repos: app.repos().await?,
    }))
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

async fn review_status(State(app): State<AppState>) -> ApiResult<ReviewStatus> {
    let Status {
        branch,
        head,
        entries,
    } = app.git.status().await?;
    let repos = app.repos().await?;
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
    State(app): State<AppState>,
    Query(query): Query<ReviewDiffQuery>,
) -> Result<Response, ApiError> {
    let context = context_or_default(query.context);
    let diff = app
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
    State(app): State<AppState>,
    Json(request): Json<ApplyRequest>,
) -> ApiResult<serde_json::Value> {
    let side = match request.action {
        Action::Stage => Side::Unstaged,
        Action::Unstage => Side::Staged,
    };
    app.git
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
    State(app): State<AppState>,
    Json(request): Json<ApplyFilesRequest>,
) -> ApiResult<serde_json::Value> {
    match request.action {
        Action::Stage => app.git.stage_paths(&request.paths).await?,
        Action::Unstage => app.git.unstage_paths(&request.paths).await?,
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct FileQuery {
    /// Path relative to the root.
    path: String,
}

async fn file_read(
    State(app): State<AppState>,
    Query(query): Query<FileQuery>,
) -> ApiResult<TextFile> {
    Ok(Json(files::read(&app.root, &query.path)?))
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
    State(app): State<AppState>,
    Json(request): Json<FileWrite>,
) -> ApiResult<serde_json::Value> {
    let version = files::write(
        &app.root,
        &request.path,
        &request.content,
        request.eol,
        request.base.as_deref(),
    )?;
    Ok(Json(json!({ "version": version })))
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
    State(app): State<AppState>,
    Json(request): Json<FileRebase>,
) -> ApiResult<files::Rebase> {
    Ok(Json(files::rebase(
        &app.root,
        &request.path,
        &request.base,
        &request.content,
    )?))
}

/// Lists the files an explorer should offer: everything git does not ignore.
async fn file_list(State(app): State<AppState>) -> ApiResult<serde_json::Value> {
    Ok(Json(json!({ "files": app.git.files().await? })))
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
async fn drafts_list(State(app): State<AppState>) -> ApiResult<serde_json::Value> {
    let drafts = app.store.drafts(&app.drafts_root).await?;
    Ok(Json(json!({
        "store": app.store.file().display().to_string(),
        "drafts": drafts,
    })))
}

/// Keeps the latest unsaved text of one file. Nothing is written to the
/// worktree, and the file need not exist any more.
async fn drafts_put(
    State(app): State<AppState>,
    Json(update): Json<store::Update>,
) -> ApiResult<serde_json::Value> {
    files::check_path(&update.path)?;
    app.store.put_draft(&app.drafts_root, update).await?;
    Ok(Json(json!({})))
}

async fn drafts_remove(
    State(app): State<AppState>,
    Query(query): Query<FileQuery>,
) -> ApiResult<serde_json::Value> {
    app.store
        .remove_draft(&app.drafts_root, &query.path)
        .await?;
    Ok(Json(json!({})))
}

/// Resolves the `ws` parameter to a discovered workspace directory. Only
/// paths found by discovery are accepted, so the value cannot point elsewhere.
async fn workspace_dir(app: &AppState, ws: &str) -> Result<PathBuf, ApiError> {
    let known = app
        .repos()
        .await?
        .iter()
        .flat_map(|repo| &repo.workspaces)
        .any(|workspace| workspace.path == ws);
    if known {
        Ok(app.root.join(ws))
    } else {
        Err(ApiError::NotFound(format!("unknown workspace: {ws}")))
    }
}

/// Runs a jj query on a blocking thread; `jj-lib` futures are not `Send`.
async fn with_session<T, F>(app: AppState, ws: String, query: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: AsyncFnOnce(&Session) -> anyhow::Result<T> + Send + 'static,
{
    let dir = workspace_dir(&app, &ws).await?;
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
    Query(query): Query<WorkspaceQuery>,
) -> ApiResult<crate::jj::WorkspaceInfo> {
    with_session(app, query.ws, async |session| session.info().await).await
}

#[derive(Deserialize)]
struct LogQuery {
    ws: String,
    revset: Option<String>,
    limit: Option<usize>,
}

async fn jj_log(
    State(app): State<AppState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<crate::jj::Log> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, query.ws, async move |session| {
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
    Query(query): Query<FileLogQuery>,
) -> ApiResult<crate::jj::Log> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, query.ws, async move |session| {
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
    Query(query): Query<DiffQuery>,
) -> ApiResult<crate::jj::DiffResult> {
    let context = context_or_default(query.context);
    with_session(app, query.ws, async move |session| {
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
    Query(query): Query<EvolutionQuery>,
) -> ApiResult<Vec<crate::jj::EvolutionEntry>> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, query.ws, async move |session| {
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
    Query(query): Query<OperationsQuery>,
) -> ApiResult<Vec<crate::jj::OperationInfo>> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    with_session(app, query.ws, async move |session| {
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
    State(app): State<AppState>,
    Query(query): Query<WorkspaceQuery>,
) -> ApiResult<serde_json::Value> {
    let dir = workspace_dir(&app, &query.ws).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path());
        let app = App::new(dir.path().to_owned(), Jj::load().unwrap(), None, store);
        let local = Local::new(Arc::new(app));

        let body = json!({ "key": "theme", "value": "dark" }).to_string();
        assert_eq!(local.call("POST", "/api/settings", body).await.0, 200);
        let (status, body) = local.call("GET", "/api/settings", String::new()).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["settings"]["theme"],
            "dark"
        );

        let (status, body) = local.call("POST", "/api/settings", "{".into()).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(
            local.call("GET", "/api/nothing", String::new()).await.0,
            404
        );
        assert_eq!(local.call("GET", "not a path", String::new()).await.0, 400);
    }
}
