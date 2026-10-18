//! What Codex is told, and the tools it is given. In code mode it calls them
//! from JavaScript, as `tools.projj__changes({})` and so on.
//!
//! The tools that read are answered here, from the same git and jj that the
//! page reads, so that the hunks Codex talks about are the ones on screen.
//! Those that act on the page are only described here: the conversation
//! hands them to the page (see `Conversation::ask_page`).

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;

use crate::api::App;
use crate::api::Root;
use crate::codex::Process;
use crate::diff::Hunk;
use crate::diff::LineKind;
use crate::git::Side;

/// Tools the page answers.
pub const PAGE_TOOLS: &[&str] = &["view", "show", "highlight", "clear_highlights"];

/// The most text one answer carries; Codex is told how to ask for less.
const MOST_TEXT: usize = 60_000;
const MOST_LINES: usize = 600;
const MOST_FILES: usize = 2000;

/// Codex's own features and the user's extras that the guide does without:
/// it reads and points, and nothing it reads should be able to reach
/// further than that.
const WITHOUT: &[&str] = &[
    "apps",
    "browser_use",
    "computer_use",
    "goals",
    "image_generation",
    "memories",
    "multi_agent",
    "plugins",
];

/// Settings for the guide's threads, over the user's own: code mode on,
/// and everything not needed for reading off, MCP servers included.
pub async fn config(process: &Process, root: &Path) -> Value {
    let mut config = json!({
        "features.code_mode": true,
        "features.code_mode_only": true,
        "web_search": "disabled",
    });
    for feature in WITHOUT {
        config[format!("features.{feature}")] = false.into();
    }
    if let Ok(read) = process.request("config/read", json!({ "cwd": root })).await
        && let Some(servers) = read["config"]["mcp_servers"].as_object()
    {
        for name in servers.keys() {
            config[format!("mcp_servers.{name}.enabled")] = false.into();
        }
    }
    config
}

pub fn instructions(root: &Path, projj: bool) -> String {
    let kind = if projj {
        "a projj root: a git repository for human review, with jj repositories that agents work in under `track/` (each as `track/<host>/<owner>/<repo>.git/` holding the repository in `.repo/` and workspaces such as `sts-0/` or `lts-1/` beside it)"
    } else {
        "a git repository"
    };
    format!(
        "You are jjent, the guide inside projj, a local code review tool. The person is reviewing \
         `{root}`, {kind}, in a page beside this chat, and you help them understand it: walk \
         them through changes, answer questions about any file, changed or not.

How review works here: changes not yet staged in the root's git index are \"unreviewed\"; \
marking something reviewed stages it. Only the person reviews, stages, edits or commits.

You only read. Never try to change files, the git index or any repository. Do not run `jj` \
(every jj command records a snapshot, which writes) or any git command that writes. Prefer \
the `projj` tools to the shell: they show diffs exactly as the page does, with the same hunk \
ids. Reading and searching with the shell (`rg`, `cat`, `ls`) is fine.

Point at what you talk about, as you talk about it: `projj.show` scrolls the page to a file, \
hunk or line, and `projj.highlight` marks lines with an optional short note (in the diff, or \
in the editor for a file with no changes). A new highlight replaces the old ones unless you \
pass `add: true`. Line numbers are those of the file now (the new side), unless you pass \
`side: \"old\"` for removed lines. When you cite a place in a message, write it as a \
markdown link to `path:line` or `path:line-line`, like [the check](src/api.rs:120-128); the \
person can click it.

To walk through changes, read `projj.changes` and the diffs, then call `projj.tour` once \
with steps in the order that makes them easiest to understand: what the rest builds on \
first (data shapes, types, core logic), then what uses it, then tests and docs, grouping \
related hunks across files and leaving out nothing that matters. Then explain the first \
step and show it. Wait for the person before going on; they may step through the tour \
themselves, and each message says which step they are on.

Every message from the person comes with `projj-view`: what the page shows, which hunk is \
focused, what they selected and what is open in the editor. \"This\" and \"here\" mean that.

Be brief and plain. The person reads the code in the page, so quote only the few lines you \
need.",
        root = root.display()
    )
}

