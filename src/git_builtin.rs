//! The built-in git, for machines without one: what `git.rs` and `root.rs`
//! ask of the `git` executable, answered with grit-lib instead.
//!
//! Each function answers as the git command it stands in for would: diffs
//! come out as `git diff` prints them, so the parsing, hunk ids and patches
//! in `git.rs` are shared. The tests at the end compare every answer with
//! real git's, and `spikes/git-engines/` compares the index writes in more
//! kinds of repository.
//!
//! Everything here blocks; `git.rs` calls it from `spawn_blocking`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use grit_lib::config::ConfigSet;
use grit_lib::crlf::ConversionConfig;
use grit_lib::crlf::ConvertToGitOpts;
use grit_lib::crlf::DiffAttr;
use grit_lib::crlf::FileAttrs;
use grit_lib::diff::DiffStatus;
use grit_lib::ignore::IgnoreMatcher;
use grit_lib::index::Index;
use grit_lib::index::IndexEntry;
use grit_lib::objects::ObjectId;
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::status::IgnoredMode;
use grit_lib::repo::Repository;
use grit_lib::state::HeadState;
use grit_lib::userdiff::FuncnameMatcher;

use crate::diff::FileDiff;
use crate::diff::FileStatus;
use crate::diff::LineKind;
use crate::git::GitError;
use crate::git::Side;
use crate::git::Stat;
use crate::git::Status;
use crate::git::StatusEntry;

type Result<T> = std::result::Result<T, GitError>;

const MODE_REGULAR: u32 = 0o100644;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;

/// Git looks this far into a file for a NUL to decide it is binary.
const BINARY_PROBE: usize = 8000;

fn failed(command: &str, err: impl std::fmt::Display) -> GitError {
    GitError::Failed {
        command: command.to_owned(),
        stderr: err.to_string(),
    }
}

/// The repository whose worktree is `root`, with what reading it needs.
struct Repo {
    repo: Repository,
    root: PathBuf,
    config: ConfigSet,
    conversion: ConversionConfig,
    attributes: Vec<grit_lib::crlf::AttrRule>,
}

impl Repo {
    fn open(root: &Path) -> Result<Self> {
        let repo = Repository::discover(Some(root)).map_err(|err| failed("open", err))?;
        let config =
            ConfigSet::load(Some(&repo.git_dir), true).map_err(|err| failed("config", err))?;
        Ok(Self {
            conversion: ConversionConfig::from_config(&config),
            attributes: grit_lib::crlf::load_gitattributes(root),
            config,
            root: root.to_owned(),
            repo,
        })
    }

    fn index(&self) -> Result<Index> {
        match self.repo.load_index() {
            Ok(index) => Ok(index),
            Err(grit_lib::error::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                Ok(Index::new())
            }
            Err(err) => Err(failed("index", err)),
        }
    }

    fn attrs(&self, path: &str) -> FileAttrs {
        grit_lib::crlf::get_file_attrs(&self.attributes, path, false, &self.config)
    }

    fn read_blob(&self, oid: &ObjectId) -> Result<Vec<u8>> {
        if *oid == grit_lib::diff::zero_oid() {
            return Ok(Vec::new());
        }
        Ok(self
            .repo
            .odb
            .read(oid)
            .map_err(|err| failed("cat-file", err))?
            .data)
    }

    fn hash(&self, content: &[u8]) -> ObjectId {
        self.repo.odb.hash(ObjectKind::Blob, content)
    }

    fn write_blob(&self, content: &[u8]) -> Result<ObjectId> {
        self.repo
            .odb
            .write(ObjectKind::Blob, content)
            .map_err(|err| failed("hash-object", err))
    }

    /// Whether git keeps the executable bit (`core.fileMode`).
    fn trusts_mode(&self) -> bool {
        !matches!(self.config.get_bool("core.filemode"), Some(Ok(false)))
    }

    /// The path in `HEAD`'s tree, if `HEAD` has one.
    fn head_entry(&self, path: &str) -> Option<(ObjectId, u32)> {
        let found =
            grit_lib::rev_parse::resolve_treeish_blob_at_path(&self.repo, &format!("HEAD:{path}"))
                .ok()?;
        Some((found.oid, u32::from_str_radix(&found.mode, 8).ok()?))
    }

