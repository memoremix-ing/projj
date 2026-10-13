//! The built-in git, checked against real git: in each scenario, every
//! answer projj gets from one must be the one it gets from the other, and
//! every change it makes must leave the index as git would have.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::git::Engine;
use crate::git::Git;
use crate::git::Side;

/// Made the same way for every scenario, before the scenario's own steps.
const BASE: &str = r#"
git init -q -b main
printf '%s\n' '#include <stdio.h>' '' 'int first(void)' '{' '    return 1;' '}' '' 'int second(void)' '{' '    int a = 2;' '    int b = 3;' '    return a + b;' '}' '' 'int third(void)' '{' '    return 3;' '}' > code.c
printf '%s\n' one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen > list.txt
printf '#!/bin/sh\necho hi\n' > run.sh && chmod +x run.sh
mkdir -p docs/deep && printf 'deep\n' > docs/deep/note.md
printf 'no newline' > tail.txt
ln -s list.txt link
printf 'bin\000ary\001\n' > data.bin
git add -A && git commit -qm base
"#;

/// Each scenario's steps, run after `BASE` (or instead of it, for those
/// starting with `!`).
const SCENARIOS: &[(&str, &str)] = &[
    (
        "edits in several places, with function headings",
        "sed -e 's/return 1;/return 10;/' -e 's/int b = 3;/int b = 30;/' -e 's/return 3;/return 33;/' code.c > x && mv x code.c && printf 'sixteen\\n' >> list.txt && sed -i.bak 's/^one$/ONE/' list.txt && rm list.txt.bak",
    ),
    (
        "new files with awkward names",
        "printf 'a\\nb\\n' > 'sp ace.txt' && printf 'x' > 'no-newline.txt' && : > empty.txt && printf 'q\\n' > 'quo\"te.txt' && printf 't\\n' > \"$(printf 'ta\\tb.txt')\" && printf 'u\\n' > 'naïve.txt' && mkdir -p new/dir && printf 'n\\n' > new/dir/file.txt",
    ),
    ("a deletion", "rm list.txt"),
    (
        "a mode change, alone and with an edit",
        "chmod -x run.sh && chmod +x list.txt && printf 'more\\n' >> list.txt",
    ),
    (
        "symlinks retargeted, added, and a file turned into one",
        "rm link && ln -s code.c link && ln -s docs newlink && rm tail.txt && ln -s list.txt tail.txt",
    ),
    (
        "binary files",
        "printf 'bin\\000ary\\002\\n' > data.bin && printf '\\000\\001' > new.bin",
    ),
    (
        "no newline at the end, gained and lost",
        "printf 'no newline\\nnow more\\n' > tail.txt && printf '%s\\n' one two three | tr -d '\\n' > list.txt",
    ),
    (
        "core.autocrlf",
        "git config core.autocrlf true && printf 'one\\r\\ntwo\\r\\nthree\\r\\n' > list.txt && printf 'new\\r\\n' > crlf.txt",
    ),
    (
        "eol=crlf attribute",
        "printf '*.txt text eol=crlf\\n' > .gitattributes && git add .gitattributes && git commit -qm attrs && printf 'one\\r\\nTWO\\r\\n' > list.txt",
    ),
    (
        "carriage returns kept as content",
        "printf 'one\\r\\ntwo\\n' > list.txt",
    ),
    (
        "a diff driver's headings",
        "printf '*.c diff=cpp\\n' > .gitattributes && sed 's/int a = 2;/int a = 20;/' code.c > x && mv x code.c",
    ),
    (
        "diff.algorithm=histogram",
        "git config diff.algorithm histogram && sed -e 's/^}$/} \\/\\/ end/' code.c > x && mv x code.c",
    ),
    (
        "staged and unstaged changes to one file",
        "sed -i.bak 's/^two$/TWO/' list.txt && rm list.txt.bak && git add list.txt && sed -i.bak 's/^nine$/NINE/' list.txt && rm list.txt.bak && printf 'x\\n' > staged-new.txt && git add staged-new.txt && rm docs/deep/note.md && git add docs/deep/note.md",
    ),
    (
        "intent to add",
        "printf 'later\\n' > later.txt && git add -N later.txt",
    ),
    (
        "a conflict",
        "git checkout -qb other && printf 'theirs\\n' > tail.txt && git commit -qam other && git checkout -q main && printf 'ours\\n' > tail.txt && git commit -qam main && (git merge -q other >/dev/null 2>&1 || true) && printf 'x\\n' >> list.txt",
    ),
    (
        "ignored files, an ignored folder, a nested repository",
        "printf 'build/\\n*.log\\n!keep.log\\n' > .gitignore && git add .gitignore && git commit -qm ignore && mkdir -p build/out docs/build && printf 'o\\n' > build/out/a.o && printf 'l\\n' > a.log && printf 'k\\n' > keep.log && printf 'd\\n' > docs/build/x && mkdir nested && git -C nested init -q && printf 'n\\n' > nested/inside.txt && mkdir -p empty/dir && printf 'u\\n' > docs/deep/untracked.md",
    ),
    (
        "skip-worktree",
        "git update-index --skip-worktree run.sh && printf 'changed\\n' >> run.sh && printf 'x\\n' >> list.txt",
    ),
    (
        "bytes that are not UTF-8",
        "printf 'caf\\351\\n' > latin1.txt && git add latin1.txt && git commit -qm latin1 && printf 'caf\\351 cr\\350me\\n' > latin1.txt",
    ),
    ("a large change", "seq 1 400 > list.txt"),
    (
        "before the first commit",
        "!git init -q -b main && printf 'a\\n' > a.txt && printf 'b\\n' > b.txt && git add a.txt",
    ),
];

