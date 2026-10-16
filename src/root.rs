//! Discovery of "projj-style" roots: a git repository for human review at the
//! top, with jj workspaces for agents anywhere beneath it. By convention those
//! live under `track/<host>/<tenant>/<repo>.git/` as `sts-N/` or `lts-N/`
//! workspaces that share the repository stored in a sibling `.repo/`.
//!
//! Any other git repository can be reviewed too; it just has no workspaces
//! until it grows a `track/`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use serde::Serialize;

use crate::git::Engine;

pub const TRACK_DIR: &str = "track";
const JJ_DIR: &str = ".jj";
/// Conventional name of the directory holding a repository's shared store.
const STORE_DIR: &str = ".repo";
/// Path of the workspace that is the root directory itself.
pub const ROOT_WORKSPACE: &str = ".";

#[derive(Clone, Debug, Serialize)]
pub struct Workspace {
    /// Directory name, e.g. `sts-0`.
    pub name: String,
    /// Path relative to the root, with forward slashes; `.` for the root
    /// itself. Used as the API key.
    pub path: String,
    /// Why the workspace cannot be opened, if its `.jj` is incomplete.
    pub problem: Option<String>,
}

/// One jj repository and the workspaces of it that git does not ignore.
#[derive(Clone, Debug, Serialize)]
pub struct Repo {
    /// Short label, e.g. `github.com/modial/basinsync.git`.
    pub id: String,
    /// Where the repository itself (history, operations) is kept: relative to
    /// the root when inside it, absolute otherwise.
    pub path: String,
    /// The repository lives in a directory that is not one of `workspaces`,
    /// because git ignores it or it is outside the root. That is the norm for
    /// a `.repo/` store.
    pub detached: bool,
    pub workspaces: Vec<Workspace>,
}

/// Finds the root containing `start` by asking git for the enclosing
/// repository. A repository without `track/` is something nested inside a
/// root (the git store that backs a `.repo`, a vendored clone), so the search
/// carries on from its parent.
pub fn find_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", start.display()))?;
    let mut dir = start.as_path();
    let mut toplevel;
    loop {
        toplevel = match git_toplevel(dir)? {
            Some(toplevel) => toplevel,
            None => bail!(
                "{} is not inside a projj root (a git repository with a {TRACK_DIR}/ directory)",
                start.display()
            ),
        };
        if toplevel.join(TRACK_DIR).is_dir() {
            return Ok(toplevel);
        }
        dir = match toplevel.parent() {
            Some(parent) => parent,
            None => bail!("no projj root at or above {}", start.display()),
        };
    }
}

/// A root found for a path: a projj root (with `track/`) when there is one
/// around the path, otherwise the innermost git repository holding it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resolved {
    pub root: PathBuf,
    /// The root has a `track/` directory.
    pub projj: bool,
}

/// Finds the root to review for `start`, or `None` if it is in no git
/// repository. A path inside a `.git` directory counts as the repository's.
pub fn resolve(start: &Path) -> Result<Option<Resolved>> {
    let start = outside_git_dir(
        &start
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", start.display()))?,
    );
    if !start.is_dir() {
        bail!("{} is not a folder", start.display());
    }
    let mut innermost = None;
    let mut dir = start;
    while let Some(toplevel) = git_toplevel(&dir)? {
        if toplevel.join(TRACK_DIR).is_dir() {
            return Ok(Some(Resolved {
                root: toplevel,
                projj: true,
            }));
        }
        let Some(parent) = toplevel.parent().map(Path::to_owned) else {
            break;
        };
        innermost.get_or_insert(toplevel);
        dir = parent;
    }
    Ok(innermost.map(|root| Resolved { root, projj: false }))
}

/// `path` cut short before any `.git` component: the repository it belongs to.
fn outside_git_dir(path: &Path) -> PathBuf {
    let mut kept = PathBuf::new();
    for component in path.components() {
        if component.as_os_str() == ".git" {
            break;
        }
        kept.push(component);
    }
    kept
}

