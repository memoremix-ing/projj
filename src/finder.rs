//! Finding git repositories on the device, for someone choosing a root.
//!
//! The home folder is walked once in the background, the first time anyone
//! asks, and again when asked after a while. Shallow folders are looked at
//! first, so the repositories people keep near the top show up at once.
//! Nothing inside a repository is looked at (its nested repositories, a
//! `.repo` store, are not roots to choose), nor hidden folders, nor the
//! places that hold many folders and never a repository.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;

use crate::root::TRACK_DIR;

/// How far below a starting folder repositories are looked for.
const MAX_DEPTH: usize = 7;
/// How many folders one walk looks into at most.
const MAX_FOLDERS: usize = 250_000;

/// Folders that hold many folders and no repository worth offering.
const SKIP: &[&str] = &[
    "node_modules",
    "bower_components",
    "target",
    "build",
    "dist",
    "__pycache__",
    "venv",
    "site-packages",
    "Pods",
    "DerivedData",
    // macOS: application data and media libraries.
    "Library",
    "Applications",
    "Pictures",
    "Music",
    "Movies",
];

/// Packages that look like folders on macOS.
const SKIP_SUFFIXES: &[&str] = &[
    ".app",
    ".bundle",
    ".framework",
    ".photoslibrary",
    ".musiclibrary",
    ".noindex",
    ".xcarchive",
];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Found {
    pub path: PathBuf,
    /// The repository has a `track/` directory: it is a projj root.
    pub projj: bool,
}

#[derive(Default)]
struct Walks {
    /// What the last finished walk found.
    found: Vec<Found>,
    /// What the walk under way has found so far.
    finding: Vec<Found>,
    scanning: bool,
    looked_at: usize,
    finished: Option<Instant>,
}

pub struct Finder {
    dirs: Vec<PathBuf>,
    walks: Mutex<Walks>,
}