/// One location in the root, as the page tools and tour steps take it.
fn place_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "path": { "type": "string", "description": "Root-relative path, as the other tools give it." },
            "line": { "type": "integer", "description": "First line, as numbered in the file now; or in the old file with side \"old\"." },
            "end": { "type": "integer", "description": "Last line; the same as line if left out." },
            "side": { "type": "string", "enum": ["new", "old"], "description": "Which version line and end count in. \"old\" is for removed lines." },
            "hunk": { "type": "string", "description": "A hunk id from projj.diff, instead of or as well as lines." },
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

fn function(name: &str, description: &str, schema: Value) -> Value {
    json!({ "type": "function", "name": name, "description": description, "inputSchema": schema })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The tools, as one namespace.
pub fn specs() -> Value {
    let side = json!({
        "type": "string",
        "enum": ["unreviewed", "reviewed"],
        "description": "Unreviewed (the default) or reviewed changes.",
    });
    let mut highlight = place_schema("What to mark.");
    highlight["properties"]["note"] =
        json!({ "type": "string", "description": "A few words shown beside the marked lines." });
    highlight["properties"]["add"] =
        json!({ "type": "boolean", "description": "Keep the highlights already shown." });
    highlight["properties"]["scroll"] =
        json!({ "type": "boolean", "description": "Scroll to it (the default) or not." });
    let mut show = place_schema("Where to go.");
    show["properties"]["in"] = json!({
        "type": "string",
        "enum": ["diff", "editor"],
        "description": "Show the file in the editor even if it has changes. Files without changes always open in the editor.",
    });
    let step = object(
        json!({
            "title": { "type": "string", "description": "A few words." },
            "explain": { "type": "string", "description": "One to three sentences: what it is and why it comes here." },
            "places": { "type": "array", "items": place_schema("A place this step covers."), "description": "Highlighted together when the person goes to the step." },
        }),
        &["title", "places"],
    );
    json!([{
        "type": "namespace",
        "name": "projj",
        "description": "The root under review and the review page beside this chat. Every tool returns JSON or text.",
        "tools": [
            function(
                "changes",
                "The files with unreviewed (or reviewed) changes: path, the jj workspace it is in, status and lines added and removed, plus the root's git branch.",
                object(json!({ "side": side.clone() }), &[]),
            ),
            function(
                "diff",
                "One file's diff, hunk by hunk, as the page shows it. Each hunk has an id; each line starts with its number (the old number for removed lines) and +, - or a space.",
                object(json!({
                    "path": { "type": "string" },
                    "side": side,
                    "context": { "type": "integer", "description": "Lines of context around changes; the page's own setting by default." },
                }), &["path"]),
            ),
            function(
                "read",
                &format!("A file as it is on disk, with line numbers, at most {MOST_LINES} lines at a time."),
                object(json!({
                    "path": { "type": "string" },
                    "from": { "type": "integer", "description": "First line; 1 by default." },
                    "to": { "type": "integer" },
                }), &["path"]),
            ),
            function(
                "files",
                "Every file in the root that git does not ignore, optionally only those whose path contains `match`.",
                object(json!({ "match": { "type": "string" } }), &[]),
            ),
            function(
                "workspaces",
                "The jj repositories under the root and their workspaces, for jj_log and jj_diff.",
                object(json!({}), &[]),
            ),
            function(
                "jj_log",
                "jj history in a workspace: change id, commit id, bookmarks and description of each commit, newest first.",
                object(json!({
                    "workspace": { "type": "string", "description": "A workspace path from projj.workspaces." },
                    "revset": { "type": "string", "description": "A jj revset; the workspace's default log if left out." },
                    "limit": { "type": "integer" },
                }), &["workspace"]),
            ),
            function(
                "jj_diff",
                "What a jj commit changed (against its parents, or against `from`), file by file and hunk by hunk.",
                object(json!({
                    "workspace": { "type": "string" },
                    "to": { "type": "string", "description": "A revision: a change id, commit id or revset naming one commit." },
                    "from": { "type": "string" },
                    "path": { "type": "string", "description": "Only this file, relative to the workspace." },
                }), &["workspace", "to"]),
            ),
            function(
                "view",
                "What the page shows now: the view, the files and hunks on screen, the focused hunk, what the person has selected, the file open in the editor and the highlights.",
                object(json!({}), &[]),
            ),
            function(
                "show",
                "Scrolls the page to a file, hunk or line. A file without changes, or with in: \"editor\", opens in the editor below the diff.",
                show,
            ),
            function("highlight", "Marks lines on the page, with an optional note, and scrolls to them.", highlight),
            function(
                "clear_highlights",
                "Removes the highlights, all of them or those in one file.",
                object(json!({ "path": { "type": "string" } }), &[]),
            ),
            function(
                "tour",
                "Shows a walkthrough in the chat panel as numbered steps the person can step through; going to a step shows and highlights its places. Call it once per walkthrough.",
                object(json!({
                    "title": { "type": "string" },
                    "steps": { "type": "array", "items": step, "minItems": 1 },
                }), &["title", "steps"]),
            ),
        ],
    }])
}

fn string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn number(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    string(args, key).ok_or_else(|| format!("`{key}` is required"))
}

fn side(args: &Value) -> Result<Side, String> {
    match string(args, "side") {
        None | Some("unreviewed" | "unstaged") => Ok(Side::Unstaged),
        Some("reviewed" | "staged") => Ok(Side::Staged),
        Some(other) => Err(format!(
            "side is \"unreviewed\" or \"reviewed\", not {other:?}"
        )),
    }
}

/// Text cut to what one answer may carry, saying so if it was cut.
fn capped(mut text: String) -> String {
    if text.len() <= MOST_TEXT {
        return text;
    }
    let mut cut = MOST_TEXT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n… cut short here; ask for less at a time.");
    text
}

pub fn hunk_text(hunk: &Hunk, out: &mut String) {
    let _ = writeln!(
        out,
        "@@ -{},{} +{},{} @@ {}  [hunk {}]",
        hunk.old_start,
        hunk.old_count,
        hunk.new_start,
        hunk.new_count,
        hunk.section.trim(),
        hunk.id
    );
    for line in &hunk.lines {
        let (number, sign) = match line.kind {
            LineKind::Context => (line.new, ' '),
            LineKind::Added => (line.new, '+'),
            LineKind::Removed => (line.old, '-'),
        };
        let number = number.map(|n| n.to_string()).unwrap_or_default();
        let _ = writeln!(out, "{number:>5} {sign} {}", line.text);
    }
}

/// Answers a tool that reads the root.
pub async fn run(
    app: &Arc<App>,
    root: &Arc<Root>,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        "changes" => changes(root, args).await,
        "diff" => diff(app, root, args).await,
        "read" => read(root, args),
        "files" => files(root, args).await,
        "workspaces" => {
            let repos = root.repos().await.map_err(|err| format!("{err:#}"))?;
            serde_json::to_value(&*repos).map_err(|err| err.to_string())
        }
        "jj_log" => jj_log(app, root, args).await,
        "jj_diff" => jj_diff(app, root, args).await,
        _ => Err(format!("there is no tool called {name}")),
    }
}