    /// A worktree file as git would store it: through clean filters and
    /// line-ending conversion, with the mode git would record. `index` is the
    /// entry it would replace, which line-ending conversion consults.
    fn worktree_version(&self, path: &str, index: Option<&IndexEntry>) -> Result<Option<Version>> {
        let file = self.root.join(path);
        let meta = match fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if meta.is_dir() {
            return Ok(None);
        }
        let mut mode = grit_lib::diff::mode_from_metadata(&meta);
        if mode != MODE_SYMLINK
            && !self.trusts_mode()
            && let Some(entry) = index.filter(|entry| entry.mode != MODE_SYMLINK)
        {
            mode = entry.mode;
        }
        let content = if meta.file_type().is_symlink() {
            fs::read_link(&file)?.into_os_string().into_encoded_bytes()
        } else {
            let raw = fs::read(&file)?;
            let prior = match index {
                Some(entry) => Some(self.read_blob(&entry.oid)?),
                None => None,
            };
            let opts = ConvertToGitOpts {
                index_blob: prior.as_deref(),
                renormalize: false,
                check_safecrlf: false,
            };
            grit_lib::crlf::convert_to_git_with_opts(
                &raw,
                path,
                &self.conversion,
                &self.attrs(path),
                opts,
            )
            .map_err(|err| failed("add", err))?
        };
        Ok(Some(Version {
            oid: self.hash(&content),
            content,
            mode,
        }))
    }

    fn index_version(&self, entry: &IndexEntry) -> Result<Version> {
        Ok(Version {
            content: self.read_blob(&entry.oid)?,
            mode: entry.mode,
            oid: entry.oid,
        })
    }
}

/// One side of a diff.
struct Version {
    content: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

fn stage0<'a>(index: &'a Index, path: &str) -> Option<&'a IndexEntry> {
    index.get(path.as_bytes(), 0)
}

fn is_unmerged(index: &Index, path: &str) -> bool {
    (1..=3).any(|stage| index.get(path.as_bytes(), stage).is_some())
}

// ------------------------------------------------------------------ reading

/// What has changed in the index and the worktree, untracked files apart.
fn changes(repo: &Repo) -> Result<grit_lib::porcelain::status::StatusModel> {
    let opts = grit_lib::porcelain::status::StatusOptions {
        // Listed by `walk`, which, as git does, does not enter a nested
        // repository.
        untracked: grit_lib::porcelain::status::UntrackedMode::No,
        ignored: IgnoredMode::No,
        renames: None,
        pathspecs: Vec::new(),
        ahead_behind: false,
    };
    grit_lib::porcelain::status::status(&repo.repo, &opts, &mut grit_lib::progress::NullProgress)
        .map_err(|err| failed("status", err))
}

/// `git status --porcelain=v2 --branch --untracked-files=all --no-renames`,
/// parsed.
pub fn status(root: &Path) -> Result<Status> {
    let repo = Repo::open(root)?;
    let model = changes(&repo)?;
    let (branch, head) = match &model.head {
        HeadState::Branch {
            short_name, oid, ..
        } => (Some(short_name.clone()), oid.map(|oid| oid.to_hex())),
        HeadState::Detached { oid } => (None, Some(oid.to_hex())),
        HeadState::Invalid => (None, None),
    };

    // Index-side and worktree-side letters by path; unmerged paths apart.
    let mut letters: BTreeMap<String, (char, char)> = BTreeMap::new();
    for entry in &model.staged {
        if entry.status != DiffStatus::Unmerged {
            letters
                .entry(entry.path().to_owned())
                .or_insert(('.', '.'))
                .0 = letter(entry);
        }
    }
    for entry in &model.unstaged {
        if entry.status != DiffStatus::Unmerged {
            letters
                .entry(entry.path().to_owned())
                .or_insert(('.', '.'))
                .1 = letter(entry);
        }
    }
    let mut stages: BTreeMap<String, u8> = BTreeMap::new();
    for entry in &model.index.entries {
        if entry.stage() > 0 {
            *stages
                .entry(String::from_utf8_lossy(&entry.path).into_owned())
                .or_default() |= 1 << (entry.stage() - 1);
        }
    }
    let mut entries: Vec<StatusEntry> = Vec::new();
    for (path, (index, worktree)) in letters {
        if stages.contains_key(&path) {
            continue;
        }
        entries.push(status_entry(path, index, worktree, false));
    }
    for (path, mask) in stages {
        // As git's wt-status names each combination of conflict stages.
        let xy = match mask {
            0b001 => "DD",
            0b010 => "AU",
            0b011 => "UD",
            0b100 => "UA",
            0b101 => "DU",
            0b110 => "AA",
            _ => "UU",
        };
        let mut xy = xy.chars();
        let (index, worktree) = (xy.next().unwrap(), xy.next().unwrap());
        entries.push(status_entry(path, index, worktree, true));
    }
    // Git lists tracked changes in path order, then the untracked.
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for path in Walk::run(&repo, &model.index, false)?.untracked {
        let mut entry = status_entry(path.trim_end_matches('/').to_owned(), '.', '?', false);
        entry.untracked = true;
        entry.directory = path.ends_with('/');
        entries.push(entry);
    }
    Ok(Status {
        branch,
        head,
        entries,
    })
}

