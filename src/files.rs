//! Reading and rewriting text files in the worktree, for the in-page editor.

use std::path::Path;
use std::path::PathBuf;

use jj_lib::files::FileMergeHunkLevel;
use jj_lib::merge::Merge;
use jj_lib::merge::SameChange;
use jj_lib::tree_merge::MergeOptions;
use serde::Deserialize;

use crate::diff::FileDiff;
use crate::diff::text_diff;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use thiserror::Error;

/// Files beyond this size are left to a real editor.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FileError {
    /// The file is not one this editor can handle.
    #[error("{0}")]
    Unsupported(String),
    /// The file on disk is not the version the edit started from.
    #[error("{0}")]
    Changed(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, FileError>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Eol {
    Lf,
    Crlf,
}

#[derive(Debug, Serialize)]
pub struct TextFile {
    pub path: String,
    /// Content with `\n` line endings, whatever the file uses.
    pub content: String,
    /// Line endings to restore when saving.
    pub eol: Eol,
    /// Identifies this version of the file; sent back when saving.
    pub version: String,
}

fn version_of(bytes: &[u8]) -> String {
    hex::encode(&Sha256::digest(bytes)[..16])
}

/// Refuses anything but a plain root-relative path that stays clear of the
/// version control directories.
pub fn check_path(path: &str) -> Result<()> {
    let plain = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && part != ".git" && part != ".jj"
        });
    if plain {
        Ok(())
    } else {
        Err(FileError::Unsupported(format!(
            "{path}: not a path this editor may touch"
        )))
    }
}

/// Resolves a root-relative path to an existing regular file inside the root.
fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let unsupported = |why: &str| FileError::Unsupported(format!("{path}: {why}"));
    check_path(path)?;
    let file = root.join(path);
    let meta = std::fs::symlink_metadata(&file)?;
    if !meta.is_file() {
        return Err(unsupported("not a regular file"));
    }
    if meta.len() > MAX_BYTES {
        return Err(unsupported("too large to edit here"));
    }
    // A symlinked directory on the way must not lead out of the root.
    if !file.canonicalize()?.starts_with(root) {
        return Err(unsupported("outside the root"));
    }
    Ok(file)
}

pub fn read(root: &Path, path: &str) -> Result<TextFile> {
    let unsupported = |why: &str| FileError::Unsupported(format!("{path}: {why}"));
    let bytes = std::fs::read(resolve(root, path)?)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| unsupported("not UTF-8 text"))?;
    if text.contains('\0') {
        return Err(unsupported("binary content"));
    }
    // A text area only speaks `\n`, so other endings must be restorable.
    let newlines = text.matches('\n').count();
    let returns = text.matches('\r').count();
    let eol = if returns == 0 {
        Eol::Lf
    } else if returns == newlines && text.matches("\r\n").count() == newlines {
        Eol::Crlf
    } else {
        return Err(unsupported("mixed line endings; use your own editor"));
    };
    Ok(TextFile {
        path: path.to_owned(),
        content: text.replace("\r\n", "\n"),
        eol,
        version: version_of(&bytes),
    })
}