fn status_word(letter: char) -> &'static str {
    match letter {
        'M' => "modified",
        'A' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "type changed",
        'U' => "conflicted",
        _ => "changed",
    }
}

async fn changes(root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let side = side(args)?;
    let status = root.git.status().await.map_err(|err| err.to_string())?;
    let repos = root.repos().await.map_err(|err| format!("{err:#}"))?;
    let files: Vec<Value> = status
        .entries
        .iter()
        .filter_map(|entry| {
            let stat = match side {
                Side::Unstaged => entry.unstaged.as_ref(),
                Side::Staged => entry.staged.as_ref(),
            };
            if stat.is_none() && !(side == Side::Unstaged && entry.directory) {
                return None;
            }
            let status = if entry.directory {
                "a nested repository, which git does not look inside"
            } else if entry.untracked {
                "new, untracked"
            } else if entry.conflicted {
                "conflicted"
            } else {
                status_word(match side {
                    Side::Unstaged => entry.worktree,
                    Side::Staged => entry.index,
                })
            };
            let other = match side {
                Side::Unstaged => entry.staged.is_some(),
                Side::Staged => entry.unstaged.is_some(),
            };
            Some(json!({
                "path": entry.path,
                "workspace": crate::root::workspace_for(&repos, &entry.path).map(|ws| ws.path.clone()),
                "status": status,
                "added": stat.and_then(|stat| stat.added),
                "removed": stat.and_then(|stat| stat.removed),
                "also_on_the_other_side": other,
            }))
        })
        .collect();
    Ok(json!({
        "side": if side == Side::Staged { "reviewed" } else { "unreviewed" },
        "branch": status.branch,
        "head": status.head,
        "files": files,
    }))
}

