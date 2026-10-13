//! Review state lives in the root git repository's index: a hunk that is
//! staged has been reviewed, a hunk that is not staged has not. This module
//! drives the `git` executable so results always agree with what the human
//! sees in their own terminal. On a machine with no git, the built-in one
//! (`git_builtin`) answers the same questions instead.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::diff::FileDiff;
use crate::diff::FileStatus;
use crate::diff::Hunk;
use crate::diff::Line;
use crate::diff::LineKind;
use crate::diff::highlight_hunk;
use crate::syntax;

/// Untracked files larger than this are listed without line counts.
const MAX_COUNTED_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Line counts are computed for at most this many untracked files.
const MAX_COUNTED_UNTRACKED: usize = 2000;
const MAX_PATHS_PER_COMMAND: usize = 200;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("{0}")]
    Invalid(String),
    /// The repository changed since the client last looked.
    #[error("{0}")]
    Stale(String),
    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },
    #[error("cannot run git: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, GitError>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Worktree against index: not yet reviewed.
    Unstaged,
    /// Index against `HEAD`: reviewed, awaiting commit.
    Staged,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stat {
    /// `None` when the content is binary or too large to count.
    pub added: Option<usize>,
    pub removed: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusEntry {
    pub path: String,
    /// Porcelain status letter for index vs `HEAD` (`.` when unchanged).
    pub index: char,
    /// Porcelain status letter for worktree vs index (`?` when untracked).
    pub worktree: char,
    pub untracked: bool,
    pub conflicted: bool,
    /// An untracked directory git will not descend into (a nested repository).
    pub directory: bool,
    pub staged: Option<Stat>,
    pub unstaged: Option<Stat>,
    /// When the worktree file was last written, in milliseconds since the
    /// Unix epoch. Lets a client notice edits that leave the counts alone.
    pub modified: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub entries: Vec<StatusEntry>,
}

/// Which git answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// The `git` program installed on the machine.
    Installed,
    /// projj's own, for machines without one.
    BuiltIn,
}

impl Engine {
    /// The installed git when there is one that runs, else the built-in
    /// one. `PROJJ_GIT=installed` or `PROJJ_GIT=built-in` decides instead.
    pub fn detect() -> Engine {
        static ENGINE: OnceLock<Engine> = OnceLock::new();
        *ENGINE.get_or_init(|| match std::env::var("PROJJ_GIT").as_deref() {
            Ok("built-in") => Engine::BuiltIn,
            Ok("installed") => Engine::Installed,
            _ if git_is_installed() => Engine::Installed,
            _ => Engine::BuiltIn,
        })
    }

    pub fn describe(self) -> &'static str {
        match self {
            Engine::Installed => "the installed git",
            Engine::BuiltIn => "projj's built-in git",
        }
    }
}

/// Whether a `git` on `PATH` runs. On a Mac without the developer tools,
/// `/usr/bin/git` is only a stub that offers to install them, so it is not
/// even started then.
fn git_is_installed() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let Some(git) = std::env::split_paths(&path)
        .map(|dir| dir.join(if cfg!(windows) { "git.exe" } else { "git" }))
        .find(|candidate| candidate.is_file())
    else {
        return false;
    };
    let quiet = |program: &Path, arg: &str| {
        std::process::Command::new(program)
            .arg(arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if cfg!(target_os = "macos")
        && git == Path::new("/usr/bin/git")
        && !quiet(Path::new("/usr/bin/xcode-select"), "-p")
    {
        return false;
    }
    quiet(&git, "--version")
}

/// Runs a call to the built-in git, which blocks, off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(std::io::Error::other)?
}

#[derive(Clone, Debug)]
pub struct Git {
    root: PathBuf,
    engine: Engine,
}

impl Git {
    pub fn new(root: &Path) -> Self {
        Self::with_engine(root, Engine::detect())
    }

