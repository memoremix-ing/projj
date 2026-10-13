//! Runs each scenario three times, from scratch: once with real git, once
//! with gitoxide, once with grit-lib. Then asks real git what each resulting
//! repository looks like, and reports where the libraries differ from git.
//!
//!     cargo run --release            # all scenarios
//!     cargo run --release -- sparse  # those whose name contains "sparse"

mod engines;
mod index_file;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use anyhow::bail;
use engines::Engine;
use engines::git;

const BASE: &str = r#"
git init -q -b main
printf '%s\n' one two three four five six seven eight nine ten > a.txt
mkdir dir1 dir2
printf 'b1\nb2\nb3\n' > dir1/b.txt
printf 'c1\nc2\n' > dir2/c.txt
printf '#!/bin/sh\necho hi\n' > exec.sh && chmod +x exec.sh
ln -s a.txt link
git add -A && git commit -qm base
"#;

const SHA256_BASE: &str = r#"
git init -q -b main --object-format=sha256
printf 'one\n' > a.txt
git add -A && git commit -qm base
"#;

/// Line 3 changed in the worktree; the index is to take only that change.
const A_LINE_3: &str = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
const MODIFY_A: &str = "printf '%s\\n' one two THREE four five six SEVEN eight nine ten > a.txt";

enum Op {
    Set(&'static str, Option<&'static str>, u32),
    Add(&'static [&'static str]),
    Reset(&'static [&'static str]),
}

struct Scenario {
    name: &'static str,
    /// Replaces `BASE` when set.
    base: Option<&'static str>,
    /// Shell run after `BASE`.
    setup: &'static str,
    op: Op,
}

