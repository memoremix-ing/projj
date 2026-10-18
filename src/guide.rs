//! The guide: a conversation with Codex in a drawer beside the review, one
//! per root, that explains changes, walks through them in a sensible order,
//! answers questions about any file, and points at what it means.
//!
//! Codex runs read-only and in code mode: it acts by writing JavaScript that
//! calls tools projj gives it (see `tools`). The tools that read answer from
//! the root; those that act on the page are handed to the page that sent the
//! latest message, and Codex waits for its answer. Pages learn of everything,
//! replies as they are written included, by asking what happened since they
//! last asked (`Conversation::events`), which waits until something has. That
//! works the same over HTTP and through the desktop app's bridge.
//!
//! A conversation lasts as long as the server: Codex keeps nothing of it.

mod tools;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::api::App;
use crate::api::Root;
use crate::codex::Codex;
use crate::codex::Incoming;
use crate::codex::Process;

/// How long a page's request for events waits for one.
const WAIT_FOR_EVENTS: Duration = Duration::from_secs(20);
/// A page that has not asked for events for this long is taken to be gone.
const PAGE_GONE_AFTER: Duration = Duration::from_secs(45);
/// How long Codex waits for the page to show something.
const PAGE_ANSWERS_WITHIN: Duration = Duration::from_secs(20);
/// Events kept for pages that fall behind; one further behind starts over.
const KEPT_EVENTS: usize = 4000;
/// The most of the page's description sent with each message.
const MOST_CONTEXT: usize = 24_000;

const NO_PAGE: &str =
    "The review page is not open, so nothing can be shown there. Say it in words instead.";

/// Every root's conversation.
pub struct Guide {
    codex: Arc<Codex>,
    conversations: Mutex<HashMap<PathBuf, Arc<Conversation>>>,
}

impl Guide {
    /// A guide that runs Codex from `program`, or one that explains that
    /// Codex is not installed.
    pub fn new(program: Option<PathBuf>) -> Self {
        Self {
            codex: Arc::new(Codex::new(program)),
            conversations: Mutex::default(),
        }
    }

    pub fn conversation(&self, app: &Arc<App>, root: &Arc<Root>) -> Arc<Conversation> {
        self.conversations
            .lock()
            .unwrap()
            .entry(root.path.clone())
            .or_insert_with(|| {
                Arc::new(Conversation {
                    codex: self.codex.clone(),
                    root: root.clone(),
                    app: Arc::downgrade(app),
                    state: Mutex::default(),
                    seq: watch::channel(0).0,
                    starting: tokio::sync::Mutex::new(()),
                })
            })
            .clone()
    }

    /// Why the guide cannot be used, if it cannot. Starts Codex to find out.
    pub async fn problem(&self) -> Option<String> {
        if self.codex.program().is_none() {
            return Some(
                "Codex is not installed here. Install the Codex command line (codex), sign in \
                 with `codex login`, then reload this page."
                    .into(),
            );
        }
        let process = match self.codex.process().await {
            Ok(process) => process,
            Err(err) => return Some(format!("Codex cannot be started: {err:#}")),
        };
        match process.request("account/read", json!({})).await {
            Ok(account) if account["account"].is_null() && account["requiresOpenaiAuth"] == true => Some(
                "Codex is not signed in. Run `codex login` in a terminal, then reload this page."
                    .into(),
            ),
            Ok(_) => None,
            Err(err) => Some(format!("Codex is not answering: {err:#}")),
        }
    }

    pub fn program(&self) -> Option<String> {
        self.codex
            .program()
            .map(|program| program.display().to_string())
    }
}

/// The thread a conversation is having with Codex.
struct Thread {
    id: String,
    process: Arc<Process>,
}

