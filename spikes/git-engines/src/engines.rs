//! The three index writes projj makes, by three engines: real git (the
//! reference), gitoxide and grit-lib.
//!
//! - `set`: stage exactly this content for a path, or take the path out of
//!   the index. This is what `git apply --cached` of a hunk, or of some of its
//!   lines, comes to: projj knows the content the index should end up with.
//! - `add`: `git add --all -- <paths>`, staging whole files from the worktree
//!   (clean filters, line endings, modes, deletions, conflicts).
//! - `reset`: `git reset -- <paths>`, putting the `HEAD` version back.

use std::io::Read as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

pub trait Engine {
    fn name(&self) -> &'static str;
    /// Stage `content` for `path`, or remove `path` (all stages) for `None`.
    /// `mode` applies to a path not yet in the index; otherwise the mode is kept.
    fn set(&self, repo: &Path, path: &str, content: Option<&[u8]>, mode: u32) -> Result<()>;
    fn add(&self, repo: &Path, paths: &[&str]) -> Result<()>;
    fn reset(&self, repo: &Path, paths: &[&str]) -> Result<()>;
}

// ---------------------------------------------------------------- real git

pub struct Git;

pub fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run git")?;
    if let Some(input) = input {
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

impl Engine for Git {
    fn name(&self) -> &'static str {
        "git"
    }

    fn set(&self, repo: &Path, path: &str, content: Option<&[u8]>, mode: u32) -> Result<()> {
        let Some(content) = content else {
            git(repo, &["update-index", "--force-remove", "--", path], None)?;
            return Ok(());
        };
        // No --path: the content is already as the index keeps it.
        let oid = git(repo, &["hash-object", "-w", "--stdin"], Some(content))?;
        let oid = String::from_utf8(oid)?.trim().to_owned();
        let staged = git(repo, &["ls-files", "--stage", "--", path], None)?;
        let mode = match String::from_utf8(staged)?.split_whitespace().next() {
            Some(existing) => existing.to_owned(),
            None => format!("{mode:o}"),
        };
        let info = format!("{mode},{oid},{path}");
        git(repo, &["update-index", "--add", "--cacheinfo", &info], None)?;
        Ok(())
    }

    fn add(&self, repo: &Path, paths: &[&str]) -> Result<()> {
        git(repo, &[&["add", "--all", "--"], paths].concat(), None)?;
        Ok(())
    }

    fn reset(&self, repo: &Path, paths: &[&str]) -> Result<()> {
        git(repo, &[&["reset", "--quiet", "--"], paths].concat(), None)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- gitoxide

/// `keep_tree` leaves the cache-tree as read: wrong, and here only to show
/// that the comparison catches a stale cache-tree.
pub struct Gix {
    pub keep_tree: bool,
}

mod gx {
    pub use gix::index::entry::Flags;
    pub use gix::index::entry::Mode;
    pub use gix::index::entry::Stage;
    pub use gix::index::entry::Stat;
}

fn gix_mode(mode: u32) -> gx::Mode {
    match mode {
        0o100755 => gx::Mode::FILE_EXECUTABLE,
        0o120000 => gx::Mode::SYMLINK,
        0o160000 => gx::Mode::COMMIT,
        _ => gx::Mode::FILE,
    }
}

/// Puts `id` at stage 0 of `path`, dropping any conflict stages, keeping the
/// entry's other flags.
fn gix_put(
    index: &mut gix::index::File,
    path: &str,
    id: gix::ObjectId,
    mode: Option<gx::Mode>,
    stat: gx::Stat,
) {
    let path = gix::bstr::BStr::new(path);
    index.remove_entries(|_, p, entry| p == path && entry.stage() != gx::Stage::Unconflicted);
    match index.entry_mut_by_path_and_stage(path, gx::Stage::Unconflicted) {
        Some(entry) => {
            entry.id = id;
            if let Some(mode) = mode {
                entry.mode = mode;
            }
            entry.stat = stat;
            entry.flags.remove(gx::Flags::INTENT_TO_ADD);
        }
        None => {
            index.dangerously_push_entry(
                stat,
                id,
                gx::Flags::empty(),
                mode.unwrap_or(gx::Mode::FILE),
                path,
            );
            index.sort_entries();
        }
    }
}

fn gix_remove(index: &mut gix::index::File, path: &str) {
    let path = gix::bstr::BStr::new(path);
    index.remove_entries(|_, p, _| p == path);
}

/// gitoxide writes the cache-tree back as it was read, so it goes: git
/// rebuilds it when next needed.
fn gix_write(mut index: gix::index::File, keep_tree: bool) -> Result<()> {
    if !keep_tree {
        index.remove_tree();
    }
    index.write(Default::default())?;
    Ok(())
}

impl Engine for Gix {
    fn name(&self) -> &'static str {
        if self.keep_tree {
            "control"
        } else {
            "gitoxide"
        }
    }

    fn set(&self, repo: &Path, path: &str, content: Option<&[u8]>, mode: u32) -> Result<()> {
        let repo = gix::open(repo)?;
        let mut index = repo.open_index()?;
        match content {
            Some(content) => {
                let id = repo.write_blob(content)?.detach();
                let exists = index
                    .entry_by_path_and_stage(gix::bstr::BStr::new(path), gx::Stage::Unconflicted)
                    .is_some();
                let mode = (!exists).then(|| gix_mode(mode));
                gix_put(&mut index, path, id, mode, gx::Stat::default());
            }
            None => gix_remove(&mut index, path),
        }
        gix_write(index, self.keep_tree)
    }

    fn add(&self, repo_dir: &Path, paths: &[&str]) -> Result<()> {
        let repo = gix::open(repo_dir)?;
        let mut index = repo.open_index()?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let workdir = repo.workdir().context("no worktree")?.to_owned();
        for &path in paths {
            let file = workdir.join(path);
            let meta = match std::fs::symlink_metadata(&file) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    gix_remove(&mut index, path);
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let (content, mode) = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&file)?;
                (
                    target.into_os_string().into_encoded_bytes(),
                    gx::Mode::SYMLINK,
                )
            } else {
                use std::os::unix::fs::PermissionsExt as _;
                let executable = meta.permissions().mode() & 0o111 != 0;
                let mode = if executable {
                    gx::Mode::FILE_EXECUTABLE
                } else {
                    gx::Mode::FILE
                };
                let mut content = Vec::new();
                match pipeline.convert_to_git(
                    std::fs::File::open(&file)?,
                    Path::new(path),
                    &index,
                )? {
                    gix::filter::plumbing::pipeline::convert::ToGitOutcome::Unchanged(mut read) => {
                        read.read_to_end(&mut content)?;
                    }
                    gix::filter::plumbing::pipeline::convert::ToGitOutcome::Buffer(buffer) => {
                        content.extend_from_slice(buffer);
                    }
                    gix::filter::plumbing::pipeline::convert::ToGitOutcome::Process(mut read) => {
                        read.read_to_end(&mut content)?;
                    }
                }
                (content, mode)
            };
            let id = repo.write_blob(&content)?.detach();
            let stat = gx::Stat::from_fs(&gix::index::fs::Metadata::from_path_no_follow(&file)?)?;
            gix_put(&mut index, path, id, Some(mode), stat);
        }
        gix_write(index, self.keep_tree)
    }

    fn reset(&self, repo_dir: &Path, paths: &[&str]) -> Result<()> {
        let repo = gix::open(repo_dir)?;
        let mut index = repo.open_index()?;
        let tree = repo.head_tree()?;
        for &path in paths {
            match tree.lookup_entry_by_path(path)? {
                Some(entry) => {
                    let mode = gix_mode(u32::from(entry.mode().value()));
                    gix_put(
                        &mut index,
                        path,
                        entry.object_id(),
                        Some(mode),
                        gx::Stat::default(),
                    );
                }
                None => gix_remove(&mut index, path),
            }
        }
        gix_write(index, self.keep_tree)
    }
}