fn status_entry(path: String, index: char, worktree: char, conflicted: bool) -> StatusEntry {
    StatusEntry {
        path,
        index,
        worktree,
        untracked: false,
        conflicted,
        directory: false,
        staged: None,
        unstaged: None,
        modified: None,
    }
}

fn letter(entry: &grit_lib::diff::DiffEntry) -> char {
    let kind = |mode: &str| {
        u32::from_str_radix(mode, 8)
            .map(|mode| mode & 0o170000)
            .unwrap_or(0)
    };
    let (old, new) = (kind(&entry.old_mode), kind(&entry.new_mode));
    if entry.status == DiffStatus::Modified && old != 0 && new != 0 && old != new {
        return 'T';
    }
    match entry.status {
        DiffStatus::Added => 'A',
        DiffStatus::Deleted => 'D',
        DiffStatus::Modified => 'M',
        DiffStatus::Renamed => 'R',
        DiffStatus::Copied => 'C',
        DiffStatus::TypeChanged => 'T',
        DiffStatus::Unmerged => 'U',
    }
}

/// `git diff --numstat [--cached]`, parsed.
pub fn numstat(root: &Path, cached: bool) -> Result<HashMap<String, Stat>> {
    let repo = Repo::open(root)?;
    // Only what changed on that side is counted, as status finds it.
    let model = changes(&repo)?;
    let index = model.index;
    let side = if cached {
        &model.staged
    } else {
        &model.unstaged
    };
    let mut seen = HashSet::new();
    let paths: Vec<String> = side
        .iter()
        .map(|entry| entry.path().to_owned())
        .chain(
            index
                .entries
                .iter()
                .filter(|entry| entry.stage() > 0)
                .map(|entry| path_of(entry).into_owned()),
        )
        .filter(|path| seen.insert(path.clone()))
        .collect();
    let mut stats = HashMap::new();
    for path in paths {
        let (old, new) = if is_unmerged(&index, &path) {
            // As git counts a conflict: nothing staged, and the worktree
            // against our side.
            if cached {
                let none = Stat {
                    added: Some(0),
                    removed: Some(0),
                };
                stats.insert(path, none);
                continue;
            }
            let ours = match index.get(path.as_bytes(), 2) {
                Some(entry) => Some(repo.index_version(entry)?),
                None => None,
            };
            (ours, repo.worktree_version(&path, None)?)
        } else {
            sides(&repo, &index, &path, cached)?
        };
        if same(old.as_ref(), new.as_ref()) {
            continue;
        }
        let empty = Vec::new();
        let old_content = old.as_ref().map_or(&empty, |v| &v.content);
        let new_content = new.as_ref().map_or(&empty, |v| &v.content);
        let stat = if is_binary(&repo, &path, old_content, new_content) {
            Stat::default()
        } else {
            let hunks = hunks(&repo, &path, old_content, new_content, 0);
            let count = |marker: u8| {
                hunks
                    .split(|b| *b == b'\n')
                    .filter(|line| line.first() == Some(&marker))
                    .count()
            };
            Stat {
                added: Some(count(b'+')),
                removed: Some(count(b'-')),
            }
        };
        stats.insert(path, stat);
    }
    Ok(stats)
}

/// The two versions a diff of `path` compares: index and worktree, or (for
/// `cached`) `HEAD` and index.
fn sides(
    repo: &Repo,
    index: &Index,
    path: &str,
    cached: bool,
) -> Result<(Option<Version>, Option<Version>)> {
    let staged = stage0(index, path);
    let staged_version = match staged {
        Some(entry) => Some(repo.index_version(entry)?),
        None => None,
    };
    if cached {
        let head = match repo.head_entry(path) {
            Some((oid, mode)) => Some(Version {
                content: repo.read_blob(&oid)?,
                mode,
                oid,
            }),
            None => None,
        };
        // An intent-to-add entry is not staged content yet.
        let staged_version = staged_version.filter(|_| !staged.is_some_and(intent_to_add));
        return Ok((head, staged_version));
    }
    let Some(entry) = staged else {
        return Ok((None, None));
    };
    let mut worktree = repo.worktree_version(path, Some(entry))?;
    if entry.mode == MODE_GITLINK {
        worktree = None;
    }
    // A skip-worktree entry is not compared with the worktree.
    if skip_worktree(entry) {
        return Ok((None, None));
    }
    let old = if intent_to_add(entry) {
        None
    } else {
        staged_version
    };
    Ok((old, worktree))
}