#[derive(Default)]
struct State {
    thread: Option<Thread>,
    turn: Option<String>,
    running: bool,
    /// What the drawer shows: the person's messages, Codex's replies and
    /// what it did, tours, errors. Each has an `id` and a `kind`.
    entries: Vec<Value>,
    /// What changed, numbered by `seq`, for pages to catch up on.
    events: VecDeque<Value>,
    seq: u64,
    /// The pages showing this conversation, and when each last asked.
    pages: HashMap<String, Instant>,
    /// The page that sent the latest message: where the guide points.
    driver: Option<String>,
    /// Requests to pages, waiting for an answer.
    waiting: HashMap<String, oneshot::Sender<Result<Value, String>>>,
    next: u64,
    /// Goes up when the conversation starts over, so that what was under
    /// way for the old one is dropped.
    generation: u64,
}

impl State {
    fn push(&mut self, mut event: Value) {
        self.seq += 1;
        event["seq"] = self.seq.into();
        if self.events.len() == KEPT_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    /// Adds an entry, or replaces the one with its id.
    fn put(&mut self, entry: Value) {
        match self.entries.iter_mut().find(|old| old["id"] == entry["id"]) {
            Some(old) => *old = entry.clone(),
            None => self.entries.push(entry.clone()),
        }
        self.push(json!({ "type": "entry", "entry": entry }));
    }

    fn say(&mut self, kind: &str, text: impl Into<String>) {
        let id = self.id("n");
        self.put(json!({ "id": id, "kind": kind, "text": text.into() }));
    }

    fn set_running(&mut self, running: bool) {
        self.running = running;
        self.push(json!({ "type": "status", "running": running }));
    }

    /// The page to point on: the one that sent the latest message, or else
    /// the one heard from last.
    fn page(&self) -> Option<String> {
        let alive = |seen: &Instant| seen.elapsed() < PAGE_GONE_AFTER;
        if let Some(driver) = &self.driver
            && self.pages.get(driver).is_some_and(alive)
        {
            return Some(driver.clone());
        }
        self.pages
            .iter()
            .filter(|(_, seen)| alive(seen))
            .max_by_key(|(_, seen)| **seen)
            .map(|(page, _)| page.clone())
    }

    fn snapshot(&self) -> Value {
        json!({ "seq": self.seq, "running": self.running, "entries": self.entries })
    }
}

/// What a thread item looks like in the drawer, if it shows at all.
fn entry_of(item: &Value, done: bool) -> Option<Value> {
    let id = item["id"].as_str()?;
    let status = item["status"].clone();
    Some(match item["type"].as_str()? {
        "agentMessage" => json!({
            "id": id,
            "kind": "agent",
            "text": item["text"],
            "phase": item["phase"],
            "done": done,
        }),
        "dynamicToolCall" => json!({
            "id": id,
            "kind": "tool",
            "tool": item["tool"],
            "args": item["arguments"],
            "status": status,
            "ok": item["success"],
        }),
        "commandExecution" => json!({
            "id": id,
            "kind": "command",
            "command": item["command"],
            "status": status,
            "exit": item["exitCode"],
        }),
        "fileChange" => json!({
            "id": id,
            "kind": "command",
            "command": "edit files",
            "status": status,
        }),
        "webSearch" | "mcpToolCall" | "imageView" => json!({
            "id": id,
            "kind": "tool",
            "tool": item["tool"].as_str().or(item["type"].as_str()),
            "status": status,
        }),
        "contextCompaction" if done => json!({
            "id": id,
            "kind": "note",
            "text": "Codex condensed the conversation so far to make room.",
        }),
        _ => return None,
    })
}

/// A root's conversation with Codex, and the pages showing it.
pub struct Conversation {
    codex: Arc<Codex>,
    root: Arc<Root>,
    app: Weak<App>,
    state: Mutex<State>,
    /// The latest event's number, for pages waiting for one.
    seq: watch::Sender<u64>,
    /// Held while a thread is being started, so that only one is.
    starting: tokio::sync::Mutex<()>,
}

impl Conversation {
    /// Changes the state, then wakes the pages waiting for events.
    fn with<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let (out, seq) = {
            let mut state = self.state.lock().unwrap();
            let out = change(&mut state);
            (out, state.seq)
        };
        self.seq.send_if_modified(|latest| {
            let changed = *latest != seq;
            *latest = seq;
            changed
        });
        out
    }

