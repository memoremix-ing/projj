//! A client for `codex app-server`: the JSON-RPC protocol, over a child
//! process's standard input and output, that Codex's own editor extensions
//! speak. The guide (see `guide`) is built on it.
//!
//! One process serves every conversation on the device's server. It is
//! started when first needed, and again if it stops.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// How long Codex has to answer a request before projj gives up on it.
const ANSWER_WITHIN: Duration = Duration::from_secs(90);

/// Lines of Codex's standard error kept to explain why it stopped.
const KEPT_LINES: usize = 40;

/// Notifications projj has no use for, which Codex is asked not to send.
const UNWANTED: &[&str] = &[
    "account/rateLimits/updated",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "mcpServer/startupStatus/updated",
    "remoteControl/status/changed",
    "thread/status/changed",
    "thread/tokenUsage/updated",
    "turn/diff/updated",
    "turn/moderationMetadata",
];

/// Where Codex is installed, if anywhere projj knows to look:
/// `PROJJ_CODEX`, then the `PATH`, then where installers put it. An app
/// started from the Dock gets a short `PATH`, hence the list.
pub fn locate() -> Option<PathBuf> {
    if let Some(program) = std::env::var_os("PROJJ_CODEX").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(program));
    }
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
            ".codex/bin",
            ".codex/packages/app-server-daemon/current/bin",
        ] {
            dirs.push(home.join(dir));
        }
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// What the process sends about one thread.
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Codex asks something; answer with `Process::respond`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The process stopped, for the reason given.
    Closed(String),
}

type Answer = oneshot::Sender<Result<Value, String>>;

/// A running `codex app-server`.
pub struct Process {
    stdin: tokio::sync::Mutex<ChildStdin>,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, Answer>>,
    threads: Mutex<HashMap<String, mpsc::UnboundedSender<Incoming>>>,
    /// Why it stopped, once it has.
    stopped: Mutex<Option<String>>,
    /// What it last wrote to standard error.
    said: Arc<Mutex<VecDeque<String>>>,
    /// Held so that the process ends with this value.
    _child: Mutex<Child>,
}

