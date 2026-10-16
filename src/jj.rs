//! Read-only access to jj history through `jj-lib`.
//!
//! Nothing here snapshots a working copy or writes an operation, so browsing
//! history can never race with an agent that is busy in the same workspace.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use bstr::BStr;
use futures::StreamExt as _;
use jj_lib::backend::CommitId;
use jj_lib::backend::Signature;
use jj_lib::backend::Timestamp;
use jj_lib::commit::Commit;
use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::config::StackedConfig;
use jj_lib::conflicts::ConflictMaterializeOptions;
use jj_lib::conflicts::materialize_tree_value;
use jj_lib::diff_presentation::LineCompareMode;
use jj_lib::diff_presentation::unified::git_diff_part;
use jj_lib::diff_presentation::unified::unified_diff_hunks;
use jj_lib::evolution::walk_predecessors;
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::fileset::FilesetExpression;
use jj_lib::git::REMOTE_NAME_FOR_LOCAL_GIT_REPO;
use jj_lib::graph::GraphEdgeType;
use jj_lib::graph::TopoGroupedGraph;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::id_prefix::IdPrefixIndex;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::matchers::Matcher;
use jj_lib::merge::Diff;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_walk;
use jj_lib::operation::Operation;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo::StoreFactories;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::revset;
use jj_lib::revset::ResolvedRevsetExpression;
use jj_lib::revset::RevsetAliasesMap;
use jj_lib::revset::RevsetContainingFn;
use jj_lib::revset::RevsetDiagnostics;
use jj_lib::revset::RevsetExpression;
use jj_lib::revset::RevsetExtensions;
use jj_lib::revset::RevsetFilterPredicate;
use jj_lib::revset::RevsetParseContext;
use jj_lib::revset::RevsetWorkspaceContext;
use jj_lib::revset::SymbolResolver;
use jj_lib::revset::UserRevsetExpression;
use jj_lib::settings::UserSettings;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::workspace::Workspace;
use jj_lib::workspace::default_working_copy_factories;
use serde::Serialize;

use crate::diff::FileDiff;
use crate::diff::FileStatus;
use crate::diff::LineKind;
use crate::diff::convert_hunk;
use crate::syntax;

/// Revset defaults that `jj-lib` leaves to its front end. They mirror the
/// `jj` CLI so expressions behave the same here as in a terminal.
const BUILTIN_CONFIG: &str = r#"
[revsets]
log = "present(@) | ancestors(immutable_heads().., 2) | present(trunk())"

[revset-aliases]
"trunk()" = '''
latest(
  remote_bookmarks(exact:"main", exact:"origin") |
  remote_bookmarks(exact:"master", exact:"origin") |
  remote_bookmarks(exact:"trunk", exact:"origin") |
  remote_bookmarks(exact:"main", exact:"upstream") |
  remote_bookmarks(exact:"master", exact:"upstream") |
  remote_bookmarks(exact:"trunk", exact:"upstream") |
  root()
)
'''
"builtin_immutable_heads()" = "present(trunk()) | tags() | untracked_remote_bookmarks()"
"immutable_heads()" = "builtin_immutable_heads()"
"immutable()" = "::(immutable_heads() | root())"
"mutable()" = "~immutable()"
"visible()" = "::visible_heads()"
"hidden()" = "~visible()"
"#;

/// Files beyond this many diff lines in one response are returned without
/// hunks and fetched individually by the client.
const DIFF_LINE_BUDGET: usize = 6000;
const MAX_DIFF_FILES: usize = 3000;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Time {
    /// Milliseconds since the Unix epoch.
    pub ms: i64,
    /// Offset from UTC in minutes.
    pub tz: i32,
}