fn intent_to_add(entry: &IndexEntry) -> bool {
    entry
        .flags_extended
        .is_some_and(|flags| flags & (1 << 13) != 0)
}

fn skip_worktree(entry: &IndexEntry) -> bool {
    entry
        .flags_extended
        .is_some_and(|flags| flags & (1 << 14) != 0)
}

fn same(old: Option<&Version>, new: Option<&Version>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(old), Some(new)) => old.oid == new.oid && old.mode == new.mode,
        _ => false,
    }
}

/// Whether git would diff these as binary: by attribute, or by a NUL near
/// the start of either side.
fn is_binary(repo: &Repo, path: &str, old: &[u8], new: &[u8]) -> bool {
    match repo.attrs(path).diff_attr {
        DiffAttr::Unset => true,
        DiffAttr::Set => false,
        DiffAttr::Driver(driver) if repo.config.get(&format!("diff.{driver}.binary")).is_some() => {
            matches!(
                repo.config.get_bool(&format!("diff.{driver}.binary")),
                Some(Ok(true))
            )
        }
        _ => [old, new]
            .iter()
            .any(|side| side[..side.len().min(BINARY_PROBE)].contains(&0)),
    }
}

/// Hunks of a unified diff, from the first `@@` on, as `git diff` writes
/// them with the user's diff settings.
fn hunks(repo: &Repo, path: &str, old: &[u8], new: &[u8], context: usize) -> Vec<u8> {
    // grit-lib diffs text; one char per byte (Latin-1) carries any bytes
    // through and back unchanged.
    let latin = |bytes: &[u8]| -> String { bytes.iter().map(|&b| b as char).collect() };
    let matcher: Option<FuncnameMatcher> =
        grit_lib::userdiff::matcher_for_path(&repo.config, &repo.attributes, path)
            .ok()
            .flatten();
    let algorithm = repo.config.get("diff.algorithm").unwrap_or_default();
    let inter_hunk = repo
        .config
        .get("diff.interhunkcontext")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let text = grit_lib::diff::unified_diff_with_prefix_and_funcname_and_algorithm(
        &latin(old),
        &latin(new),
        "a/x",
        "b/x",
        context,
        inter_hunk,
        "",
        "",
        matcher.as_ref(),
        match algorithm.as_str() {
            "patience" => similar::Algorithm::Patience,
            _ => similar::Algorithm::Myers,
        },
        false,
        algorithm == "histogram",
        grit_lib::diff::indent_heuristic_from_config(&repo.config),
        false,
    );
    let start = if text.starts_with("@@ ") {
        Some(0)
    } else {
        text.find("\n@@ ").map(|at| at + 1)
    };
    match start {
        Some(start) => text[start..].chars().map(|c| c as u8).collect(),
        None => Vec::new(),
    }
}