fn scenarios() -> Vec<Scenario> {
    use Op::*;
    vec![
        Scenario {
            base: None,
            name: "set: some of a file's changes",
            setup: MODIFY_A,
            op: Set("a.txt", Some(A_LINE_3), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: part of a new file",
            setup: "printf 'x\\ny\\n' > new.txt",
            op: Set("new.txt", Some("x\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: a new executable",
            setup: "printf 'echo\\n' > tool.sh && chmod +x tool.sh",
            op: Set("tool.sh", Some("echo\n"), 0o100755),
        },
        Scenario {
            base: None,
            name: "set: a deletion",
            setup: "rm dir1/b.txt",
            op: Set("dir1/b.txt", None, 0),
        },
        Scenario {
            base: None,
            name: "set: after intent-to-add",
            setup: "printf 'x\\ny\\n' > new.txt && git add -N new.txt",
            op: Set("new.txt", Some("x\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: keeps other entries' flags",
            setup: "git update-index --skip-worktree dir2/c.txt && git update-index --assume-unchanged dir1/b.txt && printf '%s\\n' one two THREE four five six SEVEN eight nine ten > a.txt",
            op: Set("a.txt", Some(A_LINE_3), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: index version 4",
            setup: "git update-index --index-version 4 && printf '%s\\n' one two THREE four five six SEVEN eight nine ten > a.txt",
            op: Set("a.txt", Some(A_LINE_3), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: split index",
            setup: "git config core.splitIndex true && git update-index --split-index && printf '%s\\n' one two THREE four five six SEVEN eight nine ten > a.txt",
            op: Set("a.txt", Some(A_LINE_3), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: untracked cache, staging a new file",
            setup: "git config core.untrackedCache true && git update-index --untracked-cache && printf 'x\\ny\\n' > new.txt && git status --porcelain >/dev/null",
            op: Set("new.txt", Some("x\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: fsmonitor hook",
            setup: "printf '#!/bin/sh\\nprintf \"t\\\\0/\\\\0\"\\n' > .git/fsmon.sh && chmod +x .git/fsmon.sh && git config core.fsmonitor \"$PWD/.git/fsmon.sh\" && git update-index --fsmonitor && git status --porcelain >/dev/null && printf '%s\\n' one two THREE four five six SEVEN eight nine ten > a.txt",
            op: Set("a.txt", Some(A_LINE_3), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: sparse index",
            setup: "git sparse-checkout set --cone --sparse-index dir1 && printf 'b1\\nB2\\nB3\\n' > dir1/b.txt",
            op: Set("dir1/b.txt", Some("b1\nB2\nb3\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "add: modified, new, mode change",
            setup: "printf 'x\\n' > new.txt && printf 'changed\\n' >> a.txt && chmod -x exec.sh",
            op: Add(&["new.txt", "a.txt", "exec.sh"]),
        },
        Scenario {
            base: None,
            name: "add: a deletion",
            setup: "rm dir2/c.txt",
            op: Add(&["dir2/c.txt"]),
        },
        Scenario {
            base: None,
            name: "add: a symlink retargeted",
            setup: "rm link && ln -s dir1/b.txt link",
            op: Add(&["link"]),
        },
        Scenario {
            base: None,
            name: "add: core.autocrlf",
            setup: "git config core.autocrlf true && printf 'one\\r\\ntwo\\r\\n' > a.txt",
            op: Add(&["a.txt"]),
        },
        Scenario {
            base: None,
            name: "add: eol=crlf attribute",
            setup: "printf '*.txt text eol=crlf\\n' > .gitattributes && git add .gitattributes && git commit -qm attrs && printf 'one\\r\\ntwo\\r\\n' > a.txt",
            op: Add(&["a.txt"]),
        },
        Scenario {
            base: None,
            name: "add: clean filter",
            setup: "git config filter.upper.clean 'tr a-z A-Z' && git config filter.upper.smudge cat && printf '*.dat filter=upper\\n' > .gitattributes && git add .gitattributes && git commit -qm attrs && printf 'hello\\n' > data.dat",
            op: Add(&["data.dat"]),
        },
        Scenario {
            base: None,
            name: "add: resolves a conflict",
            setup: "git checkout -qb other && printf 'theirs\\n' > a.txt && git commit -qam other && git checkout -q main && printf 'ours\\n' > a.txt && git commit -qam main && (git merge -q other >/dev/null 2>&1 || true) && printf 'both\\n' > a.txt",
            op: Add(&["a.txt"]),
        },
        Scenario {
            base: None,
            name: "reset: a staged change",
            setup: "printf 'changed\\n' >> a.txt && git add a.txt",
            op: Reset(&["a.txt"]),
        },
        Scenario {
            base: None,
            name: "reset: a staged new file",
            setup: "printf 'x\\n' > new.txt && git add new.txt",
            op: Reset(&["new.txt"]),
        },
        Scenario {
            base: None,
            name: "set: someone else holds index.lock",
            setup: "touch .git/index.lock && printf 'x\\n' > new.txt",
            op: Set("new.txt", Some("x\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "set: keeps a submodule entry",
            setup: "git update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,sub && git commit -qm sub && printf 'changed\\n' >> a.txt",
            op: Set(
                "a.txt",
                Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\nchanged\n"),
                0o100644,
            ),
        },
        Scenario {
            base: None,
            name: "set: an awkward path",
            setup: "mkdir -p 'dir1/sp ace' && printf 'x\\n' > 'dir1/sp ace/naïve \"q\".txt'",
            op: Set("dir1/sp ace/naïve \"q\".txt", Some("x\n"), 0o100644),
        },
        Scenario {
            base: None,
            name: "add: an awkward path",
            setup: "printf 'x\\n' > 'dir2/tab\there.txt'",
            op: Add(&["dir2/tab\there.txt"]),
        },
        Scenario {
            base: None,
            name: "set: index.skipHash",
            setup: "git config index.skipHash true && git update-index --really-refresh >/dev/null; printf 'changed\\n' >> a.txt",
            op: Set("a.txt", Some("changed\n"), 0o100644),
        },
        Scenario {
            base: Some(SHA256_BASE),
            name: "set: SHA-256 repository",
            setup: "printf 'changed\\n' >> a.txt",
            op: Set("a.txt", Some("one\nchanged\n"), 0o100644),
        },
    ]
}

/// What real git says about a repository, plus the index's shape.
#[derive(PartialEq)]
struct Seen {
    stages: String,
    flags: String,
    resolve_undo: String,
    tree: String,
    status: String,
    version: u32,
    extensions: Vec<String>,
}

fn look(repo: &Path) -> Result<Seen> {
    let text = |args: &[&str]| -> Result<String> {
        Ok(String::from_utf8_lossy(&git(repo, args, None)?).into_owned())
    };
    let hash_len = if text(&["rev-parse", "--show-object-format"])?.trim() == "sha256" {
        32
    } else {
        20
    };
    let shape = index_file::shape(&std::fs::read(repo.join(".git/index"))?, hash_len)?;
    Ok(Seen {
        version: shape.version,
        extensions: shape.extensions,
        stages: text(&["ls-files", "--stage"])?,
        flags: text(&["ls-files", "-v"])?,
        resolve_undo: text(&["ls-files", "--resolve-undo"])?,
        tree: text(&["write-tree"]).unwrap_or_else(|err| format!("write-tree failed: {err}")),
        // Last: status may rewrite the index.
        status: text(&["status", "--porcelain=v2", "--untracked-files=all"])?,
    })
}

fn build(dir: &Path, scenario: &Scenario) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let script = format!("{}\n{}", scenario.base.unwrap_or(BASE), scenario.setup);
    let output = Command::new("sh")
        .arg("-euc")
        .arg(script)
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        bail!("setup failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// The engine's own error is `Ok(Err(..))`; anything else going wrong is `Err`.
fn run(engine: &dyn Engine, dir: &Path, scenario: &Scenario) -> Result<Result<Seen, String>> {
    build(dir, scenario)?;
    let done = match &scenario.op {
        Op::Set(path, content, mode) => engine.set(dir, path, content.map(str::as_bytes), *mode),
        Op::Add(paths) => engine.add(dir, paths),
        Op::Reset(paths) => engine.reset(dir, paths),
    };
    if let Err(err) = done {
        return Ok(Err(format!("{err:#}")));
    }
    Ok(Ok(look(dir)?))
}

/// Where `got` parts from what git did.
fn differences(want: &Seen, got: &Seen) -> (Vec<String>, Vec<String>) {
    let mut wrong = Vec::new();
    let mut fields = |name: &str, a: &str, b: &str| {
        if a != b {
            wrong.push(format!(
                "{name}:\n        git:  {}\n        this: {}",
                a.trim().replace('\n', "\n              "),
                b.trim().replace('\n', "\n              ")
            ));
        }
    };
    fields("staged entries", &want.stages, &got.stages);
    fields("entry flags", &want.flags, &got.flags);
    fields("resolve-undo", &want.resolve_undo, &got.resolve_undo);
    fields("tree to commit", &want.tree, &got.tree);
    fields("status", &want.status, &got.status);
    let mut notes = Vec::new();
    if want.version != got.version {
        notes.push(format!(
            "index version {} (git kept {})",
            got.version, want.version
        ));
    }
    if want.extensions != got.extensions {
        notes.push(format!(
            "extensions {:?} (git: {:?})",
            got.extensions, want.extensions
        ));
    }
    (wrong, notes)
}

fn main() -> Result<()> {
    let only = std::env::args().nth(1);
    let work = tempfile::tempdir()?;
    // The same commits every time, and none of this machine's git config.
    // SAFETY: nothing else is running yet.
    unsafe {
        for (key, value) in [
            ("HOME", work.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", work.path().to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Spike"),
            ("GIT_AUTHOR_EMAIL", "spike@example.com"),
            ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
            ("GIT_COMMITTER_NAME", "Spike"),
            ("GIT_COMMITTER_EMAIL", "spike@example.com"),
            ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
        ] {
            std::env::set_var(key, value);
        }
    }
    let engines: [&dyn Engine; 3] = [
        &engines::Gix { keep_tree: false },
        &engines::Grit,
        &engines::Gix { keep_tree: true },
    ];
    let mut tally = [(0, 0, 0); 3];
    for (n, scenario) in scenarios().iter().enumerate() {
        if only
            .as_deref()
            .is_some_and(|only| !scenario.name.contains(only))
        {
            continue;
        }
        println!("{}", scenario.name);
        let want = run(
            &engines::Git,
            &work.path().join(format!("{n}-git")),
            scenario,
        )?;
        if let Err(refusal) = &want {
            println!("  git       refused: {refusal}");
        }
        for (i, engine) in engines.iter().enumerate() {
            let dir = work.path().join(format!("{n}-{}", engine.name()));
            match (&want, run(*engine, &dir, scenario)?) {
                (Err(_), Err(err)) => {
                    tally[i].0 += 1;
                    println!("  {:<9} refused, as git did: {err}", engine.name());
                }
                (Ok(_), Err(err)) => {
                    tally[i].2 += 1;
                    println!("  {:<9} FAILED: {err}", engine.name());
                }
                (Err(refusal), Ok(_)) => {
                    tally[i].1 += 1;
                    println!(
                        "  {:<9} DIFFERENT: went ahead where git refused ({refusal})",
                        engine.name()
                    );
                }
                (Ok(want), Ok(got)) => {
                    let (wrong, notes) = differences(want, &got);
                    let notes = if notes.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", notes.join("; "))
                    };
                    if wrong.is_empty() {
                        tally[i].0 += 1;
                        println!("  {:<9} same as git{notes}", engine.name());
                    } else {
                        tally[i].1 += 1;
                        println!("  {:<9} DIFFERENT{notes}", engine.name());
                        for wrong in wrong {
                            println!("      {wrong}");
                        }
                    }
                }
            }
        }
    }
    println!();
    for (engine, (same, different, failed)) in engines.iter().zip(tally) {
        println!(
            "{:<9} {same} same as git, {different} different, {failed} failed",
            engine.name()
        );
    }
    Ok(())
}