/// What a folder someone has typed or chosen is, as far as opening it goes.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Place {
    /// The folder is a root, or is inside one: that root is what opens.
    Root {
        #[serde(flatten)]
        resolved: Resolved,
        /// The folder is somewhere inside the root rather than the root.
        inside: bool,
    },
    /// A folder in no git repository: a new root can be started in it.
    Folder,
    /// Nothing is there. A new root can be started there if the folder it
    /// would go in exists.
    Missing { creatable: bool },
    /// A file rather than a folder.
    File,
}

/// The path someone means (with `~` for their home), and what is there.
pub fn inspect(path: &Path) -> Result<(PathBuf, Place)> {
    let path = expand_home(path);
    if !path.is_absolute() {
        bail!("{} is not a full path", path.display());
    }
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let creatable = path.parent().is_some_and(Path::is_dir);
            return Ok((path, Place::Missing { creatable }));
        }
        Err(err) => return Err(err).with_context(|| format!("cannot look at {}", path.display())),
    };
    if !meta.is_dir() {
        return Ok((path, Place::File));
    }
    let path = outside_git_dir(&path.canonicalize()?);
    let place = match resolve(&path)? {
        Some(resolved) => Place::Root {
            inside: resolved.root != path,
            resolved,
        },
        None => Place::Folder,
    };
    Ok((path, place))
}

/// `~` and `~/...` as the home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match path.strip_prefix("~") {
        Ok(rest) => match home() {
            Some(home) => home.join(rest),
            None => path.to_owned(),
        },
        Err(_) => path.to_owned(),
    }
}

/// Starts a new projj root at `path`: a git repository with an empty
/// `track/` for jj repositories to go in. Makes the folder first if it is
/// missing and the folder it goes in exists. Refuses a folder that is
/// already in a git repository.
pub fn init(path: &Path) -> Result<PathBuf> {
    init_with(path, Engine::detect())
}