impl Process {
    async fn start(program: &Path) -> Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(program)
            .args([
                "app-server",
                "-c",
                "suppress_unstable_features_warning=true",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot run {}", program.display()))?;
        let stdin = child.stdin.take().context("no input to Codex")?;
        let stdout = child.stdout.take().context("no output from Codex")?;
        let stderr = child.stderr.take().context("no errors from Codex")?;
        let said = Arc::new(Mutex::new(VecDeque::new()));
        let process = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            next: AtomicU64::new(1),
            waiting: Mutex::default(),
            threads: Mutex::default(),
            stopped: Mutex::new(None),
            said: said.clone(),
            _child: Mutex::new(child),
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut said = said.lock().unwrap();
                if said.len() == KEPT_LINES {
                    said.pop_front();
                }
                said.push_back(line);
            }
        });
        // The reader holds the process only weakly, so that dropping the last
        // handle ends it.
        let reader = Arc::downgrade(&process);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = lines.next_line().await;
                let Some(process) = reader.upgrade() else {
                    return;
                };
                match line {
                    Ok(Some(line)) => process.receive(&line).await,
                    Ok(None) => return process.close("Codex stopped".into()),
                    Err(err) => return process.close(format!("cannot read from Codex: {err}")),
                }
            }
        });
        let version = env!("CARGO_PKG_VERSION");
        process
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "projj", "title": "projj", "version": version },
                    "capabilities": {
                        "experimentalApi": true,
                        "requestAttestation": false,
                        "optOutNotificationMethods": UNWANTED,
                    },
                }),
            )
            .await
            .context("Codex did not start")?;
        process.notify("initialized", Value::Null).await?;
        Ok(process)
    }

    /// Whether the process has stopped, and why.
    pub fn stopped(&self) -> Option<String> {
        self.stopped.lock().unwrap().clone()
    }

    fn close(&self, reason: String) {
        let said: Vec<String> = self.said.lock().unwrap().iter().cloned().collect();
        let last = said
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| format!(": {}", line.trim()))
            .unwrap_or_default();
        let reason = format!("{reason}{last}");
        *self.stopped.lock().unwrap() = Some(reason.clone());
        for (_, answer) in self.waiting.lock().unwrap().drain() {
            let _ = answer.send(Err(reason.clone()));
        }
        for (_, thread) in self.threads.lock().unwrap().drain() {
            let _ = thread.send(Incoming::Closed(reason.clone()));
        }
    }

    async fn receive(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (None, Some(id)) => {
                let Some(answer) = id
                    .as_u64()
                    .and_then(|id| self.waiting.lock().unwrap().remove(&id))
                else {
                    return;
                };
                let result = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string())),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = answer.send(result);
            }
            (Some(method), id) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let thread = params
                    .get("threadId")
                    .or_else(|| params.get("conversationId"))
                    .and_then(Value::as_str)
                    .and_then(|thread| self.threads.lock().unwrap().get(thread).cloned());
                let incoming = match id {
                    Some(id) => Incoming::Request {
                        id: id.clone(),
                        method: method.to_owned(),
                        params,
                    },
                    None => Incoming::Notification {
                        method: method.to_owned(),
                        params,
                    },
                };
                match thread {
                    Some(thread) => {
                        let _ = thread.send(incoming);
                    }
                    // A question about no thread of projj's gets no for an answer.
                    None => {
                        if let Incoming::Request { id, method, .. } = incoming {
                            let _ = self.refuse(id, &method).await;
                        }
                    }
                }
            }
            (None, None) => {}
        }
    }

    async fn write(&self, message: Value) -> Result<()> {
        if let Some(reason) = self.stopped() {
            return Err(anyhow!(reason));
        }
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("cannot write to Codex")?;
        stdin.flush().await.context("cannot write to Codex")
    }

    /// Asks Codex something and waits for the answer.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, answer);
        if let Err(err) = self
            .write(json!({ "id": id, "method": method, "params": params }))
            .await
        {
            self.waiting.lock().unwrap().remove(&id);
            return Err(err);
        }
        match tokio::time::timeout(ANSWER_WITHIN, answered).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(anyhow!("{method}: {message}")),
            Ok(Err(_)) => Err(anyhow!("Codex stopped")),
            Err(_) => {
                self.waiting.lock().unwrap().remove(&id);
                Err(anyhow!("Codex did not answer {method}"))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut message = json!({ "method": method });
        if !params.is_null() {
            message["params"] = params;
        }
        self.write(message).await
    }

    /// Answers a request from Codex.
    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.write(json!({ "id": id, "result": result })).await
    }

    pub async fn respond_error(&self, id: Value, message: &str) -> Result<()> {
        self.write(json!({ "id": id, "error": { "code": -32601, "message": message } }))
            .await
    }

    /// Turns down anything Codex asks permission for: the guide only reads.
    pub async fn refuse(&self, id: Value, method: &str) -> Result<()> {
        const READ_ONLY: &str = "projj's guide only reads; it cannot change anything";
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                self.respond(id, json!({ "decision": "decline" })).await
            }
            "execCommandApproval" | "applyPatchApproval" => {
                self.respond(
                    id,
                    json!({ "decision": { "denied": { "rejection": READ_ONLY } } }),
                )
                .await
            }
            "mcpServer/elicitation/request" => {
                self.respond(
                    id,
                    json!({ "action": "decline", "content": null, "_meta": null }),
                )
                .await
            }
            _ => {
                self.respond_error(id, &format!("projj does not answer {method}"))
                    .await
            }
        }
    }

    /// Hands everything Codex sends about `thread` to the receiver.
    pub fn subscribe(&self, thread: &str) -> mpsc::UnboundedReceiver<Incoming> {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Some(reason) = self.stopped() {
            let _ = sender.send(Incoming::Closed(reason));
        }
        self.threads
            .lock()
            .unwrap()
            .insert(thread.to_owned(), sender);
        receiver
    }

    pub fn unsubscribe(&self, thread: &str) {
        self.threads.lock().unwrap().remove(thread);
    }
}

/// Codex, as far as projj is concerned: where it is, and the process once
/// one is running.
pub struct Codex {
    program: Option<PathBuf>,
    process: tokio::sync::Mutex<Option<Arc<Process>>>,
}

impl Codex {
    pub fn new(program: Option<PathBuf>) -> Self {
        Self {
            program,
            process: tokio::sync::Mutex::new(None),
        }
    }

    pub fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }

    /// The running process, started if there is none.
    pub async fn process(&self) -> Result<Arc<Process>> {
        let mut process = self.process.lock().await;
        if let Some(running) = &*process
            && running.stopped().is_none()
        {
            return Ok(running.clone());
        }
        let program = self
            .program
            .as_deref()
            .ok_or_else(|| anyhow!("Codex is not installed"))?;
        let started = Process::start(program).await?;
        *process = Some(started.clone());
        Ok(started)
    }
}