impl From<Timestamp> for Time {
    fn from(timestamp: Timestamp) -> Self {
        Self {
            ms: timestamp.timestamp.0,
            tz: timestamp.tz_offset,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub time: Time,
}

impl From<&Signature> for Person {
    fn from(signature: &Signature) -> Self {
        Self {
            name: signature.name.clone(),
            email: signature.email.clone(),
            time: signature.timestamp.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitInfo {
    pub commit_id: String,
    pub change_id: String,
    /// Length of the shortest unambiguous prefix of each id.
    pub commit_prefix: usize,
    pub change_prefix: usize,
    pub description: String,
    pub author: Person,
    pub committer: Person,
    pub parents: Vec<String>,
    pub empty: bool,
    pub conflict: bool,
    pub hidden: bool,
    pub immutable: bool,
    pub root: bool,
    pub bookmarks: Vec<String>,
    pub remote_bookmarks: Vec<String>,
    pub tags: Vec<String>,
    /// Names of the workspaces whose working copy is this commit.
    pub working_copies: Vec<String>,
    /// This commit is the working copy of the workspace being browsed.
    pub current: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Edge {
    pub target: String,
    /// `direct`, `indirect` (commits elided in between), or `missing`.
    pub kind: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogNode {
    pub commit: CommitInfo,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Log {
    pub revset: String,
    pub nodes: Vec<LogNode>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperationInfo {
    pub id: String,
    pub description: String,
    pub start: Time,
    pub end: Time,
    pub user: String,
    pub snapshot: bool,
    pub workspace: Option<String>,
    pub attributes: Vec<(String, String)>,
}

impl From<&Operation> for OperationInfo {
    fn from(operation: &Operation) -> Self {
        let metadata = operation.metadata();
        Self {
            id: operation.id().hex(),
            description: metadata.description.clone(),
            start: metadata.time.start.into(),
            end: metadata.time.end.into(),
            user: format!("{}@{}", metadata.username, metadata.hostname),
            snapshot: metadata.is_snapshot,
            workspace: metadata
                .workspace_name
                .as_ref()
                .map(|name| name.as_str().to_owned()),
            attributes: metadata
                .attributes
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EvolutionEntry {
    pub commit: CommitInfo,
    /// Operation that created this version of the change, when recorded.
    pub operation: Option<OperationInfo>,
    pub predecessors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffResult {
    /// Resolved commit on the old side; `None` when diffing against parents.
    pub from: Option<CommitInfo>,
    pub to: CommitInfo,
    pub files: Vec<FileDiff>,
    /// More files changed than are listed.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub operation: OperationInfo,
    pub working_copy: Option<CommitInfo>,
    pub default_revset: String,
}

#[derive(Default)]
struct RefLabels {
    bookmarks: Vec<String>,
    remote_bookmarks: Vec<String>,
    tags: Vec<String>,
    working_copies: Vec<String>,
}

/// Process-wide jj configuration.
pub struct Jj {
    settings: UserSettings,
}

impl Jj {
    pub fn load() -> Result<Self> {
        let mut config = StackedConfig::with_defaults();
        config.add_layer(ConfigLayer::parse(ConfigSource::Default, BUILTIN_CONFIG)?);
        for path in user_config_paths() {
            if path.is_file() {
                config
                    .load_file(ConfigSource::User, &path)
                    .with_context(|| format!("cannot load jj config {}", path.display()))?;
            }
        }
        let settings = UserSettings::from_config(config)?;
        Ok(Self { settings })
    }

    /// Loads the workspace at `root` as of the repository's latest operation.
    pub fn open(&self, root: &Path) -> Result<Session> {
        let workspace = Workspace::load(
            &self.settings,
            root,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .with_context(|| format!("cannot load jj workspace {}", root.display()))?;
        let repo = pollster::block_on(workspace.repo_loader().load_at_head())?;
        Session::new(&self.settings, workspace, repo)
    }
}

fn user_config_paths() -> Vec<PathBuf> {
    if let Some(paths) = std::env::var_os("JJ_CONFIG") {
        return std::env::split_paths(&paths).collect();
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return vec![];
    };
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    vec![
        home.join(".jjconfig.toml"),
        config_home.join("jj").join("config.toml"),
    ]
}

/// One workspace, pinned to a single operation for the length of a request.
pub struct Session {
    settings: UserSettings,
    workspace: Workspace,
    repo: Arc<ReadonlyRepo>,
    aliases: RevsetAliasesMap,
    fileset_aliases: FilesetAliasesMap,
    extensions: Arc<RevsetExtensions>,
    path_converter: RepoPathUiConverter,
    id_prefix: IdPrefixContext,
    labels: HashMap<CommitId, RefLabels>,
    log_revset: String,
}

/// Fills in the parts of [`CommitInfo`] that need repo-wide indexes.
struct Describer<'a> {
    session: &'a Session,
    prefixes: IdPrefixIndex<'a>,
    immutable: Option<Box<RevsetContainingFn<'a>>>,
}

impl Session {
    fn new(settings: &UserSettings, workspace: Workspace, repo: Arc<ReadonlyRepo>) -> Result<Self> {
        let mut aliases = RevsetAliasesMap::new();
        let names: Vec<String> = settings
            .table_keys("revset-aliases")
            .map(str::to_owned)
            .collect();
        for name in names {
            if let Ok(definition) = settings.get_string(["revset-aliases", name.as_str()]) {
                // A malformed alias only matters if an expression uses it.
                let _ = aliases.insert(&name, definition, None);
            }
        }
        let path_converter = RepoPathUiConverter::Fs {
            cwd: workspace.workspace_root().to_owned(),
            base: workspace.workspace_root().to_owned(),
        };
        let mut session = Self {
            settings: settings.clone(),
            log_revset: settings.get_string("revsets.log")?,
            labels: ref_labels(&repo),
            workspace,
            repo,
            aliases,
            fileset_aliases: FilesetAliasesMap::new(),
            extensions: Arc::new(RevsetExtensions::new()),
            path_converter,
            id_prefix: IdPrefixContext::new(Arc::new(RevsetExtensions::new())),
        };
        // Like the CLI, keep id prefixes short by disambiguating within the
        // commits a person is likely to be looking at.
        let short_prefixes = settings
            .get_string("revsets.short-prefixes")
            .unwrap_or_else(|_| session.log_revset.clone());
        if let Ok(expression) = session.parse(&short_prefixes) {
            session.id_prefix =
                IdPrefixContext::new(session.extensions.clone()).disambiguate_within(expression);
        }
        Ok(session)
    }

    fn parse_context(&self) -> RevsetParseContext<'_> {
        RevsetParseContext {
            aliases_map: &self.aliases,
            local_variables: HashMap::new(),
            user_email: self.settings.user_email(),
            date_pattern_context: chrono::Local::now().into(),
            default_ignored_remote: Some(REMOTE_NAME_FOR_LOCAL_GIT_REPO),
            fileset_aliases_map: &self.fileset_aliases,
            extensions: &self.extensions,
            workspace: Some(RevsetWorkspaceContext {
                path_converter: &self.path_converter,
                workspace_name: self.workspace.workspace_name(),
            }),
        }
    }

    fn parse(&self, text: &str) -> Result<Arc<UserRevsetExpression>> {
        let mut diagnostics = RevsetDiagnostics::new();
        revset::parse(&mut diagnostics, text, &self.parse_context())
            .map_err(|err| anyhow!("invalid revset: {err}"))
    }

    fn resolve(&self, text: &str) -> Result<Arc<ResolvedRevsetExpression>> {
        let expression = self.parse(text)?;
        let resolver = SymbolResolver::new(self.repo.as_ref(), self.extensions.symbol_resolvers())
            .with_id_prefix_context(&self.id_prefix);
        expression
            .resolve_user_expression(self.repo.as_ref(), &resolver)
            .map_err(|err| anyhow!("cannot resolve revset: {err}"))
    }

    /// Resolves an expression that must name exactly one commit.
    async fn resolve_single(&self, text: &str) -> Result<Commit> {
        let store = self.repo.store();
        // Full commit ids stay addressable after the commit becomes hidden,
        // which is how predecessors in an evolution log are reached.
        if text.len() == 40
            && let Some(id) = CommitId::try_from_hex(text)
            && let Ok(commit) = store.get_commit_async(&id).await
        {
            return Ok(commit);
        }
        let revset = self.resolve(text)?.evaluate(self.repo.as_ref())?;
        let mut ids = revset.stream().take(2).collect::<Vec<_>>().await;
        match ids.len() {
            0 => bail!("revset {text:?} is empty"),
            1 => Ok(store.get_commit_async(&ids.remove(0)?).await?),
            _ => bail!("revset {text:?} resolves to more than one commit"),
        }
    }

    fn describer(&self) -> Result<Describer<'_>> {
        let prefixes = self
            .id_prefix
            .populate(self.repo.as_ref())
            .map_err(|err| anyhow!("cannot index id prefixes: {err}"))?;
        // An unusual `immutable_heads()` override must not break browsing.
        let immutable = self
            .resolve("immutable()")
            .ok()
            .and_then(|expression| expression.evaluate(self.repo.as_ref()).ok())
            .map(|revset| revset.containing_fn());
        Ok(Describer {
            session: self,
            prefixes,
            immutable,
        })
    }

    pub async fn info(&self) -> Result<WorkspaceInfo> {
        let name = self.workspace.workspace_name();
        let working_copy = match self.repo.view().get_wc_commit_id(name) {
            Some(id) => {
                let commit = self.repo.store().get_commit_async(id).await?;
                Some(self.describer()?.describe(&commit).await?)
            }
            None => None,
        };
        Ok(WorkspaceInfo {
            name: name.as_str().to_owned(),
            operation: self.repo.operation().into(),
            working_copy,
            default_revset: self.log_revset.clone(),
        })
    }

    pub async fn log(&self, revset: Option<&str>, limit: usize) -> Result<Log> {
        let text = revset
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(&self.log_revset);
        let expression = self.resolve(text)?;
        self.log_expression(text, expression, limit).await
    }

    /// History of the commits that changed `path` (relative to the workspace).
    pub async fn file_log(&self, path: &str, limit: usize) -> Result<Log> {
        let repo_path = RepoPathBuf::from_internal_string(path)
            .map_err(|err| anyhow!("invalid path {path:?}: {err}"))?;
        let touches = RevsetFilterPredicate::File(FilesetExpression::file_path(repo_path));
        let expression = RevsetExpression::all().filtered(touches);
        let text = format!("files({})", revset::format_string(path));
        self.log_expression(&text, expression, limit).await
    }

    async fn log_expression(
        &self,
        text: &str,
        expression: Arc<ResolvedRevsetExpression>,
        limit: usize,
    ) -> Result<Log> {
        let repo = self.repo.as_ref();
        let revset = expression.evaluate(repo)?;
        let mut grouped = TopoGroupedGraph::new(revset.stream_graph(), |id| id);
        // Show the branch being worked on first, as the CLI does.
        let has_commit = revset.containing_fn();
        if let Some(id) = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            && has_commit(id)?
        {
            grouped.prioritize_branch(id.clone());
        }
        let describer = self.describer()?;
        let mut stream = pin!(grouped.stream());
        let mut nodes = vec![];
        let mut has_more = false;
        while let Some(node) = stream.next().await {
            let (id, edges) = node?;
            if nodes.len() >= limit {
                has_more = true;
                break;
            }
            let commit = repo.store().get_commit_async(&id).await?;
            nodes.push(LogNode {
                commit: describer.describe(&commit).await?,
                edges: edges
                    .into_iter()
                    .map(|edge| Edge {
                        target: edge.target.hex(),
                        kind: match edge.edge_type {
                            GraphEdgeType::Direct => "direct",
                            GraphEdgeType::Indirect => "indirect",
                            GraphEdgeType::Missing => "missing",
                        },
                    })
                    .collect(),
            });
        }
        Ok(Log {
            revset: text.to_owned(),
            nodes,
            has_more,
        })
    }

    /// How the change at `rev` was rewritten over time, newest first.
    pub async fn evolution(&self, rev: &str, limit: usize) -> Result<Vec<EvolutionEntry>> {
        let commit = self.resolve_single(rev).await?;
        let describer = self.describer()?;
        let mut stream = pin!(walk_predecessors(&self.repo, &[commit.id().clone()]));
        let mut entries = vec![];
        while let Some(entry) = stream.next().await {
            let entry = entry.map_err(|err| anyhow!("cannot walk predecessors: {err}"))?;
            entries.push(EvolutionEntry {
                commit: describer.describe(&entry.commit).await?,
                operation: entry.operation.as_ref().map(Into::into),
                predecessors: entry.predecessor_ids().iter().map(|id| id.hex()).collect(),
            });
            if entries.len() >= limit {
                break;
            }
        }
        Ok(entries)
    }

    pub async fn operations(&self, limit: usize) -> Result<Vec<OperationInfo>> {
        let head = self.repo.operation().clone();
        let mut stream = pin!(op_walk::walk_ancestors(&[head]));
        let mut operations = vec![];
        while let Some(operation) = stream.next().await {
            operations.push((&operation?).into());
            if operations.len() >= limit {
                break;
            }
        }
        Ok(operations)
    }

    /// Diff of the commit at `to` against its parents, or against `from`.
    /// With `path`, only that file is returned and always in full.
    pub async fn diff(
        &self,
        from: Option<&str>,
        to: &str,
        path: Option<&str>,
        context: usize,
    ) -> Result<DiffResult> {
        let repo = self.repo.as_ref();
        let to_commit = self.resolve_single(to).await?;
        let from_commit = match from.map(str::trim).filter(|text| !text.is_empty()) {
            Some(text) => Some(self.resolve_single(text).await?),
            None => None,
        };
        let from_tree = match &from_commit {
            Some(commit) => commit.tree(),
            None => to_commit.parent_tree(repo).await?,
        };
        let matcher: Box<dyn Matcher> = match path {
            Some(path) => {
                let repo_path = RepoPathBuf::from_internal_string(path)
                    .map_err(|err| anyhow!("invalid path {path:?}: {err}"))?;
                FilesetExpression::file_path(repo_path).to_matcher()
            }
            None => Box::new(EverythingMatcher),
        };
        let budget = path.is_none().then_some(DIFF_LINE_BUDGET);
        let (files, truncated) = self
            .diff_trees(
                &from_tree,
                &to_commit.tree(),
                matcher.as_ref(),
                context,
                budget,
            )
            .await?;
        let describer = self.describer()?;
        Ok(DiffResult {
            from: match &from_commit {
                Some(commit) => Some(describer.describe(commit).await?),
                None => None,
            },
            to: describer.describe(&to_commit).await?,
            files,
            truncated,
        })
    }

    async fn diff_trees(
        &self,
        from: &MergedTree,
        to: &MergedTree,
        matcher: &dyn Matcher,
        context: usize,
        mut budget: Option<usize>,
    ) -> Result<(Vec<FileDiff>, bool)> {
        let store = self.repo.store();
        let options = ConflictMaterializeOptions {
            marker_style: self.settings.get("ui.conflict-marker-style")?,
            marker_len: None,
            merge: MergeOptions::from_settings(&self.settings)?,
        };
        let mut files = vec![];
        let mut stream = from.diff_stream(to, matcher);
        while let Some(entry) = stream.next().await {
            if files.len() >= MAX_DIFF_FILES {
                return Ok((files, true));
            }
            let path = entry.path;
            let values = entry.values?;
            let before = materialize_tree_value(store, &path, values.before, from.labels()).await?;
            let after = materialize_tree_value(store, &path, values.after, to.labels()).await?;
            let left = git_diff_part(&path, before, &options).await?;
            let right = git_diff_part(&path, after, &options).await?;
            let status = match (left.mode, right.mode) {
                (None, None) => continue,
                (None, Some(_)) => FileStatus::Added,
                (Some(_), None) => FileStatus::Deleted,
                (Some(_), Some(_)) => FileStatus::Modified,
            };
            let mut file = FileDiff {
                path: path.as_internal_file_string().to_owned(),
                status,
                old_mode: left.mode.map(str::to_owned),
                new_mode: right.mode.map(str::to_owned),
                binary: left.content.is_binary || right.content.is_binary,
                note: None,
                added: 0,
                removed: 0,
                hunks: vec![],
                deferred: false,
                header: vec![],
            };
            if !file.binary {
                let contents = Diff::new(
                    BStr::new(&left.content.contents),
                    BStr::new(&right.content.contents),
                );
                let hunks = unified_diff_hunks(contents, context, LineCompareMode::Exact);
                file.hunks = hunks.iter().map(convert_hunk).collect();
                for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
                    match line.kind {
                        LineKind::Added => file.added += 1,
                        LineKind::Removed => file.removed += 1,
                        LineKind::Context => {}
                    }
                }
            }
            if let Some(budget) = &mut budget {
                let size: usize = file.hunks.iter().map(|hunk| hunk.lines.len()).sum();
                if size > *budget {
                    file.hunks.clear();
                    file.deferred = true;
                    *budget = 0;
                } else {
                    *budget -= size;
                }
            }
            if !file.hunks.is_empty() {
                let old = syntax::highlight(&file.path, &left.content.contents);
                let new = syntax::highlight(&file.path, &right.content.contents);
                syntax::apply(&mut file.hunks, old.as_deref(), new.as_deref());
            }
            files.push(file);
        }
        Ok((files, false))
    }
}

impl Describer<'_> {
    async fn describe(&self, commit: &Commit) -> Result<CommitInfo> {
        let session = self.session;
        let repo = session.repo.as_ref();
        let id = commit.id();
        let root = id == repo.store().root_commit_id();
        let labels = session.labels.get(id);
        let label =
            |pick: fn(&RefLabels) -> &Vec<String>| labels.map(pick).cloned().unwrap_or_default();
        let working_copies = label(|labels| &labels.working_copies);
        Ok(CommitInfo {
            commit_id: id.hex(),
            change_id: commit.change_id().reverse_hex(),
            commit_prefix: self.prefixes.shortest_commit_prefix_len(repo, id)?,
            change_prefix: self
                .prefixes
                .shortest_change_prefix_len(repo, commit.change_id())?,
            description: commit.description().to_owned(),
            author: commit.author().into(),
            committer: commit.committer().into(),
            parents: commit.parent_ids().iter().map(|id| id.hex()).collect(),
            empty: commit.is_empty(repo).await?,
            conflict: commit.has_conflict(),
            hidden: commit.is_hidden(repo)?,
            immutable: match &self.immutable {
                Some(contains) => contains(id)?,
                None => root,
            },
            root,
            bookmarks: label(|labels| &labels.bookmarks),
            remote_bookmarks: label(|labels| &labels.remote_bookmarks),
            tags: label(|labels| &labels.tags),
            current: working_copies
                .iter()
                .any(|name| name == session.workspace.workspace_name().as_str()),
            working_copies,
        })
    }
}

fn ref_labels(repo: &ReadonlyRepo) -> HashMap<CommitId, RefLabels> {
    let view = repo.view();
    let mut labels: HashMap<CommitId, RefLabels> = HashMap::new();
    for (name, target) in view.local_bookmarks() {
        let suffix = if target.has_conflict() { "??" } else { "" };
        for id in target.added_ids() {
            let entry = labels.entry(id.clone()).or_default();
            entry.bookmarks.push(format!("{}{suffix}", name.as_str()));
        }
    }
    for (symbol, remote_ref) in view.all_remote_bookmarks() {
        if symbol.remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO {
            continue;
        }
        for id in remote_ref.target.added_ids() {
            let entry = labels.entry(id.clone()).or_default();
            // A remote bookmark in sync with its local one adds no information.
            if entry
                .bookmarks
                .iter()
                .any(|local| local == symbol.name.as_str())
            {
                continue;
            }
            entry.remote_bookmarks.push(format!(
                "{}@{}",
                symbol.name.as_str(),
                symbol.remote.as_str()
            ));
        }
    }
    for (name, target) in view.local_tags() {
        for id in target.added_ids() {
            let entry = labels.entry(id.clone()).or_default();
            entry.tags.push(name.as_str().to_owned());
        }
    }
    for (name, id) in view.wc_commit_ids() {
        let entry = labels.entry(id.clone()).or_default();
        entry.working_copies.push(name.as_str().to_owned());
    }
    labels
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Builds the scratch root from `scripts/make-scratch-root.sh` and returns
    /// its `sts-0` workspace, or `None` where the `jj` executable is missing.
    fn scratch_workspace() -> Option<(tempfile::TempDir, PathBuf)> {
        if Command::new("jj").arg("--version").output().is_err() {
            eprintln!("skipping: jj is not installed");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/make-scratch-root.sh");
        let output = Command::new("bash")
            .arg(script)
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let workspace = root.join("track/example.com/acme/widgets.git/sts-0");
        Some((dir, workspace))
    }

    #[test]
    fn reads_history_of_a_scratch_workspace() {
        let Some((_dir, workspace)) = scratch_workspace() else {
            return;
        };
        let session = Jj::load().unwrap().open(&workspace).unwrap();
        pollster::block_on(async {
            let info = session.info().await.unwrap();
            assert_eq!(info.name, "sts-0");
            assert_eq!(info.working_copy.unwrap().description, "Add later()\n");

            let log = session.log(Some("::@ ~ root()"), 10).await.unwrap();
            let titles: Vec<_> = log
                .nodes
                .iter()
                .map(|node| node.commit.description.trim().to_owned())
                .collect();
            assert_eq!(
                titles,
                [
                    "Add later()",
                    "Widen step 2 and add new module",
                    "Add widget steps and guide"
                ]
            );
            assert!(log.nodes[0].commit.current);
            assert_eq!(log.nodes[0].edges[0].target, log.nodes[1].commit.commit_id);
            assert_eq!(log.nodes[0].edges[0].kind, "direct");
            assert!(!log.has_more);
            assert!(session.log(Some("all()"), 2).await.unwrap().has_more);
            assert!(session.log(Some("nope("), 10).await.is_err());

            // The change's own diff, addressed by change id prefix.
            let middle = &log.nodes[1].commit;
            let prefix = &middle.change_id[..middle.change_prefix];
            let diff = session.diff(None, prefix, None, 3).await.unwrap();
            let paths: Vec<_> = diff.files.iter().map(|file| file.path.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "docs/guide.md",
                    "notes.txt",
                    "src/lib.rs",
                    "src/new_module.rs"
                ]
            );
            assert_eq!(diff.files[1].status, FileStatus::Deleted);
            assert_eq!(diff.files[3].status, FileStatus::Added);
            let guide = &diff.files[0];
            assert_eq!((guide.added, guide.removed), (1, 1));
            let added = guide.hunks[0]
                .lines
                .iter()
                .find(|line| line.kind == LineKind::Added)
                .unwrap();
            assert_eq!(added.new, Some(7));
            let [start, end] = added.hl.as_ref().unwrap()[0];
            assert_eq!(&added.text[start..end], ", inspected,");
            let rust = &diff.files[2].hunks[0].lines;
            let signature = rust
                .iter()
                .find(|line| line.text == "pub fn step_2() -> u64 {")
                .unwrap();
            let classes: Vec<_> = signature.syn.iter().flatten().map(|span| span.2).collect();
            // `pub`, `fn`, the name, and the primitive return type.
            assert_eq!(classes, ["k", "k", "f", "k"]);

            // Across two changes, limited to one file.
            let oldest = &log.nodes[2].commit.commit_id;
            let range = session
                .diff(Some(oldest), "@", Some("src/new_module.rs"), 3)
                .await
                .unwrap();
            assert_eq!(range.files.len(), 1);
            assert_eq!(range.files[0].added, 6);

            let touched = session.file_log("src/new_module.rs", 10).await.unwrap();
            assert_eq!(touched.nodes.len(), 2);
            assert_eq!(touched.revset, r#"files("src/new_module.rs")"#);

            let evolution = session.evolution(&middle.commit_id, 10).await.unwrap();
            assert_eq!(evolution[0].commit.commit_id, middle.commit_id);
            assert!(evolution.len() > 1);
            assert!(evolution[1].commit.hidden);
            assert_eq!(
                evolution[0].predecessors,
                [evolution[1].commit.commit_id.clone()]
            );

            let operations = session.operations(3).await.unwrap();
            assert_eq!(operations.len(), 3);
            assert_eq!(operations[0].workspace.as_deref(), Some("sts-0"));
        });
    }
}