    fn seen(&self, page: &str) {
        if !page.is_empty() {
            self.state
                .lock()
                .unwrap()
                .pages
                .insert(page.to_owned(), Instant::now());
        }
    }

    /// Everything a page needs to show the conversation.
    pub fn snapshot(&self, page: &str) -> Value {
        self.seen(page);
        self.state.lock().unwrap().snapshot()
    }

    /// What happened after event `after`, waiting a while for something if
    /// nothing has. A page too far behind, or ahead (the server restarted),
    /// gets everything instead, marked `resync`.
    pub async fn events(&self, page: &str, after: u64) -> Value {
        self.seen(page);
        let mut latest = self.seq.subscribe();
        let deadline = tokio::time::Instant::now() + WAIT_FOR_EVENTS;
        while *latest.borrow_and_update() == after {
            if tokio::time::timeout_at(deadline, latest.changed())
                .await
                .is_err()
            {
                break;
            }
        }
        self.seen(page);
        let state = self.state.lock().unwrap();
        let oldest = state
            .events
            .front()
            .map_or(state.seq + 1, |event| event["seq"].as_u64().unwrap_or(0));
        if after > state.seq || oldest > after + 1 {
            let mut snapshot = state.snapshot();
            snapshot["resync"] = true.into();
            return snapshot;
        }
        let events: Vec<&Value> = state
            .events
            .iter()
            .filter(|event| event["seq"].as_u64().is_some_and(|seq| seq > after))
            .collect();
        json!({ "seq": state.seq, "running": state.running, "events": events })
    }

    /// The page has gone; nothing more is shown on it.
    pub fn leave(&self, page: &str) {
        let mut state = self.state.lock().unwrap();
        state.pages.remove(page);
        if state.driver.as_deref() == Some(page) {
            state.driver = None;
        }
    }