/// Lines of context the page shows diffs with, so that hunk ids match.
async fn page_context(app: &App) -> usize {
    app.store
        .settings()
        .await
        .ok()
        .and_then(|settings| settings.get("context").and_then(Value::as_u64))
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(3)
}

async fn diff(app: &App, root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let path = required(args, "path")?;
    let side = side(args)?;
    let context = match number(args, "context") {
        Some(context) => context.min(100_000),
        None => page_context(app).await,
    };
    let Some(file) = root
        .git
        .file_diff(path, side, context)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(Value::String(format!(
            "{path} has no {} changes.",
            if side == Side::Staged {
                "reviewed"
            } else {
                "unreviewed"
            }
        )));
    };
    let mut out = format!(
        "{path}: {:?}, +{} -{}\n",
        file.status, file.added, file.removed
    )
    .to_lowercase();
    if let Some(note) = &file.note {
        let _ = writeln!(out, "{note}");
    }
    if file.binary {
        out.push_str("A binary file.\n");
    }
    for hunk in &file.hunks {
        hunk_text(hunk, &mut out);
    }
    Ok(Value::String(capped(out)))
}

fn read(root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let path = required(args, "path")?;
    let file = crate::files::read(&root.path, path).map_err(|err| err.to_string())?;
    let lines: Vec<&str> = file.content.split('\n').collect();
    let total = lines.len();
    let from = number(args, "from").unwrap_or(1).clamp(1, total.max(1));
    let to = number(args, "to")
        .unwrap_or(from + MOST_LINES - 1)
        .clamp(from, (from + MOST_LINES - 1).min(total));
    let mut out = format!("{path}: lines {from}-{to} of {total}\n");
    for (index, line) in lines.iter().enumerate().take(to).skip(from - 1) {
        let _ = writeln!(out, "{:>5}  {line}", index + 1);
    }
    Ok(Value::String(capped(out)))
}

async fn files(root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let all = root.git.files().await.map_err(|err| err.to_string())?;
    let wanted = string(args, "match").map(str::to_lowercase);
    let matching: Vec<&String> = all
        .iter()
        .filter(|path| {
            wanted
                .as_ref()
                .is_none_or(|wanted| path.to_lowercase().contains(wanted))
        })
        .collect();
    Ok(json!({
        "count": matching.len(),
        "files": matching.iter().take(MOST_FILES).collect::<Vec<_>>(),
        "cut_short": matching.len() > MOST_FILES,
    }))
}