/// A path as git writes it in diff headers: C-quoted when it holds control
/// characters, quotes or backslashes (`core.quotePath=false` leaves other
/// bytes alone).
fn quoted(prefix: &str, path: &str) -> String {
    let whole = format!("{prefix}{path}");
    if !whole
        .bytes()
        .any(|b| b < 0x20 || b == b'"' || b == b'\\' || b == 0x7f)
    {
        return whole;
    }
    let mut out = String::from("\"");
    for c in whole.chars() {
        match c {
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One file's section of `git diff --full-index`, from `old` to `new`.
fn file_patch(
    repo: &Repo,
    path: &str,
    old: Option<&Version>,
    new: Option<&Version>,
    context: usize,
) -> Vec<u8> {
    if same(old, new) {
        return Vec::new();
    }
    let kind = |mode: u32| mode & 0o170000;
    if let (Some(o), Some(n)) = (old, new)
        && kind(o.mode) != kind(n.mode)
    {
        // A change of file type is a deletion and an addition.
        let mut out = file_patch(repo, path, Some(o), None, context);
        out.extend(file_patch(repo, path, None, Some(n), context));
        return out;
    }
    let zero = grit_lib::diff::zero_oid().to_hex();
    let (a, b) = (quoted("a/", path), quoted("b/", path));
    let mut out = format!("diff --git {a} {b}\n");
    match (old, new) {
        (None, Some(n)) => {
            out += &format!(
                "new file mode {:o}\nindex {zero}..{}\n",
                n.mode,
                n.oid.to_hex()
            );
        }
        (Some(o), None) => {
            out += &format!(
                "deleted file mode {:o}\nindex {}..{zero}\n",
                o.mode,
                o.oid.to_hex()
            );
        }
        (Some(o), Some(n)) if o.mode != n.mode => {
            out += &format!("old mode {:o}\nnew mode {:o}\n", o.mode, n.mode);
            if o.oid != n.oid {
                out += &format!("index {}..{}\n", o.oid.to_hex(), n.oid.to_hex());
            }
        }
        (Some(o), Some(n)) => {
            out += &format!(
                "index {}..{} {:o}\n",
                o.oid.to_hex(),
                n.oid.to_hex(),
                o.mode
            );
        }
        (None, None) => unreachable!(),
    }
    let old_content = old.filter(|v| v.oid != new.map_or(grit_lib::diff::zero_oid(), |n| n.oid));
    let new_content = new.filter(|v| v.oid != old.map_or(grit_lib::diff::zero_oid(), |o| o.oid));
    if old_content.is_none() && new_content.is_none() && old.is_some() && new.is_some() {
        // The mode alone changed.
        return out.into_bytes();
    }
    let empty = Vec::new();
    let old_bytes = old.map_or(&empty, |v| &v.content);
    let new_bytes = new.map_or(&empty, |v| &v.content);
    let gitlink = |v: Option<&Version>| v.is_some_and(|v| v.mode == MODE_GITLINK);
    let (from, to) = (
        if old.is_some() {
            a.clone()
        } else {
            "/dev/null".into()
        },
        if new.is_some() {
            b.clone()
        } else {
            "/dev/null".into()
        },
    );
    let body = if gitlink(old) || gitlink(new) {
        let line =
            |v: Option<&Version>| v.map(|v| format!("Subproject commit {}\n", v.oid.to_hex()));
        let mut diff = String::from("@@ -");
        diff += if old.is_some() { "1" } else { "0,0" };
        diff += " +";
        diff += if new.is_some() { "1" } else { "0,0" };
        diff += " @@\n";
        if let Some(line) = line(old) {
            diff += &format!("-{line}");
        }
        if let Some(line) = line(new) {
            diff += &format!("+{line}");
        }
        diff.into_bytes()
    } else if is_binary(repo, path, old_bytes, new_bytes) {
        out += &format!("Binary files {from} and {to} differ\n");
        return out.into_bytes();
    } else {
        hunks(repo, path, old_bytes, new_bytes, context)
    };
    let mut out = out.into_bytes();
    if !body.is_empty() {
        // Git ends a name with a space in it with a tab, for `patch`.
        let tab = |name: &str| if name.contains(' ') { "\t" } else { "" };
        out.extend(format!("--- {from}{}\n+++ {to}{}\n", tab(&from), tab(&to)).into_bytes());
        out.extend(body);
    }
    out
}

/// `git diff --no-color --no-ext-diff --no-textconv --no-renames
/// --full-index -U<context> [--cached] -- <path>`, and for an untracked path
/// what `git diff --no-index -- /dev/null <path>` prints.
pub fn diff(root: &Path, path: &str, side: Side, context: usize) -> Result<Vec<u8>> {
    let repo = Repo::open(root)?;
    let index = repo.index()?;
    if is_unmerged(&index, path) {
        return Ok(match side {
            Side::Unstaged => format!("diff --cc {path}\n").into_bytes(),
            Side::Staged => format!("* Unmerged path {path}\n").into_bytes(),
        });
    }
    let cached = side == Side::Staged;
    let tracked = stage0(&index, path).is_some();
    let (old, new) = if !cached && !tracked {
        if !is_untracked(&repo, &index, path)? {
            return Ok(Vec::new());
        }
        // `git diff --no-index` cannot read a link to a directory, and
        // shows nothing for one.
        if fs::metadata(root.join(path)).is_ok_and(|meta| meta.is_dir()) {
            return Ok(Vec::new());
        }
        (None, repo.worktree_version(path, None)?)
    } else {
        sides(&repo, &index, path, cached)?
    };
    Ok(file_patch(&repo, path, old.as_ref(), new.as_ref(), context))
}

/// Whether `ls-files --others --exclude-standard` would list `path`.
fn is_untracked(repo: &Repo, index: &Index, path: &str) -> Result<bool> {
    if index
        .entries
        .iter()
        .any(|entry| entry.path == path.as_bytes())
        || !fs::symlink_metadata(repo.root.join(path)).is_ok_and(|meta| !meta.is_dir())
    {
        return Ok(false);
    }
    // Not inside a repository of its own, or a submodule.
    let mut dir = Path::new(path).parent();
    while let Some(at) = dir.filter(|at| !at.as_os_str().is_empty()) {
        let rel = at.to_string_lossy();
        if repo.root.join(at).join(".git").exists() || stage0(index, &rel).is_some() {
            return Ok(false);
        }
        dir = at.parent();
    }
    let mut matcher =
        IgnoreMatcher::from_repository(&repo.repo).map_err(|err| failed("ls-files", err))?;
    let (ignored, _) = matcher
        .check_path(&repo.repo, Some(index), path, false)
        .map_err(|err| failed("ls-files", err))?;
    Ok(!ignored)
}

/// The untracked and ignored paths of a worktree, found as git finds them:
/// `.git` is skipped; a directory holding a repository of its own is listed
/// as `dir/` and not entered; an ignored directory is not entered.
struct Walk<'a> {
    repo: &'a Repo,
    index: &'a Index,
    tracked: HashSet<&'a [u8]>,
    /// Directories with tracked files somewhere below.
    tracked_dirs: HashSet<String>,
    matcher: IgnoreMatcher,
    want_ignored: bool,
    untracked: Vec<String>,
    ignored: Vec<String>,
}

impl<'a> Walk<'a> {
    fn run(repo: &'a Repo, index: &'a Index, want_ignored: bool) -> Result<Walk<'a>> {
        let mut tracked_dirs = HashSet::new();
        for entry in &index.entries {
            let path = path_of(entry);
            let mut dir = Path::new(path.as_ref()).parent();
            while let Some(at) = dir.filter(|at| !at.as_os_str().is_empty()) {
                if !tracked_dirs.insert(at.to_string_lossy().into_owned()) {
                    break;
                }
                dir = at.parent();
            }
        }
        let mut walk = Walk {
            repo,
            index,
            tracked: index
                .entries
                .iter()
                .map(|entry| entry.path.as_slice())
                .collect(),
            tracked_dirs,
            matcher: IgnoreMatcher::from_repository(&repo.repo)
                .map_err(|err| failed("ls-files", err))?,
            want_ignored,
            untracked: Vec::new(),
            ignored: Vec::new(),
        };
        walk.visit("", false)?;
        walk.untracked.sort();
        walk.ignored.sort();
        Ok(walk)
    }

    fn is_ignored(&mut self, rel: &str, is_dir: bool) -> Result<bool> {
        let (ignored, _) = self
            .matcher
            .check_path(&self.repo.repo, Some(self.index), rel, is_dir)
            .map_err(|err| failed("ls-files", err))?;
        Ok(ignored)
    }

    /// Visits the directory `rel`. `ignored` says it is ignored itself (and
    /// holds tracked files, or it would not be entered).
    fn visit(&mut self, rel: &str, ignored: bool) -> Result<()> {
        let dir = self.repo.root.join(rel);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let name = name.to_string_lossy();
            let child = if rel.is_empty() {
                name.into_owned()
            } else {
                format!("{rel}/{name}")
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !is_dir {
                if self.tracked.contains(child.as_bytes()) {
                    continue;
                }
                if ignored || self.is_ignored(&child, false)? {
                    if self.want_ignored {
                        self.ignored.push(child);
                    }
                } else {
                    self.untracked.push(child);
                }
                continue;
            }
            if self.tracked.contains(child.as_bytes()) {
                // A submodule.
                continue;
            }
            let child_ignored = ignored || self.is_ignored(&child, true)?;
            if self.tracked_dirs.contains(&child) {
                self.visit(&child, child_ignored)?;
            } else if child_ignored {
                if self.want_ignored {
                    self.ignored.push(format!("{child}/"));
                }
            } else if entry.path().join(".git").exists() {
                self.untracked.push(format!("{child}/"));
            } else if self.want_ignored {
                // Listed as one if all it holds is ignored, as
                // `ls-files --directory` does.
                let (untracked, ignored) = (self.untracked.len(), self.ignored.len());
                self.visit(&child, false)?;
                if self.untracked.len() == untracked && self.ignored.len() > ignored {
                    self.ignored.truncate(ignored);
                    self.ignored.push(format!("{child}/"));
                }
            } else {
                self.visit(&child, false)?;
            }
        }
        Ok(())
    }
}

/// `git cat-file blob <revision>` for `:<path>` (the index) or `HEAD:<path>`.
pub fn blob(root: &Path, revision: &str) -> Option<Vec<u8>> {
    let repo = Repo::open(root).ok()?;
    if let Some(path) = revision.strip_prefix(':') {
        let index = repo.index().ok()?;
        let entry = stage0(&index, path)?;
        return repo.read_blob(&entry.oid).ok();
    }
    let path = revision.strip_prefix("HEAD:")?;
    let (oid, _) = repo.head_entry(path)?;
    repo.read_blob(&oid).ok()
}

/// `git ls-files --cached --others --exclude-standard`, less the tracked
/// files that are gone from the worktree.
pub fn files(root: &Path) -> Result<Vec<String>> {
    let repo = Repo::open(root)?;
    let index = repo.index()?;
    let mut files: Vec<String> = index
        .entries
        .iter()
        .filter(|entry| {
            skip_worktree(entry) || fs::symlink_metadata(root.join(path_of(entry).as_ref())).is_ok()
        })
        .map(|entry| path_of(entry).into_owned())
        .collect();
    files.extend(Walk::run(&repo, &index, false)?.untracked);
    files.sort();
    files.dedup();
    Ok(files)
}

fn path_of(entry: &IndexEntry) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(&entry.path)
}