    /// The thread to talk in, started if there is none.
    async fn thread(self: &Arc<Self>) -> Result<(String, Arc<Process>)> {
        let _starting = self.starting.lock().await;
        let stale = {
            let state = self.state.lock().unwrap();
            match &state.thread {
                Some(thread) if thread.process.stopped().is_none() => {
                    return Ok((thread.id.clone(), thread.process.clone()));
                }
                stale => stale.is_some(),
            }
        };
        let process = self.codex.process().await?;
        let root = &self.root.path;
        let config = tools::config(&process, root).await;
        let projj = root.join(crate::root::TRACK_DIR).is_dir();
        let started = process
            .request(
                "thread/start",
                json!({
                    "cwd": root,
                    "sandbox": "read-only",
                    "approvalPolicy": "never",
                    "ephemeral": true,
                    "serviceName": "projj",
                    "config": config,
                    "developerInstructions": tools::instructions(root, projj),
                    "dynamicTools": tools::specs(),
                }),
            )
            .await?;
        let id = started["thread"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Codex started a thread with no id"))?
            .to_owned();
        let incoming = process.subscribe(&id);
        let generation = self.with(|state| {
            if stale && !state.entries.is_empty() {
                state.say("note", "Codex started again, without what was said above.");
            }
            state.thread = Some(Thread {
                id: id.clone(),
                process: process.clone(),
            });
            state.generation
        });
        tokio::spawn(self.clone().listen(incoming, process.clone(), generation));
        Ok((id, process))
    }

    /// Sends the person's message, with a description of what their page
    /// shows. While Codex is still answering, the message joins that turn.
    pub async fn send(
        self: &Arc<Self>,
        page: &str,
        text: &str,
        context: &Value,
        effort: Option<&str>,
    ) -> Result<()> {
        self.with(|state| {
            state.pages.insert(page.to_owned(), Instant::now());
            state.driver = Some(page.to_owned());
            let id = state.id("u");
            state.put(json!({ "id": id, "kind": "user", "text": text }));
        });
        let (thread, process) = match self.thread().await {
            Ok(thread) => thread,
            Err(err) => {
                self.with(|state| state.say("error", format!("{err:#}")));
                return Err(err);
            }
        };
        let mut described = context.to_string();
        if described.len() > MOST_CONTEXT {
            described = serde_json::to_string(
                &json!({ "note": "The page's description was too long to send." }),
            )?;
        }
        let input = json!([{ "type": "text", "text": text, "text_elements": [] }]);
        let context = json!({ "projj-view": { "value": described, "kind": "application" } });
        let active = {
            let state = self.state.lock().unwrap();
            state.turn.clone().filter(|_| state.running)
        };
        if let Some(turn) = active {
            let steered = process
                .request(
                    "turn/steer",
                    json!({
                        "threadId": thread,
                        "input": input,
                        "additionalContext": context,
                        "expectedTurnId": turn,
                    }),
                )
                .await;
            // The turn may have ended in the meantime: start another.
            if steered.is_ok() {
                return Ok(());
            }
        }
        self.with(|state| state.set_running(true));
        let mut params =
            json!({ "threadId": thread, "input": input, "additionalContext": context });
        if let Some(effort) = effort {
            params["effort"] = effort.into();
        }
        match process.request("turn/start", params).await {
            Ok(started) => {
                if let Some(turn) = started["turn"]["id"].as_str() {
                    self.with(|state| {
                        if state.running && state.turn.is_none() {
                            state.turn = Some(turn.to_owned());
                        }
                    });
                }
                Ok(())
            }
            Err(err) => {
                self.with(|state| {
                    state.set_running(false);
                    state.say("error", format!("{err:#}"));
                });
                Err(err)
            }
        }
    }

    /// Stops Codex mid-answer.
    pub async fn stop(&self) -> Result<()> {
        let (thread, process, turn) = {
            let state = self.state.lock().unwrap();
            match (&state.thread, &state.turn) {
                (Some(thread), Some(turn)) if state.running => {
                    (thread.id.clone(), thread.process.clone(), turn.clone())
                }
                _ => return Ok(()),
            }
        };
        process
            .request(
                "turn/interrupt",
                json!({ "threadId": thread, "turnId": turn }),
            )
            .await?;
        Ok(())
    }

    /// Starts over: what was said is gone, for Codex and the drawer alike.
    pub async fn reset(&self) {
        let (thread, turn) = self.with(|state| {
            state.generation += 1;
            let turn = state.turn.take().filter(|_| state.running);
            state.running = false;
            state.entries.clear();
            state.waiting.clear();
            state.push(json!({ "type": "reset" }));
            (state.thread.take(), turn)
        });
        if let Some(thread) = thread {
            thread.process.unsubscribe(&thread.id);
            if let Some(turn) = turn {
                let _ = thread
                    .process
                    .request(
                        "turn/interrupt",
                        json!({ "threadId": thread.id, "turnId": turn }),
                    )
                    .await;
            }
            let _ = thread
                .process
                .request("thread/unsubscribe", json!({ "threadId": thread.id }))
                .await;
        }
    }

    /// The page's answer to a request made of it.
    pub fn answer(&self, call: &str, result: Result<Value, String>) -> bool {
        let waiting = self.state.lock().unwrap().waiting.remove(call);
        waiting.is_some_and(|waiting| waiting.send(result).is_ok())
    }

    async fn listen(
        self: Arc<Self>,
        mut incoming: mpsc::UnboundedReceiver<Incoming>,
        process: Arc<Process>,
        generation: u64,
    ) {
        while let Some(message) = incoming.recv().await {
            if self.state.lock().unwrap().generation != generation {
                return;
            }
            match message {
                Incoming::Notification { method, params } => self.notified(&method, &params),
                Incoming::Request { id, method, params } if method == "item/tool/call" => {
                    let (conversation, process) = (self.clone(), process.clone());
                    tokio::spawn(async move {
                        let tool = params["tool"].as_str().unwrap_or_default();
                        let (ok, text) = match conversation.call(tool, &params["arguments"]).await {
                            Ok(Value::String(text)) => (true, text),
                            Ok(value) => (true, value.to_string()),
                            Err(message) => (false, message),
                        };
                        let content = json!([{ "type": "inputText", "text": text }]);
                        let _ = process
                            .respond(id, json!({ "contentItems": content, "success": ok }))
                            .await;
                    });
                }
                Incoming::Request { id, method, .. } => {
                    let _ = process.refuse(id, &method).await;
                }
                Incoming::Closed(reason) => {
                    self.with(|state| {
                        state.thread = None;
                        state.turn = None;
                        state.set_running(false);
                        state.say(
                            "error",
                            format!("{reason}. Send another message to start it again."),
                        );
                    });
                    return;
                }
            }
        }
    }

    fn notified(&self, method: &str, params: &Value) {
        self.with(|state| match method {
            "turn/started" => {
                state.turn = params["turn"]["id"].as_str().map(str::to_owned);
                if !state.running {
                    state.set_running(true);
                }
            }
            "turn/completed" => {
                state.turn = None;
                state.set_running(false);
                let turn = &params["turn"];
                match turn["status"].as_str() {
                    Some("failed") => {
                        let message = turn["error"]["message"]
                            .as_str()
                            .unwrap_or("Codex could not answer.");
                        state.say("error", message);
                    }
                    Some("interrupted") => state.say("note", "Stopped."),
                    _ => {}
                }
            }
            "item/started" | "item/completed" => {
                if let Some(entry) = entry_of(&params["item"], method == "item/completed") {
                    state.put(entry);
                }
            }
            "item/agentMessage/delta" => {
                let (Some(id), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str())
                else {
                    return;
                };
                match state.entries.iter_mut().find(|entry| entry["id"] == id) {
                    Some(entry) => {
                        let text = format!("{}{delta}", entry["text"].as_str().unwrap_or(""));
                        entry["text"] = text.into();
                        state.push(json!({ "type": "delta", "id": id, "text": delta }));
                    }
                    None => state
                        .put(json!({ "id": id, "kind": "agent", "text": delta, "done": false })),
                }
            }
            "error" if params["willRetry"] != true => {
                let message = params["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex ran into an error.");
                state.say("error", message);
            }
            _ => {}
        });
    }

    /// Answers one of the guide's tools.
    async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        if tools::PAGE_TOOLS.contains(&tool) {
            return self.ask_page(tool, args).await;
        }
        if tool == "tour" {
            return self.tour(args);
        }
        let app = self.app.upgrade().ok_or("projj is stopping")?;
        tools::run(&app, &self.root, tool, args).await
    }