    pub fn with_engine(root: &Path, engine: Engine) -> Self {
        Self {
            root: root.to_owned(),
            engine,
        }
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    fn command(&self, read_only: bool) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["--no-pager", "--literal-pathspecs"])
            .args(["-c", "core.quotePath=false"])
            .args(["-c", "color.ui=false"])
            .args(["-c", "diff.noprefix=false"])
            .args(["-c", "diff.mnemonicPrefix=false"])
            .args(["-c", "diff.relative=false"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if read_only {
            // Never take index.lock just to refresh stat info; the human or an
            // agent may be running git at the same moment.
            command.env("GIT_OPTIONAL_LOCKS", "0");
        }
        command
    }

    async fn run(
        &self,
        read_only: bool,
        args: &[&str],
        stdin: Option<&[u8]>,
        ok_codes: &[i32],
    ) -> Result<Vec<u8>> {
        let mut command = self.command(read_only);
        command.args(args);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn()?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin was piped");
            pipe.write_all(input).await?;
            pipe.shutdown().await?;
        }
        let output = child.wait_with_output().await?;
        let code = output.status.code().unwrap_or(-1);
        if !ok_codes.contains(&code) {
            return Err(GitError::Failed {
                command: args.first().copied().unwrap_or_default().to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }

    async fn read(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run(true, args, None, &[0]).await
    }

    pub async fn status(&self) -> Result<Status> {
        let mut status = match self.engine {
            Engine::Installed => {
                let out = self
                    .read(&[
                        "status",
                        "--porcelain=v2",
                        "--branch",
                        "-z",
                        "--untracked-files=all",
                        "--no-renames",
                    ])
                    .await?;
                parse_status(&out)
            }
            Engine::BuiltIn => {
                let root = self.root.clone();
                blocking(move || crate::git_builtin::status(&root)).await?
            }
        };
        status.entries.sort_by(listing_order);
        let unstaged = self.numstat(false).await?;
        let staged = self.numstat(true).await?;
        let mut counted = 0;
        for entry in &mut status.entries {
            if entry.directory {
                continue;
            }
            if entry.worktree != '.' {
                entry.modified = std::fs::symlink_metadata(self.root.join(&entry.path))
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| since.as_millis() as u64);
            }
            if entry.untracked {
                entry.unstaged = Some(if counted < MAX_COUNTED_UNTRACKED {
                    counted += 1;
                    count_new_file(&self.root.join(&entry.path))
                } else {
                    Stat::default()
                });
                continue;
            }
            if entry.index != '.' {
                entry.staged = Some(staged.get(&entry.path).cloned().unwrap_or_default());
            }
            if entry.worktree != '.' {
                entry.unstaged = Some(unstaged.get(&entry.path).cloned().unwrap_or_default());
            }
        }
        Ok(status)
    }

    async fn numstat(&self, cached: bool) -> Result<HashMap<String, Stat>> {
        if self.engine == Engine::BuiltIn {
            let root = self.root.clone();
            return blocking(move || crate::git_builtin::numstat(&root, cached)).await;
        }
        let mut args = vec!["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff"];
        if cached {
            args.push("--cached");
        }
        let out = self.read(&args).await?;
        let mut stats = HashMap::new();
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = String::from_utf8_lossy(record);
            let mut fields = record.splitn(3, '\t');
            let (Some(added), Some(removed), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            stats.insert(
                path.to_owned(),
                Stat {
                    added: added.parse().ok(),
                    removed: removed.parse().ok(),
                },
            );
        }
        Ok(stats)
    }

    /// Diff of one file on one side of the index, or `None` if that side has
    /// no change for the path.
    pub async fn file_diff(
        &self,
        path: &str,
        side: Side,
        context: usize,
    ) -> Result<Option<FileDiff>> {
        validate_path(path)?;
        let out = self.diff_bytes(path, side, context).await?;
        if out.is_empty() {
            return Ok(None);
        }
        Ok(Some(parse_diff(path, &out)))
    }

    /// What `git diff` prints for one path on one side, with untracked files
    /// diffed against nothing.
    pub(crate) async fn diff_bytes(
        &self,
        path: &str,
        side: Side,
        context: usize,
    ) -> Result<Vec<u8>> {
        match self.engine {
            Engine::Installed => self.diff_output(path, side, context).await,
            Engine::BuiltIn => {
                let (root, path) = (self.root.clone(), path.to_owned());
                blocking(move || crate::git_builtin::diff(&root, &path, side, context)).await
            }
        }
    }

    async fn diff_output(&self, path: &str, side: Side, context: usize) -> Result<Vec<u8>> {
        let unified = format!("-U{context}");
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--full-index",
            &unified,
        ];
        if side == Side::Staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        let mut out = self.read(&args).await?;
        if out.is_empty() && side == Side::Unstaged && self.is_untracked(path).await? {
            // `git diff` ignores untracked files; diff against nothing instead.
            // Exit status 1 only means "there are differences".
            out = self
                .run(
                    true,
                    &[
                        "diff",
                        "--no-index",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--full-index",
                        &unified,
                        "--",
                        "/dev/null",
                        path,
                    ],
                    None,
                    &[0, 1],
                )
                .await?;
        }
        Ok(out)
    }

    /// Like [`Git::file_diff`], with syntax tokens on every line.
    pub async fn highlighted_file_diff(
        &self,
        path: &str,
        side: Side,
        context: usize,
    ) -> Result<Option<FileDiff>> {
        let Some(mut file) = self.file_diff(path, side, context).await? else {
            return Ok(None);
        };
        if file.hunks.is_empty() {
            return Ok(Some(file));
        }
        // Each side of the diff is tokenized as a whole file.
        let index = format!(":{path}");
        let head = format!("HEAD:{path}");
        let (old, new) = match side {
            Side::Unstaged => (self.blob(&index).await, self.worktree_file(path)),
            Side::Staged => (self.blob(&head).await, self.blob(&index).await),
        };
        let path = path.to_owned();
        let file = tokio::task::spawn_blocking(move || {
            let tokens = |content: Option<Vec<u8>>| syntax::highlight(&path, &content?);
            let (old, new) = (tokens(old), tokens(new));
            syntax::apply(&mut file.hunks, old.as_deref(), new.as_deref());
            file
        })
        .await
        .map_err(std::io::Error::other)?;
        Ok(Some(file))
    }

    /// Content of a blob named by a revision such as `:path` or `HEAD:path`,
    /// or `None` if there is no such blob.
    pub(crate) async fn blob(&self, revision: &str) -> Option<Vec<u8>> {
        match self.engine {
            Engine::Installed => self.read(&["cat-file", "blob", revision]).await.ok(),
            Engine::BuiltIn => {
                let (root, revision) = (self.root.clone(), revision.to_owned());
                blocking(move || Ok(crate::git_builtin::blob(&root, &revision)))
                    .await
                    .ok()
                    .flatten()
            }
        }
    }

    fn worktree_file(&self, path: &str) -> Option<Vec<u8>> {
        let file = self.root.join(path);
        let regular = std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file());
        regular.then(|| std::fs::read(&file).ok()).flatten()
    }