/// Runs a jj query in one of the root's workspaces.
async fn in_workspace<T, F>(
    app: &Arc<App>,
    root: &Arc<Root>,
    workspace: &str,
    query: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: AsyncFnOnce(&crate::jj::Session) -> anyhow::Result<T> + Send + 'static,
{
    let repos = root.repos().await.map_err(|err| format!("{err:#}"))?;
    let known = repos
        .iter()
        .flat_map(|repo| &repo.workspaces)
        .any(|ws| ws.path == workspace);
    if !known {
        return Err(format!(
            "no workspace {workspace:?}; projj.workspaces lists them"
        ));
    }
    let dir = root.path.join(workspace);
    let app = app.clone();
    tokio::task::spawn_blocking(move || {
        let session = app.jj.open(&dir)?;
        pollster::block_on(query(&session))
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(|err| format!("{err:#}"))
}

fn commit_line(commit: &crate::jj::CommitInfo, out: &mut String) {
    let id = |id: &str, length: usize| id.chars().take(length.max(8)).collect::<String>();
    let mut labels: Vec<String> = commit.bookmarks.clone();
    labels.extend(commit.working_copies.iter().map(|ws| format!("{ws}@")));
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", labels.join(", "))
    };
    let mut flags = String::new();
    if commit.empty {
        flags.push_str(" (empty)");
    }
    if commit.conflict {
        flags.push_str(" (conflict)");
    }
    let description = commit.description.lines().next().unwrap_or("");
    let description = if description.is_empty() {
        "(no description)"
    } else {
        description
    };
    let when = chrono::DateTime::from_timestamp_millis(commit.committer.time.ms)
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "{} {}{labels}{flags} {description} ({}, {when})",
        id(&commit.change_id, commit.change_prefix),
        id(&commit.commit_id, commit.commit_prefix),
        commit.author.name,
    );
}

async fn jj_log(app: &Arc<App>, root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let workspace = required(args, "workspace")?.to_owned();
    let revset = string(args, "revset").map(str::to_owned);
    let limit = number(args, "limit").unwrap_or(30).clamp(1, 500);
    let log = in_workspace(app, root, &workspace, async move |session| {
        session.log(revset.as_deref(), limit).await
    })
    .await?;
    let mut out = format!("{} in {workspace}: change id, commit id\n", log.revset);
    for node in &log.nodes {
        commit_line(&node.commit, &mut out);
    }
    if log.has_more {
        out.push_str("… and more; raise limit to see them.\n");
    }
    Ok(Value::String(capped(out)))
}

async fn jj_diff(app: &Arc<App>, root: &Arc<Root>, args: &Value) -> Result<Value, String> {
    let workspace = required(args, "workspace")?.to_owned();
    let to = required(args, "to")?.to_owned();
    let from = string(args, "from").map(str::to_owned);
    let path = string(args, "path").map(str::to_owned);
    let result = in_workspace(app, root, &workspace, async move |session| {
        session.diff(from.as_deref(), &to, path.as_deref(), 3).await
    })
    .await?;
    let mut out = String::from("to: ");
    commit_line(&result.to, &mut out);
    if let Some(from) = &result.from {
        out.push_str("from: ");
        commit_line(from, &mut out);
    }
    for file in &result.files {
        let _ = writeln!(
            out,
            "\n{}: {:?}, +{} -{}",
            file.path, file.status, file.added, file.removed
        );
        if file.deferred {
            out.push_str("(too big to include; ask for this path alone)\n");
        }
        for hunk in &file.hunks {
            hunk_text(hunk, &mut out);
        }
    }
    if result.truncated {
        out.push_str("\n… more files changed than are listed.\n");
    }
    Ok(Value::String(capped(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_every_tool_once() {
        let specs = specs();
        let names: Vec<&str> = specs[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        for page_tool in PAGE_TOOLS {
            assert!(names.contains(page_tool), "{page_tool}");
        }
        assert!(names.contains(&"tour"));
    }

    #[test]
    fn numbers_lines_as_the_file_has_them() {
        let mut file = crate::diff::text_diff("a.txt", "one\ntwo\nthree\n", "one\n2\nthree\n", 1);
        let mut out = String::new();
        hunk_text(&file.hunks.remove(0), &mut out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("@@ -1,3 +1,3 @@"), "{out}");
        assert_eq!(
            &lines[1..],
            ["    1   one", "    2 - two", "    2 + 2", "    3   three"]
        );
    }

    #[test]
    fn cuts_long_answers_short() {
        let text = capped("é".repeat(MOST_TEXT));
        assert!(text.len() < MOST_TEXT + 100);
        assert!(text.ends_with("ask for less at a time."));
    }
}