    /// Hands a tool to the page and waits for what it did.
    async fn ask_page(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let (call, answered) = self.with(|state| {
            let page = state.page().ok_or(NO_PAGE)?;
            let call = state.id("c");
            let (answer, answered) = oneshot::channel();
            state.waiting.insert(call.clone(), answer);
            state.push(
                json!({ "type": "call", "call": call, "page": page, "tool": tool, "args": args }),
            );
            Ok::<_, String>((call, answered))
        })?;
        match tokio::time::timeout(PAGE_ANSWERS_WITHIN, answered).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("The conversation was started over.".into()),
            Err(_) => {
                self.state.lock().unwrap().waiting.remove(&call);
                Err("The page did not answer; it may have been closed.".into())
            }
        }
    }

    /// Puts a walkthrough in the drawer.
    fn tour(&self, args: &Value) -> Result<Value, String> {
        let title = args["title"].as_str().unwrap_or("Walkthrough");
        let steps = args["steps"]
            .as_array()
            .filter(|steps| !steps.is_empty())
            .ok_or("a tour needs steps")?;
        if let Some(index) = steps
            .iter()
            .position(|step| step["title"].as_str().is_none_or(str::is_empty))
        {
            return Err(format!("step {} has no title", index + 1));
        }
        let count = steps.len();
        self.with(|state| {
            let id = state.id("t");
            state.put(json!({ "id": id, "kind": "tour", "title": title, "steps": steps }));
        });
        Ok(Value::String(format!(
            "The tour is in the chat panel with {count} steps, which the person can step \
             through themselves. Now explain step 1 and show it."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation() -> (tempfile::TempDir, Arc<Conversation>) {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::new(dir.path().to_owned()));
        let conversation = Arc::new(Conversation {
            codex: Arc::new(Codex::new(None)),
            root,
            app: Weak::new(),
            state: Mutex::default(),
            seq: watch::channel(0).0,
            starting: tokio::sync::Mutex::new(()),
        });
        (dir, conversation)
    }

    #[tokio::test]
    async fn pages_catch_up_on_what_happened() {
        let (_dir, conversation) = conversation();
        let before = conversation.snapshot("page")["seq"].as_u64().unwrap();
        let waiting = {
            let conversation = conversation.clone();
            tokio::spawn(async move { conversation.events("page", before).await })
        };
        conversation.notified(
            "item/started",
            &json!({ "item": { "type": "agentMessage", "id": "m", "text": "", "phase": "final_answer" } }),
        );
        let first = waiting.await.unwrap();
        assert_eq!(first["events"][0]["entry"]["kind"], "agent");
        let seq = first["seq"].as_u64().unwrap();

        for delta in ["Hel", "lo"] {
            conversation.notified(
                "item/agentMessage/delta",
                &json!({ "itemId": "m", "delta": delta }),
            );
        }
        let next = conversation.events("page", seq).await;
        let deltas: Vec<&str> = next["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["text"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, ["Hel", "lo"]);
        assert_eq!(conversation.snapshot("page")["entries"][0]["text"], "Hello");

        // A page from before a restart starts over.
        let lost = conversation.events("page", 10_000).await;
        assert_eq!(lost["resync"], true);
        assert_eq!(lost["entries"][0]["text"], "Hello");
    }

    #[tokio::test]
    async fn the_page_that_asked_answers() {
        let (_dir, conversation) = conversation();
        assert_eq!(
            conversation.call("show", &json!({ "path": "a" })).await,
            Err(NO_PAGE.into())
        );
        let seq = conversation.snapshot("page")["seq"].as_u64().unwrap();
        let asking = {
            let conversation = conversation.clone();
            tokio::spawn(async move { conversation.call("show", &json!({ "path": "a" })).await })
        };
        let events = conversation.events("page", seq).await;
        let call = &events["events"][0];
        assert_eq!(
            (call["type"].as_str(), call["page"].as_str()),
            (Some("call"), Some("page"))
        );
        assert!(conversation.answer(
            call["call"].as_str().unwrap(),
            Ok(json!({ "shown": "diff" }))
        ));
        assert_eq!(asking.await.unwrap(), Ok(json!({ "shown": "diff" })));

        conversation.leave("page");
        assert_eq!(
            conversation.call("view", &json!({})).await,
            Err(NO_PAGE.into())
        );
    }

    #[tokio::test]
    async fn tours_go_in_the_drawer() {
        let (_dir, conversation) = conversation();
        assert!(
            conversation
                .call("tour", &json!({ "steps": [] }))
                .await
                .is_err()
        );
        let steps = json!([{ "title": "Types", "places": [{ "path": "src/a.rs", "line": 3 }] }]);
        conversation
            .call("tour", &json!({ "title": "The change", "steps": steps }))
            .await
            .unwrap();
        let entry = &conversation.snapshot("")["entries"][0];
        assert_eq!(
            (entry["kind"].as_str(), entry["title"].as_str()),
            (Some("tour"), Some("The change"))
        );
    }
}