    /// Every file in the worktree that git does not ignore, tracked or not,
    /// in path order. A nested repository appears as its directory, ending
    /// in `/`, because git does not look inside one.
    pub async fn files(&self) -> Result<Vec<String>> {
        if self.engine == Engine::BuiltIn {
            let root = self.root.clone();
            return blocking(move || crate::git_builtin::files(&root)).await;
        }
        let listed = self
            .read(&[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .await?;
        // Tracked files that were deleted are still in the index.
        let deleted = self.read(&["ls-files", "-z", "--deleted"]).await?;
        let records = |out: &[u8]| -> Vec<String> {
            out.split(|byte| *byte == 0)
                .filter(|record| !record.is_empty())
                .map(|record| String::from_utf8_lossy(record).into_owned())
                .collect()
        };
        let deleted: HashSet<String> = records(&deleted).into_iter().collect();
        let mut files = records(&listed);
        files.retain(|path| !deleted.contains(path));
        files.sort();
        files.dedup();
        Ok(files)
    }

    async fn is_untracked(&self, path: &str) -> Result<bool> {
        let out = self
            .read(&[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                path,
            ])
            .await?;
        Ok(out.strip_suffix(b"\0") == Some(path.as_bytes()))
    }

    /// Stages (marks reviewed) or unstages one hunk, or only the changed lines
    /// of it selected by index into the hunk's lines.
    pub async fn apply_hunk(
        &self,
        path: &str,
        side: Side,
        context: usize,
        hunk_id: &str,
        lines: Option<&[usize]>,
    ) -> Result<()> {
        let stale = || GitError::Stale(format!("{path} changed since it was displayed"));
        let file = self
            .file_diff(path, side, context)
            .await?
            .ok_or_else(stale)?;
        let hunk = file
            .hunks
            .iter()
            .find(|hunk| hunk.id == hunk_id)
            .ok_or_else(stale)?;
        let reverse = side == Side::Staged;
        let patch = build_patch(&file, hunk, lines, reverse)?;
        if self.engine == Engine::BuiltIn {
            let (root, patch) = (self.root.clone(), parse_diff(path, &patch));
            return blocking(move || crate::git_builtin::apply(&root, &patch, reverse)).await;
        }
        let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.run(false, &args, Some(&patch), &[0])
            .await
            .map_err(|err| match err {
                GitError::Failed { stderr, .. } => {
                    GitError::Stale(format!("git could not apply the selection: {stderr}"))
                }
                err => err,
            })?;
        Ok(())
    }

    /// Stages whole paths, including deletions and untracked files.
    pub async fn stage_paths(&self, paths: &[String]) -> Result<()> {
        if self.engine == Engine::BuiltIn {
            return self.built_in_paths(paths, crate::git_builtin::add).await;
        }
        self.for_paths(&["add", "--all", "--"], paths).await
    }

    /// Resets whole paths in the index back to `HEAD`.
    pub async fn unstage_paths(&self, paths: &[String]) -> Result<()> {
        if self.engine == Engine::BuiltIn {
            return self.built_in_paths(paths, crate::git_builtin::reset).await;
        }
        self.for_paths(&["reset", "--quiet", "--"], paths).await
    }

    async fn built_in_paths(
        &self,
        paths: &[String],
        write: fn(&Path, &[String]) -> Result<()>,
    ) -> Result<()> {
        for path in paths {
            validate_path(path)?;
        }
        let (root, paths) = (self.root.clone(), paths.to_vec());
        blocking(move || write(&root, &paths)).await
    }

    async fn for_paths(&self, prefix: &[&str], paths: &[String]) -> Result<()> {
        for path in paths {
            validate_path(path)?;
        }
        for chunk in paths.chunks(MAX_PATHS_PER_COMMAND) {
            let mut args = prefix.to_vec();
            args.extend(chunk.iter().map(String::as_str));
            self.run(false, &args, None, &[0]).await?;
        }
        Ok(())
    }
}

fn validate_path(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && part != ".git");
    if ok {
        Ok(())
    } else {
        Err(GitError::Invalid(format!("invalid path: {path:?}")))
    }
}

fn count_new_file(path: &Path) -> Stat {
    let small = std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_COUNTED_FILE_BYTES);
    if !small {
        return Stat::default();
    }
    let Ok(content) = std::fs::read(path) else {
        return Stat::default();
    };
    if content[..content.len().min(8000)].contains(&0) {
        return Stat::default();
    }
    let newlines = content.iter().filter(|b| **b == b'\n').count();
    let partial = usize::from(content.last().is_some_and(|b| *b != b'\n'));
    Stat {
        added: Some(newlines + partial),
        removed: Some(0),
    }
}

/// Orders entries the way a file listing reads: by name within a directory,
/// the directory's own files ahead of its subdirectories. Git reports every
/// untracked file after the tracked ones, which would split one directory's
/// changes in two.
fn listing_order(a: &StatusEntry, b: &StatusEntry) -> Ordering {
    let by_name = |a: &str, b: &str| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    };
    let mut left = a.path.split('/').peekable();
    let mut right = b.path.split('/').peekable();
    loop {
        let (left_name, right_name) = match (left.next(), right.next()) {
            (Some(left_name), Some(right_name)) => (left_name, right_name),
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        };
        let left_is_dir = left.peek().is_some() || a.directory;
        let right_is_dir = right.peek().is_some() || b.directory;
        let order = left_is_dir
            .cmp(&right_is_dir)
            .then_with(|| by_name(left_name, right_name));
        if order != Ordering::Equal {
            return order;
        }
    }
}