/// Replaces the content of an existing file and returns its new version.
/// Unless `base` is `None`, the file must still be at that version.
pub fn write(
    root: &Path,
    path: &str,
    content: &str,
    eol: Eol,
    base: Option<&str>,
) -> Result<String> {
    let file = resolve(root, path)?;
    if let Some(base) = base
        && version_of(&std::fs::read(&file)?) != base
    {
        return Err(FileError::Changed(format!(
            "{path} changed on disk after you opened it"
        )));
    }
    let content = content.replace("\r\n", "\n");
    let content = match eol {
        Eol::Lf => content,
        Eol::Crlf => content.replace('\n', "\r\n"),
    };
    // Written in place, so the file keeps its mode and identity.
    std::fs::write(&file, &content)?;
    Ok(version_of(content.as_bytes()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RebaseStatus {
    /// The file still has the content the edits started from.
    Unchanged,
    /// The edits and the change on disk do not overlap and can be combined.
    Merged,
    /// Both changed the same lines, so a person has to choose.
    Conflict,
}

/// How unsaved edits relate to the file as it is now, with the diffs a person
/// needs to judge it. All three diffs are against the content the edits
/// started from.
#[derive(Debug, Serialize)]
pub struct Rebase {
    pub status: RebaseStatus,
    /// The file on disk.
    pub file: TextFile,
    /// The edits carried over onto the file on disk, when they combine.
    pub content: Option<String>,
    /// What the edits change.
    pub mine: FileDiff,
    /// What changed on disk meanwhile.
    pub theirs: Option<FileDiff>,
    /// Both together.
    pub merged: Option<FileDiff>,
}

/// Works out a three-way merge of `mine` and the file on disk, both of which
/// started from `base`. Nothing is written.
pub fn rebase(root: &Path, path: &str, base: &str, mine: &str) -> Result<Rebase> {
    const CONTEXT: usize = 3;
    let file = read(root, path)?;
    let (base, mine) = (base.replace("\r\n", "\n"), mine.replace("\r\n", "\n"));
    let diff = |new: &str| text_diff(path, &base, new, CONTEXT);
    let mine_diff = diff(&mine);
    if file.content == base {
        return Ok(Rebase {
            status: RebaseStatus::Unchanged,
            file,
            content: None,
            mine: mine_diff,
            theirs: None,
            merged: None,
        });
    }
    let options = MergeOptions {
        hunk_level: FileMergeHunkLevel::Line,
        same_change: SameChange::Accept,
    };
    let content = {
        let theirs = file.content.as_str();
        let sides = Merge::from_removes_adds([base.as_str()], [mine.as_str(), theirs]);
        jj_lib::files::try_merge(&sides, &options)
            .and_then(|merged| String::from_utf8(merged.into()).ok())
    };
    Ok(Rebase {
        status: match content {
            Some(_) => RebaseStatus::Merged,
            None => RebaseStatus::Conflict,
        },
        theirs: Some(diff(&file.content)),
        merged: content.as_deref().map(diff),
        mine: mine_diff,
        content,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn round_trips_and_detects_changes() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "one\ntwo").unwrap();

        let file = read(&root, "src/a.txt").unwrap();
        assert_eq!((file.content.as_str(), file.eol), ("one\ntwo", Eol::Lf));

        let next = write(
            &root,
            "src/a.txt",
            "one\n2\n",
            file.eol,
            Some(&file.version),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "one\n2\n"
        );
        assert_eq!(read(&root, "src/a.txt").unwrap().version, next);

        // Saving from the stale version is refused, and changes nothing.
        let stale = write(&root, "src/a.txt", "mine", Eol::Lf, Some(&file.version));
        assert!(matches!(stale, Err(FileError::Changed(_))), "{stale:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "one\n2\n"
        );
        // Unless the caller insists.
        write(&root, "src/a.txt", "mine", Eol::Lf, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn rebases_edits_onto_a_changed_file() {
        let (_dir, root) = root();
        let base = "one\ntwo\nthree\nfour\nfive\n";
        let mine = "ONE\ntwo\nthree\nfour\nfive\n";
        std::fs::write(root.join("a.txt"), base).unwrap();
        let outcome = rebase(&root, "a.txt", base, mine).unwrap();
        assert_eq!(outcome.status, RebaseStatus::Unchanged);
        assert_eq!((outcome.mine.added, outcome.mine.removed), (1, 1));
        assert!(outcome.theirs.is_none() && outcome.merged.is_none());

        // Someone else changes the last line; the two edits do not touch.
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();
        let outcome = rebase(&root, "a.txt", base, mine).unwrap();
        assert_eq!(outcome.status, RebaseStatus::Merged);
        assert_eq!(
            outcome.content.as_deref(),
            Some("ONE\ntwo\nthree\nfour\nFIVE\n")
        );
        assert_eq!(outcome.file.content, "one\ntwo\nthree\nfour\nFIVE\n");
        // Each diff is against the common starting point.
        let changed = |diff: &FileDiff| -> Vec<String> {
            let lines = diff.hunks.iter().flat_map(|hunk| &hunk.lines);
            lines
                .filter(|line| line.kind == crate::diff::LineKind::Added)
                .map(|line| line.text.clone())
                .collect()
        };
        assert_eq!(changed(&outcome.mine), ["ONE"]);
        assert_eq!(changed(outcome.theirs.as_ref().unwrap()), ["FIVE"]);
        assert_eq!(changed(outcome.merged.as_ref().unwrap()), ["ONE", "FIVE"]);

        // Someone else changes the same line differently.
        std::fs::write(root.join("a.txt"), "uno\ntwo\nthree\nfour\nfive\n").unwrap();
        let outcome = rebase(&root, "a.txt", base, mine).unwrap();
        assert_eq!(outcome.status, RebaseStatus::Conflict);
        assert!(outcome.content.is_none() && outcome.merged.is_none());
        assert_eq!(changed(outcome.theirs.as_ref().unwrap()), ["uno"]);

        // Both made the very same change; nothing is left to carry over.
        std::fs::write(root.join("a.txt"), mine).unwrap();
        let outcome = rebase(&root, "a.txt", base, mine).unwrap();
        assert_eq!(outcome.status, RebaseStatus::Merged);
        assert_eq!(
            outcome.content.as_deref(),
            Some(outcome.file.content.as_str())
        );
        // Rebasing never writes.
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), mine);
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let (_dir, root) = root();
        std::fs::write(root.join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let file = read(&root, "a.txt").unwrap();
        assert_eq!((file.content.as_str(), file.eol), ("one\ntwo\n", Eol::Crlf));
        write(
            &root,
            "a.txt",
            "one\ntwo\nthree\n",
            file.eol,
            Some(&file.version),
        )
        .unwrap();
        let saved = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert_eq!(saved, "one\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn refuses_what_it_should_not_edit() {
        let (dir, root) = root();
        let root = root.join("root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("ws/.jj")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::write(root.join("ws/.jj/repo"), "x").unwrap();
        std::fs::write(root.join("blob"), [1u8, 0, 2]).unwrap();
        std::fs::write(root.join("latin1"), [0xe9u8, b'\n']).unwrap();
        std::fs::write(root.join("mixed"), "a\r\nb\n").unwrap();
        std::fs::write(dir.path().join("outside"), "secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), root.join("escape")).unwrap();
            std::os::unix::fs::symlink(dir.path().join("outside"), root.join("link")).unwrap();
        }
        for path in [
            "",
            "/etc/hosts",
            "../outside",
            ".git/config",
            "ws/.jj/repo",
            "blob",
            "latin1",
            "mixed",
            "missing",
            "ws",
            "escape/outside",
            "link",
        ] {
            assert!(read(&root, path).is_err(), "read {path:?}");
        }
        for path in [
            "../outside",
            ".git/config",
            "missing",
            "escape/outside",
            "link",
        ] {
            assert!(
                write(&root, path, "x", Eol::Lf, None).is_err(),
                "write {path:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("outside")).unwrap(),
            "secret"
        );
        assert!(!root.join("missing").exists());
    }
}