// ---------------------------------------------------------------- grit-lib

pub struct Grit;

use grit_lib::index::Index;
use grit_lib::index::IndexEntry;
use grit_lib::repo::Repository;

fn grit_entry(path: &str, oid: grit_lib::objects::ObjectId, mode: u32) -> IndexEntry {
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

fn grit_open(repo: &Path) -> Result<(Repository, Index)> {
    let repo = Repository::discover(Some(repo))?;
    let index = repo.load_index()?;
    Ok((repo, index))
}

impl Engine for Grit {
    fn name(&self) -> &'static str {
        "grit-lib"
    }

    fn set(&self, repo_dir: &Path, path: &str, content: Option<&[u8]>, mode: u32) -> Result<()> {
        let (repo, mut index) = grit_open(repo_dir)?;
        match content {
            Some(content) => {
                let oid = repo
                    .odb
                    .write(grit_lib::objects::ObjectKind::Blob, content)?;
                let mode = index
                    .get(path.as_bytes(), 0)
                    .map_or(mode, |entry| entry.mode);
                index.stage_file(grit_entry(path, oid, mode));
            }
            None => {
                index.remove_path_all_stages(path.as_bytes());
            }
        }
        repo.write_index(&mut index)?;
        Ok(())
    }

    fn add(&self, repo_dir: &Path, paths: &[&str]) -> Result<()> {
        let (repo, mut index) = grit_open(repo_dir)?;
        let workdir = repo.work_tree.clone().context("no worktree")?;
        let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)?;
        let conversion = grit_lib::crlf::ConversionConfig::from_config(&config);
        let rules = grit_lib::crlf::load_gitattributes(&workdir);
        for &path in paths {
            let file = workdir.join(path);
            let meta = match std::fs::symlink_metadata(&file) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    index.remove_path_all_stages(path.as_bytes());
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let (content, mode) = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&file)?;
                (
                    target.into_os_string().into_encoded_bytes(),
                    grit_lib::index::MODE_SYMLINK,
                )
            } else {
                use std::os::unix::fs::PermissionsExt as _;
                let executable = meta.permissions().mode() & 0o111 != 0;
                let attrs = grit_lib::crlf::get_file_attrs(&rules, path, false, &config);
                let raw = std::fs::read(&file)?;
                let content = grit_lib::crlf::convert_to_git(&raw, path, &conversion, &attrs)
                    .map_err(anyhow::Error::msg)?;
                let mode = if executable {
                    grit_lib::index::MODE_EXECUTABLE
                } else {
                    grit_lib::index::MODE_REGULAR
                };
                (content, mode)
            };
            let oid = repo
                .odb
                .write(grit_lib::objects::ObjectKind::Blob, &content)?;
            index.stage_file(grit_lib::index::entry_from_metadata(
                &meta,
                path.as_bytes(),
                oid,
                mode,
            ));
        }
        repo.write_index(&mut index)?;
        Ok(())
    }

    fn reset(&self, repo_dir: &Path, paths: &[&str]) -> Result<()> {
        let (repo, mut index) = grit_open(repo_dir)?;
        for &path in paths {
            match grit_lib::rev_parse::resolve_treeish_blob_at_path(&repo, &format!("HEAD:{path}"))
            {
                Ok(found) => {
                    let mode = u32::from_str_radix(&found.mode, 8)?;
                    index.stage_file(grit_entry(path, found.oid, mode));
                }
                Err(_) => {
                    index.remove_path_all_stages(path.as_bytes());
                }
            }
        }
        repo.write_index(&mut index)?;
        Ok(())
    }
}