fn parse_status(out: &[u8]) -> Status {
    let mut status = Status {
        branch: None,
        head: None,
        entries: vec![],
    };
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8_lossy(record);
        if let Some(value) = record.strip_prefix("# branch.head ") {
            status.branch = (value != "(detached)").then(|| value.to_owned());
        } else if let Some(value) = record.strip_prefix("# branch.oid ") {
            status.head = (value != "(initial)").then(|| value.to_owned());
        } else if let Some(path) = record.strip_prefix("? ") {
            status.entries.push(StatusEntry {
                directory: path.ends_with('/'),
                path: path.trim_end_matches('/').to_owned(),
                index: '.',
                worktree: '?',
                untracked: true,
                conflicted: false,
                staged: None,
                unstaged: None,
                modified: None,
            });
        } else if record.starts_with("1 ") || record.starts_with("u ") {
            let conflicted = record.starts_with('u');
            // Fixed fields precede the path: 8 for ordinary, 10 for unmerged.
            let mut fields = record.splitn(if conflicted { 11 } else { 9 }, ' ');
            let xy: Vec<char> = fields.nth(1).unwrap_or("..").chars().collect();
            let Some(path) = fields.last() else { continue };
            status.entries.push(StatusEntry {
                path: path.to_owned(),
                index: xy.first().copied().unwrap_or('.'),
                worktree: xy.get(1).copied().unwrap_or('.'),
                untracked: false,
                conflicted,
                directory: false,
                staged: None,
                unstaged: None,
                modified: None,
            });
        }
    }
    status
}

/// Parses `git diff` output for a single path.
fn parse_diff(path: &str, out: &[u8]) -> FileDiff {
    let mut file = FileDiff {
        path: path.to_owned(),
        status: FileStatus::Modified,
        old_mode: None,
        new_mode: None,
        binary: false,
        note: None,
        added: 0,
        removed: 0,
        hunks: vec![],
        deferred: false,
        header: vec![],
    };
    let mut lines = out.split(|b| *b == b'\n').peekable();
    // `split` yields one empty trailing piece for newline-terminated input.
    let mut sections = 0;
    while let Some(line) = lines.next() {
        if lines.peek().is_none() && line.is_empty() {
            break;
        }
        if line.starts_with(b"diff --cc ") || line.starts_with(b"diff --combined ") {
            file.note = Some("unmerged path: resolve the conflict, then stage the file".into());
        } else if line.starts_with(b"diff --git ") {
            sections += 1;
            if sections > 1 {
                file.note = Some("file type changed: stage or unstage the whole file".into());
            }
        }
        if let Some(mut hunk) = parse_hunk_header(line) {
            let (mut old_left, mut new_left) = (hunk.old_count, hunk.new_count);
            let (mut old_no, mut new_no) = (hunk.old_start, hunk.new_start);
            while old_left > 0 || new_left > 0 || lines.peek().is_some_and(|l| l.starts_with(b"\\"))
            {
                let Some(line) = lines.next() else { break };
                let (kind, raw) = match line.first() {
                    Some(b'+') => (LineKind::Added, &line[1..]),
                    Some(b'-') => (LineKind::Removed, &line[1..]),
                    Some(b'\\') => {
                        if let Some(last) = hunk.lines.last_mut() {
                            last.no_eol = true;
                        }
                        continue;
                    }
                    Some(b' ') => (LineKind::Context, &line[1..]),
                    // diff.suppressBlankEmpty prints blank context as empty.
                    _ => (LineKind::Context, line),
                };
                let mut entry = Line {
                    kind,
                    text: String::from_utf8_lossy(raw).into_owned(),
                    old: None,
                    new: None,
                    no_eol: false,
                    hl: None,
                    syn: None,
                    raw: raw.to_vec(),
                };
                if kind != LineKind::Added {
                    entry.old = Some(old_no);
                    old_no += 1;
                    old_left = old_left.saturating_sub(1);
                }
                if kind != LineKind::Removed {
                    entry.new = Some(new_no);
                    new_no += 1;
                    new_left = new_left.saturating_sub(1);
                }
                match kind {
                    LineKind::Added => file.added += 1,
                    LineKind::Removed => file.removed += 1,
                    LineKind::Context => {}
                }
                hunk.lines.push(entry);
            }
            hunk.compute_id();
            highlight_hunk(&mut hunk);
            file.hunks.push(hunk);
            continue;
        }
        if file.hunks.is_empty() && sections <= 1 {
            read_header_line(&mut file, line);
            file.header.push(line.to_vec());
        }
    }
    if file.note.is_some() {
        file.hunks.clear();
    }
    file
}

fn read_header_line(file: &mut FileDiff, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    if let Some(mode) = text.strip_prefix("new file mode ") {
        file.status = FileStatus::Added;
        file.new_mode = Some(mode.to_owned());
    } else if let Some(mode) = text.strip_prefix("deleted file mode ") {
        file.status = FileStatus::Deleted;
        file.old_mode = Some(mode.to_owned());
    } else if let Some(mode) = text.strip_prefix("old mode ") {
        file.old_mode = Some(mode.to_owned());
    } else if let Some(mode) = text.strip_prefix("new mode ") {
        file.new_mode = Some(mode.to_owned());
    } else if let Some(rest) = text.strip_prefix("index ") {
        if let Some((_, mode)) = rest.split_once(' ') {
            file.old_mode = Some(mode.to_owned());
            file.new_mode = Some(mode.to_owned());
        }
    } else if text.starts_with("Binary files ") || text.starts_with("GIT binary patch") {
        file.binary = true;
    }
}