/// `git ls-files --others --exclude-standard`, or with `ignored`,
/// `git ls-files --others --ignored --exclude-standard --directory`.
pub fn others(root: &Path, ignored: bool) -> Result<Vec<String>> {
    let repo = Repo::open(root)?;
    let index = repo.index()?;
    let walk = Walk::run(&repo, &index, ignored)?;
    Ok(if ignored {
        walk.ignored
    } else {
        walk.untracked
    })
}

/// `git rev-parse --show-toplevel` run in `dir`, or `None` outside any
/// repository.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    let repo = Repository::discover(Some(dir)).ok()?;
    repo.work_tree.and_then(|tree| tree.canonicalize().ok())
}

// ------------------------------------------------------------------ writing

/// Holds `index.lock` from before the index is read until it is replaced,
/// as git does, so that no other git process writes in between.
struct IndexLock {
    lock: PathBuf,
    index: PathBuf,
    written: bool,
}

impl IndexLock {
    fn take(repo: &Repo) -> Result<Self> {
        let index = repo.repo.index_path();
        let lock = index.with_extension("lock");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => Ok(Self {
                lock,
                index,
                written: false,
            }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(GitError::Stale(format!(
                    "another git process is using the index ({} exists); try again",
                    lock.display()
                )))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Where the new index is written before it replaces the old. Not
    /// `index.<something>`: grit-lib locks it as `index.lock`, which is
    /// held already.
    fn fresh(&self) -> PathBuf {
        self.index.with_file_name("projj-index")
    }

    /// Writes `index` through grit-lib, which keeps split and sparse indexes
    /// and every extension, to a file of its own, then puts it in place.
    fn write(mut self, repo: &Repo, mut index: Index) -> Result<()> {
        let fresh = self.fresh();
        repo.repo
            .write_index_at(&fresh, &mut index)
            .map_err(|err| failed("update-index", err))?;
        fs::rename(&fresh, &self.index)?;
        self.written = true;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock);
        if !self.written {
            let _ = fs::remove_file(self.fresh());
        }
    }
}

/// An index entry git would write for content it did not stat from disk
/// (`git apply --cached`, `git reset`): no stat data, so git reads the file
/// again rather than trusting it.
fn unstatted(path: &str, oid: ObjectId, mode: u32) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

/// `git apply --cached [--reverse]` of `patch`, one hunk built by
/// `build_patch` and parsed back. Insists the lines it replaces are exactly
/// where the hunk says; otherwise the index has moved on and the hunk is
/// stale.
pub fn apply(root: &Path, patch: &FileDiff, reverse: bool) -> Result<()> {
    let stale = |why: &str| GitError::Stale(format!("could not apply the selection: {why}"));
    let [hunk] = patch.hunks.as_slice() else {
        return Err(GitError::Invalid("expected a patch of one hunk".into()));
    };
    let path = patch.path.as_str();
    let repo = Repo::open(root)?;
    let lock = IndexLock::take(&repo)?;
    let mut index = repo.index()?;
    if is_unmerged(&index, path) {
        return Err(stale(&format!("{path} is unmerged")));
    }
    let current = stage0(&index, path)
        .filter(|entry| !intent_to_add(entry))
        .cloned();

    // Which lines the index holds now, and which it is to hold instead.
    let (now, then) = if reverse {
        (LineKind::Added, LineKind::Removed)
    } else {
        (LineKind::Removed, LineKind::Added)
    };
    let text = |kind: LineKind| -> Vec<Vec<u8>> {
        hunk.lines
            .iter()
            .filter(|line| line.kind == LineKind::Context || line.kind == kind)
            .map(|line| {
                let mut bytes = line.raw.clone();
                if !line.no_eol {
                    bytes.push(b'\n');
                }
                bytes
            })
            .collect()
    };
    let (expected, replacement) = (text(now), text(then));
    let (start, creates, deletes) = if reverse {
        (
            hunk.new_start,
            patch.status == FileStatus::Deleted,
            patch.status == FileStatus::Added,
        )
    } else {
        (
            hunk.old_start,
            patch.status == FileStatus::Added,
            patch.status == FileStatus::Deleted,
        )
    };
    if creates == current.is_some() {
        return Err(stale(&format!(
            "{path} {} in the index",
            if creates { "is already" } else { "is not" }
        )));
    }
    let base = match &current {
        Some(entry) => repo.read_blob(&entry.oid)?,
        None => Vec::new(),
    };
    let lines: Vec<&[u8]> = base.split_inclusive(|b| *b == b'\n').collect();
    // A hunk that only adds starts after the line it names.
    let at = if expected.is_empty() {
        start
    } else {
        start.saturating_sub(1)
    };
    let fits = at + expected.len() <= lines.len()
        && lines[at..at + expected.len()]
            .iter()
            .zip(&expected)
            .all(|(have, want)| *have == want.as_slice());
    if !fits {
        return Err(stale(&format!("{path} no longer matches")));
    }
    let mut result: Vec<u8> = lines[..at].concat();
    for line in &replacement {
        result.extend_from_slice(line);
    }
    result.extend(lines[at + expected.len()..].concat());

    if deletes {
        if !result.is_empty() {
            return Err(stale(&format!("{path} would not be left empty")));
        }
        index.remove_path_all_stages(path.as_bytes());
    } else {
        let declared = if reverse {
            &patch.old_mode
        } else {
            &patch.new_mode
        };
        let declared = declared
            .as_deref()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok());
        // A patch that changes the mode changes it in the index too.
        let changes_mode = patch.old_mode.is_some() && patch.old_mode != patch.new_mode;
        let mode = match &current {
            Some(entry) if !changes_mode => entry.mode,
            Some(entry) => declared.unwrap_or(entry.mode),
            None => declared.unwrap_or(MODE_REGULAR),
        };
        let oid = repo.write_blob(&result)?;
        index.remove_path_all_stages(path.as_bytes());
        index.add_or_replace(unstatted(path, oid, mode));
    }
    lock.write(&repo, index)
}