fn init_with(path: &Path, engine: Engine) -> Result<PathBuf> {
    let (path, place) = inspect(path)?;
    match place {
        Place::Folder => {}
        Place::Missing { creatable: true } => {
            let parent = path.parent().unwrap_or(&path);
            if let Some(resolved) = resolve(parent)? {
                bail!(
                    "{} would be inside the git repository at {}",
                    path.display(),
                    resolved.root.display()
                );
            }
            std::fs::create_dir(&path)
                .with_context(|| format!("cannot make the folder {}", path.display()))?
        }
        Place::Missing { creatable: false } => {
            bail!(
                "{} does not exist",
                path.parent().unwrap_or(&path).display()
            )
        }
        Place::File => bail!("{} is a file", path.display()),
        Place::Root { resolved, .. } => bail!(
            "{} is already in the git repository at {}",
            path.display(),
            resolved.root.display()
        ),
    }
    let path = path.canonicalize()?;
    if engine == Engine::BuiltIn {
        grit_lib::repo::init_repository(&path, false, "main", None, "files")
            .map_err(|err| anyhow::anyhow!("cannot start a git repository: {err}"))?;
    } else {
        let output = git(&path)
            .args(["init", "--quiet"])
            .output()
            .context("cannot run git")?;
        if !output.status.success() {
            bail!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    std::fs::create_dir_all(path.join(TRACK_DIR))
        .with_context(|| format!("cannot make {TRACK_DIR}/ in {}", path.display()))?;
    Ok(path)
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

/// The working tree git reports for `dir`, or `None` outside any repository.
fn git_toplevel(dir: &Path) -> Result<Option<PathBuf>> {
    if Engine::detect() == Engine::BuiltIn {
        return Ok(crate::git_builtin::toplevel(dir));
    }
    let output = git(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim_end_matches(['\n', '\r']);
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

/// Finds every jj workspace under the root that git does not ignore, grouped
/// by the repository each belongs to.
///
/// Git does the walking, so its ignore rules (`.gitignore` at every level,
/// `.git/info/exclude`, the user's global excludes) decide what is looked at:
/// nothing inside an ignored directory is found, including a workspace that
/// is ignored itself. Git also stops at a nested git repository, so a
/// colocated jj repository is found but nothing further inside it is.
pub fn discover(root: &Path) -> Result<Vec<Repo>> {
    let mut repos: BTreeMap<String, Repo> = BTreeMap::new();
    let workspace_paths = workspace_paths(root)?;
    for path in &workspace_paths {
        let dir = root.join(path);
        let (location, problem) = match repo_location(&dir) {
            Ok(location) => (location, None),
            Err(err) => (dir.clone(), Some(format!("{err:#}"))),
        };
        let location = match location.strip_prefix(root) {
            Ok(inside) if inside.as_os_str().is_empty() => ROOT_WORKSPACE.to_owned(),
            Ok(inside) => slashed(inside),
            Err(_) => location.display().to_string(),
        };
        let repo = repos.entry(location.clone()).or_insert_with(|| Repo {
            id: repo_id(&location),
            detached: !workspace_paths.contains(&location),
            path: location,
            workspaces: vec![],
        });
        repo.workspaces.push(Workspace {
            name: match dir.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => path.clone(),
            },
            path: path.clone(),
            problem,
        });
    }
    let mut repos: Vec<Repo> = repos.into_values().collect();
    repos.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(repos)
}

/// Root-relative paths of the directories that hold a `.jj/` git can see.
fn workspace_paths(root: &Path) -> Result<BTreeSet<String>> {
    // A `.jj` is normally ignored, either by a rule for `.jj/` or by the
    // `.jj/.gitignore` jj writes, so it shows up among the ignored paths. Git
    // lists an ignored directory as one entry and does not descend into it,
    // which is what keeps `node_modules/`, `target/` and stores out of this.
    let ignored = ["--others", "--ignored", "--exclude-standard", "--directory"];
    // A `.jj` nobody has ignored yet shows up as ordinary untracked files,
    // and a directory with its own `.git` as a single untracked entry.
    let untracked = ["--others", "--exclude-standard"];
    let mut paths = BTreeSet::new();
    for filter in [&ignored[..], &untracked[..]] {
        let listed: Vec<String> = if Engine::detect() == Engine::BuiltIn {
            crate::git_builtin::others(root, filter.contains(&"--ignored"))?
        } else {
            let output = git(root)
                .args(["ls-files", "-z"])
                .args(filter)
                .output()
                .context("cannot run git")?;
            if !output.status.success() {
                bail!(
                    "git ls-files failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            output
                .stdout
                .split(|byte| *byte == 0)
                .map(|entry| String::from_utf8_lossy(entry).into_owned())
                .collect()
        };
        for entry in listed {
            // Git names a nested git repository without looking inside it,
            // which is how a colocated jj repository appears.
            let nested_repo = entry
                .strip_suffix('/')
                .filter(|_| !filter.contains(&"--directory"));
            let parent = match nested_repo {
                Some(dir) => Some(dir.to_owned()),
                None => jj_parent(&entry),
            };
            if let Some(parent) = parent
                && root.join(&parent).join(JJ_DIR).is_dir()
            {
                paths.insert(parent);
            }
        }
    }
    Ok(paths)
}

/// The directory owning the outermost `.jj` on `path`, if there is one.
fn jj_parent(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().position(|part| *part == JJ_DIR)?;
    Some(match at {
        0 => ROOT_WORKSPACE.to_owned(),
        _ => parts[..at].join("/"),
    })
}

/// The directory whose `.jj/repo` holds the repository that the workspace at
/// `dir` belongs to: `dir` itself, or wherever its `.jj/repo` file points.
fn repo_location(dir: &Path) -> Result<PathBuf> {
    let jj_dir = dir.join(JJ_DIR);
    let pointer = jj_dir.join("repo");
    if pointer.is_dir() {
        return Ok(dir.to_owned());
    }
    if !pointer.is_file() {
        bail!("{} is missing", pointer.display());
    }
    let target =
        std::fs::read(&pointer).with_context(|| format!("cannot read {}", pointer.display()))?;
    let target = String::from_utf8_lossy(&target);
    let target = jj_dir.join(target.trim_end_matches(['\n', '\r']));
    let repo = target.canonicalize().with_context(|| {
        format!(
            "{} points to {}, which does not exist",
            pointer.display(),
            target.display()
        )
    })?;
    // Normally `<owner>/.jj/repo`; name the owner rather than the internals.
    let owner = if repo.ends_with(Path::new(JJ_DIR).join("repo")) {
        repo.parent().and_then(Path::parent)
    } else {
        None
    };
    Ok(owner.unwrap_or(&repo).to_owned())
}

/// Labels a repository by where it is kept, dropping the parts of the
/// conventional layout that carry no information.
fn repo_id(location: &str) -> String {
    let base = location
        .strip_suffix(&format!("/{STORE_DIR}"))
        .unwrap_or(location);
    base.strip_prefix(&format!("{TRACK_DIR}/"))
        .unwrap_or(base)
        .to_owned()
}

fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Finds the innermost workspace containing `path` (a root-relative file
/// path).
pub fn workspace_for<'a>(repos: &'a [Repo], path: &str) -> Option<&'a Workspace> {
    repos
        .iter()
        .flat_map(|repo| &repo.workspaces)
        .filter(|workspace| {
            workspace.path == ROOT_WORKSPACE
                || path
                    .strip_prefix(&workspace.path)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .max_by_key(|workspace| match workspace.path.as_str() {
            ROOT_WORKSPACE => 0,
            path => path.len(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let status = git(dir).args(["init", "--quiet"]).status().unwrap();
        assert!(status.success());
    }

    /// Lays out the on-disk shape of a jj repository that stores itself.
    fn jj_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".jj/repo")).unwrap();
        std::fs::create_dir_all(dir.join(".jj/working_copy")).unwrap();
        // jj writes this so that git ignores everything inside.
        std::fs::write(dir.join(".jj/.gitignore"), "/*\n").unwrap();
    }

    /// Lays out the shape of an extra workspace of the repository at `repo`.
    fn jj_workspace(dir: &Path, pointer: &str) {
        std::fs::create_dir_all(dir.join(".jj/working_copy")).unwrap();
        std::fs::write(dir.join(".jj/repo"), pointer).unwrap();
        std::fs::write(dir.join(".jj/working_copy/type"), "local").unwrap();
    }

    fn summary(repos: &[Repo]) -> Vec<(String, String, bool, Vec<String>)> {
        repos
            .iter()
            .map(|repo| {
                let workspaces = repo.workspaces.iter().map(|ws| ws.path.clone()).collect();
                (
                    repo.id.clone(),
                    repo.path.clone(),
                    repo.detached,
                    workspaces,
                )
            })
            .collect()
    }

    fn scratch_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        git_init(&root);
        std::fs::create_dir_all(root.join(TRACK_DIR)).unwrap();
        (dir, root)
    }

    #[test]
    fn finds_the_root_through_nested_repositories() {
        let (_dir, root) = scratch_root();
        let repo = root.join("track/example.com/acme/widgets.git");
        // The store is a git repository of its own; the workspace is not.
        git_init(&repo.join(".repo"));
        std::fs::create_dir_all(repo.join("sts-0/src/deep")).unwrap();
        for start in [
            root.clone(),
            repo.join("sts-0/src/deep"),
            repo.join(".repo"),
        ] {
            assert_eq!(find_root(&start).unwrap(), root, "{}", start.display());
        }
    }

    #[test]
    fn rejects_directories_outside_any_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_root(dir.path()).is_err());
        // A git repository without track/ is not a root either.
        git_init(&dir.path().join("plain"));
        assert!(find_root(&dir.path().join("plain")).is_err());
    }

    #[test]
    fn discovers_workspaces_git_does_not_ignore() {
        let (dir, root) = scratch_root();
        std::fs::write(
            root.join(".gitignore"),
            ".repo/\nnode_modules/\n/scratch/\n",
        )
        .unwrap();

        // The conventional layout: an ignored store with two workspaces.
        let widgets = root.join("track/example.com/acme/widgets.git");
        jj_repo(&widgets.join(".repo"));
        jj_workspace(&widgets.join("sts-0"), "../../.repo/.jj/repo");
        jj_workspace(&widgets.join("lts-1"), "../../.repo/.jj/repo");
        // A repository of its own, outside track/, that is not ignored.
        jj_repo(&root.join("tools/solo"));
        // A workspace of a repository kept outside the root.
        let elsewhere = dir.path().canonicalize().unwrap().join("elsewhere");
        jj_repo(&elsewhere);
        jj_workspace(
            &root.join("vendor/far"),
            &format!("{}/.jj/repo", elsewhere.display()),
        );
        // A colocated repository: git sees only the directory.
        git_init(&root.join("tools/colocated"));
        jj_repo(&root.join("tools/colocated"));
        jj_repo(&root.join("tools/colocated/inner"));
        // A workspace whose repository is gone.
        jj_workspace(&root.join("vendor/orphan"), "../../nowhere/.jj/repo");
        // Workspaces inside ignored directories are not looked for.
        jj_repo(&root.join("node_modules/dep"));
        jj_repo(&root.join("scratch"));
        jj_workspace(&root.join("tools/solo/node_modules/inner"), "../x");

        let repos = discover(&root).unwrap();
        let elsewhere = elsewhere.display().to_string();
        let own = |text: &str| text.to_owned();
        assert_eq!(
            summary(&repos),
            [
                (elsewhere.clone(), elsewhere, true, vec![own("vendor/far")]),
                (
                    own("example.com/acme/widgets.git"),
                    own("track/example.com/acme/widgets.git/.repo"),
                    true,
                    vec![
                        own("track/example.com/acme/widgets.git/lts-1"),
                        own("track/example.com/acme/widgets.git/sts-0"),
                    ],
                ),
                (
                    own("tools/colocated"),
                    own("tools/colocated"),
                    false,
                    vec![own("tools/colocated")]
                ),
                (
                    own("tools/solo"),
                    own("tools/solo"),
                    false,
                    vec![own("tools/solo")]
                ),
                (
                    own("vendor/orphan"),
                    own("vendor/orphan"),
                    false,
                    vec![own("vendor/orphan")]
                ),
            ]
        );
        let orphan = &repos[4].workspaces[0];
        assert!(orphan.problem.as_ref().unwrap().contains("does not exist"));
        assert!(repos[1].workspaces[0].problem.is_none());
        assert_eq!(repos[1].workspaces[1].name, "sts-0");

        let inside = "track/example.com/acme/widgets.git/lts-1/src/a.rs";
        assert_eq!(workspace_for(&repos, inside).unwrap().name, "lts-1");
        assert!(workspace_for(&repos, "README.md").is_none());
        assert!(workspace_for(&repos, "tools/solo-notes.md").is_none());
        // Git reports a nested repository as the directory itself.
        let nested = workspace_for(&repos, "tools/colocated").unwrap();
        assert_eq!(nested.path, "tools/colocated");
    }

    #[test]
    fn treats_the_root_itself_as_a_workspace() {
        let (_dir, root) = scratch_root();
        std::fs::write(root.join(".gitignore"), ".jj/\n").unwrap();
        jj_repo(&root);
        jj_repo(&root.join("track/nested"));

        let repos = discover(&root).unwrap();
        assert_eq!(
            summary(&repos),
            [
                (".".to_owned(), ".".to_owned(), false, vec![".".to_owned()]),
                (
                    "nested".to_owned(),
                    "track/nested".to_owned(),
                    false,
                    vec!["track/nested".to_owned()]
                ),
            ]
        );
        assert_eq!(workspace_for(&repos, "README.md").unwrap().path, ".");
        let nested = workspace_for(&repos, "track/nested/a.rs").unwrap();
        assert_eq!(nested.path, "track/nested");
    }

    #[test]
    fn resolves_projj_roots_then_plain_repositories() {
        let (dir, root) = scratch_root();
        let store = root.join("track/example.com/acme/widgets.git/.repo");
        git_init(&store);
        let projj = |root: &Path| Resolved {
            root: root.to_owned(),
            projj: true,
        };
        assert_eq!(resolve(&store).unwrap(), Some(projj(&root)));
        assert_eq!(
            resolve(&root.join(".git/refs")).unwrap(),
            Some(projj(&root))
        );

        let plain = dir.path().canonicalize().unwrap().join("plain");
        git_init(&plain.join("inner"));
        git_init(&plain);
        std::fs::create_dir_all(plain.join("src")).unwrap();
        let resolved = resolve(&plain.join("src")).unwrap().unwrap();
        assert_eq!((resolved.root, resolved.projj), (plain.clone(), false));
        // The innermost repository, when none has a track/.
        let inner = resolve(&plain.join("inner")).unwrap().unwrap();
        assert_eq!(inner.root, plain.join("inner"));

        let loose = dir.path().canonicalize().unwrap().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(resolve(&loose).unwrap(), None);
    }

    #[test]
    fn inspects_what_someone_typed() {
        let (dir, root) = scratch_root();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("track/deep")).unwrap();
        std::fs::create_dir_all(base.join("loose")).unwrap();
        std::fs::write(base.join("file.txt"), "").unwrap();

        let (path, place) = inspect(&root.join("track/deep")).unwrap();
        assert_eq!(path, root.join("track/deep"));
        assert!(matches!(place, Place::Root { inside: true, resolved } if resolved.root == root));
        let (_, place) = inspect(&root.join(".git")).unwrap();
        assert!(matches!(place, Place::Root { inside: false, .. }));
        assert_eq!(inspect(&base.join("loose")).unwrap().1, Place::Folder);
        assert_eq!(inspect(&base.join("file.txt")).unwrap().1, Place::File);
        assert_eq!(
            inspect(&base.join("new")).unwrap().1,
            Place::Missing { creatable: true }
        );
        assert_eq!(
            inspect(&base.join("no/such")).unwrap().1,
            Place::Missing { creatable: false }
        );
        assert!(inspect(Path::new("relative/path")).is_err());
    }

    #[test]
    fn starts_new_roots() {
        for engine in [Engine::Installed, Engine::BuiltIn] {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            std::fs::create_dir_all(base.join("existing")).unwrap();
            for path in [base.join("existing"), base.join("fresh")] {
                let root = init_with(&path, engine).unwrap();
                assert_eq!(root, path);
                assert!(root.join(TRACK_DIR).is_dir());
                let output = git(&root).args(["status", "--porcelain"]).output().unwrap();
                assert!(output.status.success(), "{engine:?}");
                assert_eq!(find_root(&root).unwrap(), root);
            }
            // Never inside another repository, nor where there is no folder.
            std::fs::create_dir_all(base.join("fresh/sub")).unwrap();
            let err = init_with(&base.join("fresh/sub"), engine).unwrap_err();
            assert!(err.to_string().contains("already in the git repository"));
            let err = init_with(&base.join("fresh/inner"), engine).unwrap_err();
            assert!(err.to_string().contains("would be inside"));
            assert!(init_with(&base.join("no/such"), engine).is_err());
        }
    }
}