/// Parses `@@ -a[,b] +c[,d] @@[ section]`.
fn parse_hunk_header(line: &[u8]) -> Option<Hunk> {
    let text = std::str::from_utf8(line.strip_prefix(b"@@ -")?).ok();
    let text = match text {
        Some(text) => text.to_owned(),
        None => String::from_utf8_lossy(&line[4..]).into_owned(),
    };
    let (ranges, section) = text.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(usize, usize)> {
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some(Hunk {
        id: String::new(),
        old_start,
        old_count,
        new_start,
        new_count,
        section: section.trim().to_owned(),
        lines: vec![],
    })
}

/// Builds a single-hunk patch to apply to the index.
///
/// Forward patches (`reverse == false`) come from the worktree-vs-index diff,
/// so their old side must keep matching the index: unselected removals turn
/// into context and unselected additions are dropped. Reverse patches come
/// from the index-vs-`HEAD` diff and are applied with `--reverse`, so their
/// new side is the one that must match and the roles swap.
fn build_patch(
    file: &FileDiff,
    hunk: &Hunk,
    selected: Option<&[usize]>,
    reverse: bool,
) -> Result<Vec<u8>> {
    let selected: Option<HashSet<usize>> = selected.map(|lines| lines.iter().copied().collect());
    let is_selected = |index: usize| selected.as_ref().is_none_or(|set| set.contains(&index));
    let (kept_as_context, dropped) = if reverse {
        (LineKind::Added, LineKind::Removed)
    } else {
        (LineKind::Removed, LineKind::Added)
    };

    let mut body = Vec::new();
    let (mut old_count, mut new_count, mut changes) = (0, 0, 0);
    for (index, line) in hunk.lines.iter().enumerate() {
        let kind = match line.kind {
            LineKind::Context => LineKind::Context,
            kind if is_selected(index) => kind,
            kind if kind == kept_as_context => LineKind::Context,
            kind => {
                debug_assert_eq!(kind, dropped);
                continue;
            }
        };
        body.push(match kind {
            LineKind::Context => b' ',
            LineKind::Removed => b'-',
            LineKind::Added => b'+',
        });
        body.extend_from_slice(&line.raw);
        body.push(b'\n');
        if line.no_eol {
            body.extend_from_slice(b"\\ No newline at end of file\n");
        }
        if kind != LineKind::Added {
            old_count += 1;
        }
        if kind != LineKind::Removed {
            new_count += 1;
        }
        if kind != LineKind::Context {
            changes += 1;
        }
    }
    if changes == 0 {
        return Err(GitError::Invalid("no changed lines selected".into()));
    }

    // One side of the hunk is untouched and anchors the position; the other
    // side starts at the same place because the patch holds a single hunk.
    let first_line = |start: usize, count: usize| if count > 0 { start } else { start + 1 };
    let start_at = |position: usize, count: usize| if count > 0 { position } else { position - 1 };
    let (old_start, new_start) = if reverse {
        let position = first_line(hunk.new_start, new_count);
        (start_at(position, old_count), hunk.new_start)
    } else {
        let position = first_line(hunk.old_start, old_count);
        (hunk.old_start, start_at(position, new_count))
    };

    // A patch that deletes the file must leave nothing behind. If part of the
    // content stays, it is an ordinary modification instead.
    let leaves_content = match (file.status, reverse) {
        (FileStatus::Deleted, false) => new_count > 0,
        (FileStatus::Added, true) => old_count > 0,
        _ => false,
    };

    let mut patch = Vec::new();
    for line in header_lines(&file.header, leaves_content) {
        patch.extend_from_slice(&line);
        patch.push(b'\n');
    }
    patch.extend_from_slice(
        format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n").as_bytes(),
    );
    patch.extend_from_slice(&body);
    Ok(patch)
}

/// Returns the file header, rewritten as a plain modification of an existing
/// file when `as_modification` is set.
fn header_lines(header: &[Vec<u8>], as_modification: bool) -> Vec<Vec<u8>> {
    if !as_modification {
        return header.to_vec();
    }
    let old_name = header.iter().find(|l| l.starts_with(b"--- "));
    let new_name = header.iter().find(|l| l.starts_with(b"+++ "));
    let swap_side = |line: &[u8], marker: &[u8], from: &[u8], to: &[u8]| {
        let name = &line[4..];
        let (quote, name): (&[u8], &[u8]) = match name.strip_prefix(b"\"") {
            Some(rest) => (b"\"", rest),
            None => (b"", name),
        };
        let name = name.strip_prefix(from).unwrap_or(name);
        [marker, b" ", quote, to, name].concat()
    };
    header
        .iter()
        .filter(|line| {
            !line.starts_with(b"deleted file mode ")
                && !line.starts_with(b"new file mode ")
                && !line.starts_with(b"index ")
        })
        .map(|line| match (line.as_slice(), old_name, new_name) {
            (b"--- /dev/null", _, Some(new)) => swap_side(new, b"---", b"b/", b"a/"),
            (b"+++ /dev/null", Some(old), _) => swap_side(old, b"+++", b"a/", b"b/"),
            _ => line.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFIED: &[u8] = b"diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,5 +1,6 @@ fn section()
 one
-two
-three
+TWO
+THREE
+three and a half
 four
 five
";

    fn patch(diff: &[u8], selected: Option<&[usize]>, reverse: bool) -> String {
        let file = parse_diff("f.txt", diff);
        let patch = build_patch(&file, &file.hunks[0], selected, reverse).unwrap();
        String::from_utf8(patch).unwrap()
    }

    #[test]
    fn parses_hunks_and_line_numbers() {
        let file = parse_diff("f.txt", MODIFIED);
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!((file.added, file.removed), (3, 2));
        let hunk = &file.hunks[0];
        assert_eq!(hunk.section, "fn section()");
        assert_eq!(hunk.lines.len(), 8);
        assert_eq!((hunk.lines[1].old, hunk.lines[1].new), (Some(2), None));
        assert_eq!((hunk.lines[3].old, hunk.lines[3].new), (None, Some(2)));
        assert_eq!((hunk.lines[7].old, hunk.lines[7].new), (Some(5), Some(6)));
        // "two" -> "TWO" changes the whole line, which is marked as a whole;
        // "three" carries on into the last added line, after which the rest
        // is new.
        assert_eq!(hunk.lines[1].hl, Some(vec![[0, 3]]));
        assert_eq!(hunk.lines[5].text, "three and a half");
        assert_eq!(hunk.lines[5].hl, Some(vec![[5, 16]]));
        assert!(hunk.lines[0].hl.is_none());
    }

    #[test]
    fn whole_hunk_round_trips() {
        let expected = String::from_utf8(MODIFIED.to_vec())
            .unwrap()
            .replace(" @@ fn section()", " @@");
        assert_eq!(patch(MODIFIED, None, false), expected);
        assert_eq!(patch(MODIFIED, None, true), expected);
    }

    #[test]
    fn forward_selection_keeps_old_side() {
        // Select "-two" and "+TWO" only.
        let patch = patch(MODIFIED, Some(&[1, 3]), false);
        assert!(patch.ends_with("@@ -1,5 +1,5 @@\n one\n-two\n three\n+TWO\n four\n five\n"));
    }

    #[test]
    fn reverse_selection_keeps_new_side() {
        // Unstage only "+three and a half".
        let patch = patch(MODIFIED, Some(&[5]), true);
        assert!(
            patch.ends_with(
                "@@ -1,5 +1,6 @@\n one\n TWO\n THREE\n+three and a half\n four\n five\n"
            )
        );
    }

    #[test]
    fn selection_without_changes_is_rejected() {
        let file = parse_diff("f.txt", MODIFIED);
        assert!(build_patch(&file, &file.hunks[0], Some(&[0]), false).is_err());
    }

    #[test]
    fn partial_deletion_becomes_modification() {
        let diff = b"diff --git a/f.txt b/f.txt
deleted file mode 100644
index 1111111..0000000
--- a/f.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
";
        assert_eq!(
            patch(diff, Some(&[1]), false),
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,1 @@\n one\n-two\n"
        );
        assert!(patch(diff, None, false).contains("deleted file mode 100644\n"));
        assert!(patch(diff, None, false).contains("@@ -1,2 +0,0 @@\n"));
    }

    #[test]
    fn partial_new_file_keeps_creation_header() {
        let diff = b"diff --git a/f.txt b/f.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,2 @@
+one
+two
\\ No newline at end of file
";
        let file = parse_diff("f.txt", diff);
        assert!(file.hunks[0].lines[1].no_eol);
        // Every line of a new file is new, and marked so like any other.
        let marks: Vec<_> = file.hunks[0]
            .lines
            .iter()
            .map(|line| line.hl.clone())
            .collect();
        assert_eq!(marks, [Some(vec![[0, 3]]), Some(vec![[0, 3]])]);
        assert_eq!(file.status, FileStatus::Added);
        let forward = patch(diff, Some(&[0]), false);
        assert!(forward.contains("new file mode 100644\n"));
        assert!(forward.ends_with("@@ -0,0 +1,1 @@\n+one\n"));
        // Unstaging one line of a staged new file must not delete the file.
        let reverse = patch(diff, Some(&[1]), true);
        assert!(!reverse.contains("new file mode"));
        assert!(reverse.contains("--- a/f.txt\n+++ b/f.txt\n"));
        assert!(reverse.ends_with("@@ -1,1 +1,2 @@\n one\n+two\n\\ No newline at end of file\n"));
    }

    #[test]
    fn parses_porcelain_status() {
        let out = b"# branch.oid abc123\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaa bbb dir/with space.txt\0\
1 A. N... 000000 100644 100644 000 ccc new.txt\0\
u UU N... 100644 100644 100644 100644 a b c both.txt\0\
? untracked.txt\0? nested/\0";
        let status = parse_status(out);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head.as_deref(), Some("abc123"));
        let paths: Vec<_> = status.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "dir/with space.txt",
                "new.txt",
                "both.txt",
                "untracked.txt",
                "nested"
            ]
        );
        assert_eq!(
            (status.entries[0].index, status.entries[0].worktree),
            ('.', 'M')
        );
        assert_eq!(
            (status.entries[1].index, status.entries[1].worktree),
            ('A', '.')
        );
        assert!(status.entries[2].conflicted);
        assert!(status.entries[3].untracked && !status.entries[3].directory);
        assert!(status.entries[4].directory);
    }

    #[test]
    fn lists_files_before_folders_and_mixes_untracked_in() {
        // As git reports them: tracked changes in path order, then untracked.
        let out = b"1 .M N... 100644 100644 100644 aaa bbb Zed.txt\0\
1 .M N... 100644 100644 100644 aaa bbb src/a/deep.rs\0\
1 .M N... 100644 100644 100644 aaa bbb src/b.rs\0\
1 .M N... 100644 100644 100644 aaa bbb src/main.rs\0\
? alpha.txt\0? nested/\0? src/a.rs\0? src/a/Cargo.toml\0? src/lib.rs\0";
        let mut entries = parse_status(out).entries;
        entries.sort_by(listing_order);
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "alpha.txt",
                "Zed.txt",
                "nested",
                "src/a.rs",
                "src/b.rs",
                "src/lib.rs",
                "src/main.rs",
                "src/a/Cargo.toml",
                "src/a/deep.rs",
            ]
        );
    }

    /// Both gits: every test that touches a repository runs with each.
    const ENGINES: [Engine; 2] = [Engine::Installed, Engine::BuiltIn];

    /// A scratch repository with one commit containing `files`.
    fn scratch(engine: Engine, files: &[(&str, &str)]) -> (tempfile::TempDir, Git) {
        // Shown when a test fails, to say which git it failed with.
        println!("with {engine:?}");
        let dir = tempfile::tempdir().unwrap();
        sh(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        for (path, content) in files {
            write(dir.path(), path, content);
        }
        sh(dir.path(), &["add", "--all"]);
        sh(
            dir.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "base"],
        );
        let git = Git::with_engine(dir.path(), engine);
        (dir, git)
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn write(dir: &Path, path: &str, content: &str) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    /// Content of `path` as recorded in the index.
    fn staged(dir: &Path, path: &str) -> String {
        sh(dir, &["show", &format!(":{path}")])
    }

    async fn hunks(git: &Git, path: &str, side: Side) -> Vec<Hunk> {
        let file = git.file_diff(path, side, 3).await.unwrap();
        file.map(|file| file.hunks).unwrap_or_default()
    }

    /// Indexes of the hunk lines whose text is one of `texts`.
    fn pick(hunk: &Hunk, texts: &[&str]) -> Vec<usize> {
        let picked: Vec<usize> = (0..hunk.lines.len())
            .filter(|i| hunk.lines[*i].kind != LineKind::Context)
            .filter(|i| texts.contains(&hunk.lines[*i].text.as_str()))
            .collect();
        assert_eq!(picked.len(), texts.len(), "{texts:?} in {hunk:?}");
        picked
    }

    #[tokio::test]
    async fn stages_and_unstages_one_hunk_of_several() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[("f.txt", &numbered(30))]);
            let edited = numbered(30)
                .replace("line 2\n", "line two\n")
                .replace("line 28\n", "line twenty-eight\n");
            write(dir.path(), "f.txt", &edited);

            let unstaged = hunks(&git, "f.txt", Side::Unstaged).await;
            assert_eq!(unstaged.len(), 2);
            git.apply_hunk("f.txt", Side::Unstaged, 3, &unstaged[1].id, None)
                .await
                .unwrap();
            assert_eq!(
                staged(dir.path(), "f.txt"),
                numbered(30).replace("line 28\n", "line twenty-eight\n")
            );
            assert_eq!(hunks(&git, "f.txt", Side::Unstaged).await.len(), 1);

            let reviewed = hunks(&git, "f.txt", Side::Staged).await;
            assert_eq!(reviewed.len(), 1);
            git.apply_hunk("f.txt", Side::Staged, 3, &reviewed[0].id, None)
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), numbered(30));
            assert!(
                git.file_diff("f.txt", Side::Staged, 3)
                    .await
                    .unwrap()
                    .is_none()
            );
            // The worktree is never touched.
            assert_eq!(
                std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
                edited
            );
        }
    }

    #[tokio::test]
    async fn stages_and_unstages_selected_lines() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[("f.txt", "a\nb\nc\nd\n")]);
            write(dir.path(), "f.txt", "a\nB\nnew\nc\nD\n");

            let hunk = hunks(&git, "f.txt", Side::Unstaged).await.remove(0);
            let lines = pick(&hunk, &["b", "B"]);
            git.apply_hunk("f.txt", Side::Unstaged, 3, &hunk.id, Some(&lines))
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), "a\nB\nc\nd\n");

            let hunk = hunks(&git, "f.txt", Side::Unstaged).await.remove(0);
            git.apply_hunk("f.txt", Side::Unstaged, 3, &hunk.id, None)
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), "a\nB\nnew\nc\nD\n");

            let hunk = hunks(&git, "f.txt", Side::Staged).await.remove(0);
            let lines = pick(&hunk, &["new"]);
            git.apply_hunk("f.txt", Side::Staged, 3, &hunk.id, Some(&lines))
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), "a\nB\nc\nD\n");
        }
    }

    #[tokio::test]
    async fn stages_an_untracked_file_in_parts() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[]);
            let path = "dir with space/n\u{e9}w file.txt";
            write(dir.path(), path, "one\ntwo\nthree");

            let status = git.status().await.unwrap();
            assert_eq!(status.entries.len(), 1);
            assert!(status.entries[0].untracked);
            assert_eq!(status.entries[0].path, path);
            assert_eq!(status.entries[0].unstaged.as_ref().unwrap().added, Some(3));

            let file = git
                .file_diff(path, Side::Unstaged, 3)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(file.status, FileStatus::Added);
            let hunk = &file.hunks[0];
            assert!(hunk.lines[2].no_eol);
            let lines = pick(hunk, &["two"]);
            git.apply_hunk(path, Side::Unstaged, 3, &hunk.id, Some(&lines))
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), path), "two\n");

            // Unstaging the only staged line makes the file untracked again.
            let hunk = hunks(&git, path, Side::Staged).await.remove(0);
            git.apply_hunk(path, Side::Staged, 3, &hunk.id, None)
                .await
                .unwrap();
            assert!(git.status().await.unwrap().entries[0].untracked);

            git.stage_paths(&[path.to_owned()]).await.unwrap();
            assert_eq!(staged(dir.path(), path), "one\ntwo\nthree");
            assert!(
                git.file_diff(path, Side::Unstaged, 3)
                    .await
                    .unwrap()
                    .is_none()
            );
            git.unstage_paths(&[path.to_owned()]).await.unwrap();
            assert!(git.status().await.unwrap().entries[0].untracked);
        }
    }

    #[tokio::test]
    async fn stages_a_deletion_in_parts() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[("f.txt", "one\ntwo\nthree\n")]);
            std::fs::remove_file(dir.path().join("f.txt")).unwrap();

            let file = git
                .file_diff("f.txt", Side::Unstaged, 3)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(file.status, FileStatus::Deleted);
            let lines = pick(&file.hunks[0], &["two"]);
            git.apply_hunk("f.txt", Side::Unstaged, 3, &file.hunks[0].id, Some(&lines))
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), "one\nthree\n");

            git.stage_paths(&["f.txt".to_owned()]).await.unwrap();
            let status = git.status().await.unwrap();
            assert_eq!(
                (status.entries[0].index, status.entries[0].worktree),
                ('D', '.')
            );

            // Bringing back part of a staged deletion recreates the file.
            let hunk = hunks(&git, "f.txt", Side::Staged).await.remove(0);
            let lines = pick(&hunk, &["three"]);
            git.apply_hunk("f.txt", Side::Staged, 3, &hunk.id, Some(&lines))
                .await
                .unwrap();
            assert_eq!(staged(dir.path(), "f.txt"), "three\n");
        }
    }

    #[tokio::test]
    async fn highlights_each_side_from_its_own_version() {
        for engine in ENGINES {
            let base = "/* old\n   comment */\nfn a() {}\n";
            let (dir, git) = scratch(engine, &[("f.rs", base)]);
            // Line 2 was inside a comment and no longer is.
            write(dir.path(), "f.rs", "/* old */\nlet b = 2;\nfn a() {}\n");
            let classes = |file: &FileDiff, text: &str| -> Vec<&'static str> {
                let line = file.hunks[0].lines.iter().find(|line| line.text == text);
                let spans = line.unwrap().syn.clone().unwrap_or_default();
                spans.into_iter().map(|(_, _, class)| class).collect()
            };

            let file = git
                .highlighted_file_diff("f.rs", Side::Unstaged, 3)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(classes(&file, "   comment */"), ["c"]);
            assert_eq!(classes(&file, "let b = 2;"), ["k", "n"]);
            assert_eq!(classes(&file, "fn a() {}"), ["k", "f"]);
            // Tokens do not change which hunk is which.
            let plain = hunks(&git, "f.rs", Side::Unstaged).await;
            assert_eq!(plain[0].id, file.hunks[0].id);

            git.stage_paths(&["f.rs".to_owned()]).await.unwrap();
            write(dir.path(), "f.rs", "not rust at all {{{\n");
            let file = git
                .highlighted_file_diff("f.rs", Side::Staged, 3)
                .await
                .unwrap()
                .unwrap();
            // The staged side reads the index, not the file now on disk.
            assert_eq!(classes(&file, "let b = 2;"), ["k", "n"]);
            assert_eq!(classes(&file, "   comment */"), ["c"]);

            let status = git.status().await.unwrap();
            assert!(status.entries[0].modified.is_some());
        }
    }

    #[tokio::test]
    async fn lists_files_that_are_not_ignored() {
        for engine in ENGINES {
            let (dir, git) = scratch(
                engine,
                &[
                    ("kept.txt", "a"),
                    ("gone.txt", "b"),
                    (".gitignore", "build/\n"),
                ],
            );
            std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
            write(dir.path(), "src/new file.rs", "fn a() {}");
            write(dir.path(), "build/out.o", "x");
            sh(&dir.path().join("src"), &["init", "--quiet", "nested"]);
            write(dir.path(), "src/nested/inside.txt", "x");
            assert_eq!(
                git.files().await.unwrap(),
                [".gitignore", "kept.txt", "src/nested/", "src/new file.rs"]
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_hunk_that_no_longer_exists() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[("f.txt", "a\n")]);
            write(dir.path(), "f.txt", "b\n");
            let hunk = hunks(&git, "f.txt", Side::Unstaged).await.remove(0);
            write(dir.path(), "f.txt", "c\n");
            let result = git
                .apply_hunk("f.txt", Side::Unstaged, 3, &hunk.id, None)
                .await;
            assert!(matches!(result, Err(GitError::Stale(_))), "{result:?}");
            assert_eq!(staged(dir.path(), "f.txt"), "a\n");
        }
    }

    #[tokio::test]
    async fn reports_binary_files_without_hunks() {
        for engine in ENGINES {
            let (dir, git) = scratch(engine, &[]);
            std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();
            let file = git
                .file_diff("blob.bin", Side::Unstaged, 3)
                .await
                .unwrap()
                .unwrap();
            assert!(file.binary && file.hunks.is_empty());
            assert!(
                git.status().await.unwrap().entries[0]
                    .unstaged
                    .as_ref()
                    .unwrap()
                    .added
                    .is_none()
            );
        }
    }

    #[test]
    fn rejects_paths_that_escape() {
        assert!(validate_path("track/a/b.rs").is_ok());
        for bad in ["", "/etc/passwd", "a/../b", ".git/config", "a//b", "a/./b"] {
            assert!(validate_path(bad).is_err(), "{bad}");
        }
    }
}