/// `git add --all -- <paths>`.
pub fn add(root: &Path, paths: &[String]) -> Result<()> {
    let repo = Repo::open(root)?;
    let lock = IndexLock::take(&repo)?;
    let mut index = repo.index()?;
    for path in paths {
        let file = root.join(path);
        let meta = match fs::symlink_metadata(&file) {
            Ok(meta) => Some(meta),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        let known = index
            .entries
            .iter()
            .any(|entry| entry.path == path.as_bytes());
        let Some(meta) = meta else {
            if !known {
                return Err(failed(
                    "add",
                    format!("pathspec '{path}' did not match any files"),
                ));
            }
            index.remove_path_all_stages(path.as_bytes());
            continue;
        };
        if meta.is_dir() {
            return Err(failed(
                "add",
                format!("{path} is a directory; the built-in git stages files only"),
            ));
        }
        let entry = stage0(&index, path).cloned();
        let Some(version) = repo.worktree_version(path, entry.as_ref())? else {
            continue;
        };
        let oid = repo.write_blob(&version.content)?;
        let mut staged =
            grit_lib::index::entry_from_metadata(&meta, path.as_bytes(), oid, version.mode);
        if let Some(entry) = entry {
            // Keep what git keeps (skip-worktree and the like), less
            // intent-to-add, which staging fulfils.
            staged.flags_extended = entry
                .flags_extended
                .map(|flags| flags & !(1 << 13))
                .filter(|flags| *flags != 0);
        }
        index.stage_file(staged);
    }
    lock.write(&repo, index)
}

/// `git reset -- <paths>`.
pub fn reset(root: &Path, paths: &[String]) -> Result<()> {
    let repo = Repo::open(root)?;
    let lock = IndexLock::take(&repo)?;
    let mut index = repo.index()?;
    for path in paths {
        match repo.head_entry(path) {
            Some((oid, mode)) => {
                if stage0(&index, path).is_some_and(|entry| entry.oid == oid && entry.mode == mode)
                    && !is_unmerged(&index, path)
                {
                    continue;
                }
                index.remove_path_all_stages(path.as_bytes());
                index.add_or_replace(unstatted(path, oid, mode));
            }
            None => {
                index.remove_path_all_stages(path.as_bytes());
            }
        }
    }
    lock.write(&repo, index)
}

#[cfg(test)]
mod tests;