fn build(dir: &Path, script: &str) {
    let script = match script.strip_prefix('!') {
        Some(script) => script.to_owned(),
        None => format!("{BASE}\n{script}"),
    };
    let output = Command::new("sh")
        .arg("-euc")
        .arg(script)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// A scenario built twice: for real git and for the built-in one.
struct Twins {
    _dirs: [tempfile::TempDir; 2],
    real: Git,
    ours: Git,
}

fn twins(script: &str) -> Twins {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    for dir in &dirs {
        build(dir.path(), script);
    }
    Twins {
        real: Git::with_engine(dirs[0].path(), Engine::Installed),
        ours: Git::with_engine(dirs[1].path(), Engine::BuiltIn),
        _dirs: dirs,
    }
}

impl Twins {
    fn dirs(&self) -> [&Path; 2] {
        [self._dirs[0].path(), self._dirs[1].path()]
    }

    /// Everything projj reads, from both, which must agree. Returns where
    /// they do not.
    async fn reads(&self, name: &str) -> Vec<String> {
        let mut wrong = Vec::new();
        let mut check = |same: bool, what: String, ours: String, real: String| {
            if !same {
                wrong.push(format!("{what}\n  built-in: {ours}\n  git:      {real}"));
            }
        };
        let status = |git: &Git| {
            let git = git.clone();
            async move {
                let mut status = serde_json::to_value(git.status().await.unwrap()).unwrap();
                for entry in status["entries"].as_array_mut().unwrap() {
                    entry["modified"] = Value::Null;
                }
                status
            }
        };
        let (listed, ours) = (status(&self.real).await, status(&self.ours).await);
        check(
            ours == listed,
            format!("{name}: status"),
            ours.to_string(),
            listed.to_string(),
        );

        let (ours, real) = (
            self.ours.files().await.unwrap(),
            self.real.files().await.unwrap(),
        );
        check(
            ours == real,
            format!("{name}: files"),
            format!("{ours:?}"),
            format!("{real:?}"),
        );
        let others = |ignored: bool| -> [Vec<String>; 2] {
            let mut args = vec!["ls-files", "-z", "--others", "--exclude-standard"];
            if ignored {
                args.extend(["--ignored", "--directory"]);
            }
            let real = git(self.dirs()[0], &args)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            [super::others(self.dirs()[1], ignored).unwrap(), real]
        };
        let [ours, real] = others(false);
        check(
            ours == real,
            format!("{name}: untracked"),
            format!("{ours:?}"),
            format!("{real:?}"),
        );
        let [ours, real] = others(true);
        check(
            ours == real,
            format!("{name}: ignored"),
            format!("{ours:?}"),
            format!("{real:?}"),
        );

        for entry in listed["entries"].as_array().unwrap() {
            if entry["directory"] == true {
                continue;
            }
            let path = entry["path"].as_str().unwrap();
            if entry["conflicted"] == true {
                // projj shows a conflict as one, without reading the
                // combined diff git prints for it.
                for side in [Side::Unstaged, Side::Staged] {
                    let shown = |git: &Git| {
                        let git = git.clone();
                        let path = path.to_owned();
                        async move {
                            let file = git.file_diff(&path, side, 3).await.unwrap();
                            file.map(|file| (file.note, file.hunks.len()))
                        }
                    };
                    let (ours, real) = (shown(&self.ours).await, shown(&self.real).await);
                    check(
                        ours == real,
                        format!("{name}: conflict {path:?} {side:?}"),
                        format!("{ours:?}"),
                        format!("{real:?}"),
                    );
                }
                continue;
            }
            for side in [Side::Unstaged, Side::Staged] {
                for context in [0, 3, 10] {
                    let real = self.real.diff_bytes(path, side, context).await.unwrap();
                    let ours = self.ours.diff_bytes(path, side, context).await.unwrap();
                    check(
                        ours == real,
                        format!("{name}: diff of {path:?}, {side:?}, -U{context}"),
                        format!("{:?}", String::from_utf8_lossy(&ours)),
                        format!("{:?}", String::from_utf8_lossy(&real)),
                    );
                }
            }
            for revision in [format!(":{path}"), format!("HEAD:{path}")] {
                let (ours, real) = (
                    self.ours.blob(&revision).await,
                    self.real.blob(&revision).await,
                );
                check(
                    ours == real,
                    format!("{name}: {revision}"),
                    format!("{ours:?}"),
                    format!("{real:?}"),
                );
            }
        }
        wrong
    }

    /// How git sees each index, which must agree.
    fn index(&self, name: &str, step: &str, wrong: &mut Vec<String>) {
        for args in [
            &["ls-files", "--stage"][..],
            &["ls-files", "-v"],
            &["ls-files", "--resolve-undo"],
            &["write-tree"],
            &["status", "--porcelain=v2", "--untracked-files=all"],
        ] {
            let [real, ours] = self.dirs().map(|dir| git(dir, args));
            if ours != real {
                wrong.push(format!(
                    "{name}: after {step}: git {}\n  built-in: {ours:?}\n  git:      {real:?}",
                    args.join(" ")
                ));
            }
        }
    }

    /// projj's writes, made in both, one at a time, comparing as it goes.
    async fn writes(&self, name: &str) -> Vec<String> {
        let mut wrong = Vec::new();
        let paths = |git: &Git| {
            let git = git.clone();
            async move {
                let status = git.status().await.unwrap();
                status
                    .entries
                    .iter()
                    .filter(|entry| !entry.directory && !entry.conflicted)
                    .map(|entry| entry.path.clone())
                    .collect::<Vec<_>>()
            }
        };
        let changed = paths(&self.real).await;
        for path in &changed {
            for side in [Side::Unstaged, Side::Staged] {
                // One changed line of the first hunk, then the rest of it.
                for whole in [false, true] {
                    let step = format!(
                        "{side:?} {path:?}, {}",
                        if whole { "whole hunk" } else { "one line" }
                    );
                    let mut results = Vec::new();
                    for git in [&self.real, &self.ours] {
                        let Some(file) = git.file_diff(path, side, 3).await.unwrap() else {
                            results.push(None);
                            continue;
                        };
                        let Some(hunk) = file.hunks.first() else {
                            results.push(None);
                            continue;
                        };
                        let first = hunk
                            .lines
                            .iter()
                            .position(|line| line.kind != crate::diff::LineKind::Context);
                        let lines = (!whole).then(|| first.into_iter().collect::<Vec<_>>());
                        let result = git
                            .apply_hunk(path, side, 3, &hunk.id, lines.as_deref())
                            .await;
                        results.push(Some(result.map_err(|err| err.to_string())));
                    }
                    match (&results[0], &results[1]) {
                        (Some(Ok(())), Some(Ok(()))) | (None, None) => {}
                        (Some(Err(_)), Some(Err(_))) => {}
                        (real, ours) => {
                            wrong.push(format!("{name}: {step}: git {real:?}, built-in {ours:?}"))
                        }
                    }
                    self.index(name, &step, &mut wrong);
                }
            }
        }
        let paths = paths(&self.real).await;
        for (step, stage) in [
            ("staging every file", true),
            ("unstaging every file", false),
        ] {
            let [real, ours] = [&self.real, &self.ours].map(|git| {
                let (git, paths) = (git.clone(), paths.clone());
                async move {
                    if stage {
                        git.stage_paths(&paths).await
                    } else {
                        git.unstage_paths(&paths).await
                    }
                    .map_err(|err| err.to_string())
                }
            });
            let (real, ours) = (real.await, ours.await);
            if ours.is_ok() != real.is_ok() {
                wrong.push(format!("{name}: {step}: git {real:?}, built-in {ours:?}"));
            }
            self.index(name, step, &mut wrong);
        }
        wrong
    }
}

fn report(wrong: Vec<String>) {
    assert!(
        wrong.is_empty(),
        "{} differences from git:\n\n{}",
        wrong.len(),
        wrong.join("\n\n")
    );
}

#[tokio::test]
async fn reads_as_git_does() {
    let mut wrong = Vec::new();
    for (name, script) in SCENARIOS {
        wrong.extend(twins(script).reads(name).await);
    }
    report(wrong);
}

#[tokio::test]
async fn writes_the_index_as_git_does() {
    let mut wrong = Vec::new();
    for (name, script) in SCENARIOS {
        let twins = twins(script);
        wrong.extend(twins.writes(name).await);
        // And reads still agree afterwards.
        wrong.extend(twins.reads(&format!("{name}, afterwards")).await);
    }
    report(wrong);
}

#[test]
fn finds_the_same_repository() {
    let dir = tempfile::tempdir().unwrap();
    build(
        dir.path(),
        "mkdir -p a/b && mkdir inner && git -C inner init -q",
    );
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(super::toplevel(&root.join("a/b")), Some(root.clone()));
    assert_eq!(
        super::toplevel(&root.join("inner")),
        Some(root.join("inner"))
    );
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(super::toplevel(outside.path()), None);
}