/// Where a walk starts: `PROJJ_SEARCH_DIRS` (a list like `PATH`), or else
/// the home folder.
pub fn search_dirs() -> Vec<PathBuf> {
    if let Some(dirs) = std::env::var_os("PROJJ_SEARCH_DIRS").filter(|dirs| !dirs.is_empty()) {
        return std::env::split_paths(&dirs).collect();
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

/// What a walk has turned up, for the moment.
pub struct Progress {
    pub found: Vec<Found>,
    pub scanning: bool,
    pub looked_at: usize,
}

impl Finder {
    pub fn new(dirs: Vec<PathBuf>) -> Arc<Self> {
        // Resolved, so that a repository is named as it is everywhere else
        // (`/private/tmp`, not `/tmp`) and found once.
        let dirs = dirs
            .into_iter()
            .map(|dir| dir.canonicalize().unwrap_or(dir))
            .collect();
        Arc::new(Self {
            dirs,
            walks: Mutex::default(),
        })
    }

    /// Starts a walk, unless one is under way or one finished less than
    /// `fresh` ago.
    pub fn look(self: &Arc<Self>, fresh: Duration) {
        let mut walks = self.walks.lock().unwrap();
        if walks.scanning || walks.finished.is_some_and(|at| at.elapsed() < fresh) {
            return;
        }
        walks.scanning = true;
        walks.looked_at = 0;
        walks.finding.clear();
        drop(walks);
        let finder = self.clone();
        std::thread::spawn(move || {
            finder.walk();
            let mut walks = finder.walks.lock().unwrap();
            walks.found = std::mem::take(&mut walks.finding);
            walks.scanning = false;
            walks.finished = Some(Instant::now());
        });
    }

    /// Everything found, by the last walk and the one under way.
    pub fn progress(&self) -> Progress {
        let walks = self.walks.lock().unwrap();
        let mut found = walks.found.clone();
        if walks.scanning {
            let known: BTreeSet<PathBuf> = found.iter().map(|f| f.path.clone()).collect();
            found.extend(
                walks
                    .finding
                    .iter()
                    .filter(|f| !known.contains(&f.path))
                    .cloned(),
            );
        }
        Progress {
            found,
            scanning: walks.scanning,
            looked_at: walks.looked_at,
        }
    }

    fn walk(&self) {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        for dir in &self.dirs {
            // A home folder kept in git (dotfiles) is still looked through.
            if let Some(found) = repository(dir) {
                self.found(found);
            }
            queue.push_back((dir.clone(), 0));
        }
        let mut looked_at = 0;
        while let Some((dir, depth)) = queue.pop_front() {
            looked_at += 1;
            if looked_at > MAX_FOLDERS {
                break;
            }
            if looked_at % 500 == 0 {
                self.walks.lock().unwrap().looked_at = looked_at;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries
                .flatten()
                // Symbolic links are not followed: no loops, no detours.
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .filter(|entry| worth_a_look(&entry.file_name().to_string_lossy()))
                .map(|entry| entry.path())
                .collect();
            children.sort();
            for child in children {
                match repository(&child) {
                    Some(found) => self.found(found),
                    None if depth + 1 < MAX_DEPTH => queue.push_back((child, depth + 1)),
                    None => {}
                }
            }
        }
        self.walks.lock().unwrap().looked_at = looked_at.min(MAX_FOLDERS);
    }

    fn found(&self, found: Found) {
        self.walks.lock().unwrap().finding.push(found);
    }
}

fn worth_a_look(name: &str) -> bool {
    !name.starts_with('.')
        && !SKIP.contains(&name)
        && !SKIP_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The repository at `dir`, if `dir` is the top of one. A `.git` file counts
/// too: that is how worktrees are kept.
pub fn repository(dir: &Path) -> Option<Found> {
    std::fs::symlink_metadata(dir.join(".git")).ok()?;
    Some(Found {
        path: dir.to_owned(),
        projj: dir.join(TRACK_DIR).is_dir(),
    })
}

/// `path` with the home folder written as `~`.
pub fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// One answer to a search.
#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub path: String,
    /// The path as people write it, with `~` for the home folder.
    pub label: String,
    /// `projj`, `git`, or `folder` (no repository yet).
    pub kind: &'static str,
    /// When the root was last opened, if it is a recent one.
    pub opened: Option<i64>,
}

fn kind(found: &Found) -> &'static str {
    if found.projj { "projj" } else { "git" }
}

/// The repositories matching `query`, best first. Every word of the query
/// must be somewhere in the path; roots opened lately come first, then
/// projj roots, then matches in a folder's own name.
pub fn search(query: &str, found: &[Found], recent: &[(String, i64)], limit: usize) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut seen = BTreeSet::new();
    let mut scored: Vec<(i64, Hit)> = Vec::new();
    let recent = recent.iter().filter_map(|(path, opened)| {
        let found = repository(Path::new(path))?;
        Some((found, Some(*opened)))
    });
    let found = found.iter().cloned().map(|found| (found, None));
    for (rank, (found, opened)) in recent.chain(found).enumerate() {
        if !seen.insert(found.path.clone()) {
            continue;
        }
        let label = tilde(&found.path);
        let lower = label.to_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower).to_owned();
        if !words.iter().all(|word| lower.contains(word.as_str())) {
            continue;
        }
        let mut score = 0;
        if opened.is_some() {
            score += 10_000 - rank as i64;
        }
        if found.projj {
            score += 100;
        }
        for word in &words {
            if name.starts_with(word.as_str()) {
                score += 40;
            } else if name.contains(word.as_str()) {
                score += 20;
            }
        }
        score -= lower.matches('/').count() as i64;
        scored.push((
            score,
            Hit {
                path: found.path.display().to_string(),
                label,
                kind: kind(&found),
                opened,
            },
        ));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
    scored.into_iter().take(limit).map(|(_, hit)| hit).collect()
}

/// Folders that complete a typed path: those in the folder it names so far
/// whose names start with what follows the last `/`.
pub fn complete(typed: &str, limit: usize) -> Vec<Hit> {
    let (dir, prefix) = match typed.rfind('/') {
        Some(at) => (&typed[..=at], &typed[at + 1..]),
        None if typed == "~" => ("~/", ""),
        None => return Vec::new(),
    };
    let dir = crate::root::expand_home(Path::new(dir));
    if !dir.is_absolute() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.to_lowercase().starts_with(&prefix))
        // Hidden folders only when asked for.
        .filter(|name| !name.starts_with('.') || prefix.starts_with('.'))
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
        .into_iter()
        .take(limit)
        .map(|name| {
            let path = dir.join(&name);
            let kind = repository(&path).map_or("folder", |found| kind(&found));
            Hit {
                label: tilde(&path),
                path: path.display().to_string(),
                kind,
                opened: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(base: &Path, dirs: &[&str]) {
        for dir in dirs {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
    }

    fn walk(base: &Path) -> Vec<Found> {
        let finder = Finder::new(vec![base.to_owned()]);
        finder.look(Duration::ZERO);
        while finder.progress().scanning {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut found = finder.progress().found;
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found
    }

    #[test]
    fn finds_repositories_but_not_inside_them() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        layout(
            &base,
            &[
                "code/app/.git",
                "code/app/vendor/lib/.git",
                "code/root/.git",
                "code/root/track/x.git/.repo/.git",
                "code/node_modules/dep/.git",
                ".hidden/repo/.git",
                "Library/thing/.git",
                "a/b/c/d/e/f/g/h/deep/.git",
                "loose/notes",
            ],
        );
        std::fs::write(base.join("code/worktree.git"), "").unwrap();
        layout(&base, &["code/worktree"]);
        std::fs::write(base.join("code/worktree/.git"), "gitdir: elsewhere\n").unwrap();
        let found = walk(&base);
        let projj = |path: &str, projj| Found {
            path: base.join(path),
            projj,
        };
        assert_eq!(
            found,
            [
                projj("code/app", false),
                projj("code/root", true),
                projj("code/worktree", false),
            ]
        );
    }

    #[test]
    fn searches_recent_first_then_projj_roots() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        layout(
            &base,
            &[
                "one/widgets/.git",
                "two/widgets-api/.git",
                "two/widgets-api/track",
                "gone",
            ],
        );
        let found = vec![
            repository(&base.join("one/widgets")).unwrap(),
            repository(&base.join("two/widgets-api")).unwrap(),
        ];
        let paths = |hits: Vec<Hit>| -> Vec<String> {
            hits.into_iter()
                .map(|hit| {
                    hit.path
                        .strip_prefix(&base.display().to_string())
                        .unwrap()
                        .to_owned()
                })
                .collect()
        };
        assert_eq!(
            paths(search("widg", &found, &[], 10)),
            ["/two/widgets-api", "/one/widgets"]
        );
        let recent = vec![
            (base.join("one/widgets").display().to_string(), 5),
            (base.join("gone").display().to_string(), 9),
        ];
        assert_eq!(
            paths(search("", &found, &recent, 10)),
            ["/one/widgets", "/two/widgets-api"]
        );
        assert_eq!(
            paths(search("one widg", &found, &recent, 10)),
            ["/one/widgets"]
        );
        assert_eq!(
            paths(search("nothing", &found, &recent, 10)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn completes_typed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        layout(&base, &["Alpha/.git", "alpine", "beta", ".secret"]);
        std::fs::write(base.join("alfalfa.txt"), "").unwrap();
        let hits = complete(&format!("{}/al", base.display()), 10);
        let summary: Vec<(String, &str)> = hits
            .iter()
            .map(|hit| (hit.path.rsplit('/').next().unwrap().to_owned(), hit.kind))
            .collect();
        assert_eq!(
            summary,
            [("Alpha".into(), "git"), ("alpine".into(), "folder")]
        );
        assert_eq!(complete(&format!("{}/", base.display()), 10).len(), 3);
        assert_eq!(complete(&format!("{}/.", base.display()), 10).len(), 1);
        assert!(complete("relative/pa", 10).is_empty());
    }
}
