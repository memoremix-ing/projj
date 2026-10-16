//! What projj keeps between sessions, in one database for the whole device:
//! unsaved editor text, display settings, and the roots opened lately.
//!
//! Every projj server on the machine opens the same Turso database file. Each
//! draft is filed under the root it was written in, so edits follow the root,
//! whichever port it is served on and whichever browser the page is opened
//! in. Settings belong to the person rather than a root, and are shared by
//! every root.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use turso::Builder;
use turso::Connection;
use turso::Database;
use turso::Row;

use crate::files::Eol;

const FILE_NAME: &str = "projj.db";

/// How long to wait for another server that is writing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS drafts (
    root TEXT NOT NULL,
    path TEXT NOT NULL,
    eol TEXT NOT NULL,
    version TEXT NOT NULL,
    original TEXT NOT NULL,
    content TEXT NOT NULL,
    edited INTEGER,
    PRIMARY KEY (root, path)
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS roots (
    path TEXT PRIMARY KEY,
    opened INTEGER NOT NULL
);";

/// How many recently opened roots are remembered.
const RECENT_ROOTS: i64 = 30;

#[derive(Debug, Error)]
pub enum StoreError {
    /// An update left out the content the edits started from, and the store
    /// does not have it for that version of the file.
    #[error("the content these edits started from is not stored; send `original` too")]
    NeedsOriginal,
    #[error("cannot use the store at {}", file.display())]
    Store {
        file: PathBuf,
        #[source]
        source: anyhow::Error,
    },
}

type Result<T> = std::result::Result<T, StoreError>;

/// Unsaved edits to one file.
#[derive(Debug, PartialEq, Serialize)]
pub struct Draft {
    /// Path relative to the root.
    pub path: String,
    pub eol: Eol,
    /// The version of the file the edits started from, and its content.
    pub version: String,
    pub original: String,
    pub content: String,
    /// When the text was last typed in, in milliseconds since the epoch.
    pub edited: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub path: String,
    pub eol: Eol,
    pub version: String,
    /// May be left out once the store has it for this `version`, so that a
    /// keystroke does not resend the whole file it started from.
    pub original: Option<String>,
    pub content: String,
    pub edited: Option<i64>,
}

/// One line of the device-wide listing.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub root: String,
    pub path: String,
    pub edited: Option<i64>,
    pub bytes: i64,
}

/// Where the store is kept unless told otherwise: the per-user data directory
/// of the platform.
pub fn default_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(dir) = var("XDG_DATA_HOME") {
        return Some(PathBuf::from(dir).join("projj"));
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("projj"));
    }
    let home = PathBuf::from(var("HOME")?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/projj")
    } else {
        home.join(".local/share/projj")
    })
}

/// The name a root's drafts are filed under: its resolved absolute path.
pub fn root_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_owned())
        .to_string_lossy()
        .into_owned()
}

pub struct Store {
    file: PathBuf,
    /// Opened on first use and again after a failure, so that a store that is
    /// briefly unusable does not stay that way for the life of the server.
    open: Mutex<Option<(Database, Connection)>>,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            file: dir.join(FILE_NAME),
            open: Mutex::new(None),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn fail(&self, source: impl Into<anyhow::Error>) -> StoreError {
        StoreError::Store {
            file: self.file.clone(),
            source: source.into(),
        }
    }

    async fn open(&self) -> Result<(Database, Connection)> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|err| self.fail(err))?;
        }
        let file = self
            .file
            .to_str()
            .ok_or_else(|| self.fail(anyhow::anyhow!("the path is not valid UTF-8")))?;
        // Several servers share the file, which Turso only allows in this
        // mode; every process that opens it has to ask for the same.
        let database = Builder::new_local(file)
            .experimental_multiprocess_wal(true)
            .build()
            .await
            .map_err(|err| self.fail(err))?;
        let connection = database.connect().map_err(|err| self.fail(err))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|err| self.fail(err))?;
        connection
            .execute_batch(SCHEMA)
            .await
            .map_err(|err| self.fail(err))?;
        Ok((database, connection))
    }

    /// Runs one operation at a time on the connection, opening it if needed.
    async fn connection(&self) -> Result<Guard<'_>> {
        let mut open = self.open.lock().await;
        if open.is_none() {
            *open = Some(self.open().await?);
        }
        Ok(Guard { store: self, open })
    }

    /// The drafts filed under `root`, by path.
    pub async fn drafts(&self, root: &str) -> Result<Vec<Draft>> {
        let mut guard = self.connection().await?;
        let rows = guard
            .rows(
                "SELECT path, eol, version, original, content, edited FROM drafts
                 WHERE root = ?1 ORDER BY path",
                (root,),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(Draft {
                    path: row.get(0)?,
                    eol: if row.get::<String>(1)? == "crlf" {
                        Eol::Crlf
                    } else {
                        Eol::Lf
                    },
                    version: row.get(2)?,
                    original: row.get(3)?,
                    content: row.get(4)?,
                    edited: row.get(5)?,
                })
            })
            .collect::<turso::Result<_>>()
            .map_err(|err| self.fail(err))
    }

    /// Stores the latest text of a draft, replacing what was there.
    pub async fn put_draft(&self, root: &str, update: Update) -> Result<()> {
        let eol = match update.eol {
            Eol::Lf => "lf",
            Eol::Crlf => "crlf",
        };
        let mut guard = self.connection().await?;
        let Some(original) = update.original else {
            let changed = guard
                .execute(
                    "UPDATE drafts SET eol = ?3, content = ?5, edited = ?6
                     WHERE root = ?1 AND path = ?2 AND version = ?4",
                    (
                        root,
                        update.path,
                        eol,
                        update.version,
                        update.content,
                        update.edited,
                    ),
                )
                .await?;
            return if changed == 0 {
                Err(StoreError::NeedsOriginal)
            } else {
                Ok(())
            };
        };
        guard
            .execute(
                "INSERT INTO drafts (root, path, eol, version, original, content, edited)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (root, path) DO UPDATE SET
                    eol = excluded.eol,
                    version = excluded.version,
                    original = excluded.original,
                    content = excluded.content,
                    edited = excluded.edited",
                (
                    root,
                    update.path,
                    eol,
                    update.version,
                    original,
                    update.content,
                    update.edited,
                ),
            )
            .await?;
        Ok(())
    }

    /// Forgets the draft of one file. Having none is not an error.
    pub async fn remove_draft(&self, root: &str, path: &str) -> Result<()> {
        let mut guard = self.connection().await?;
        guard
            .execute(
                "DELETE FROM drafts WHERE root = ?1 AND path = ?2",
                (root, path),
            )
            .await?;
        Ok(())
    }

    /// Forgets every draft filed under `root`, and says how many there were.
    pub async fn remove_drafts(&self, root: &str) -> Result<u64> {
        let mut guard = self.connection().await?;
        guard
            .execute("DELETE FROM drafts WHERE root = ?1", (root,))
            .await
    }

    /// Every draft on the device, without its text.
    pub async fn draft_summary(&self) -> Result<Vec<Summary>> {
        let mut guard = self.connection().await?;
        let rows = guard
            .rows(
                "SELECT root, path, edited, length(CAST(content AS BLOB)) FROM drafts
                 ORDER BY root, path",
                (),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(Summary {
                    root: row.get(0)?,
                    path: row.get(1)?,
                    edited: row.get(2)?,
                    bytes: row.get(3)?,
                })
            })
            .collect::<turso::Result<_>>()
            .map_err(|err| self.fail(err))
    }

    /// Every setting by key, each a JSON value.
    pub async fn settings(&self) -> Result<Map<String, Value>> {
        let mut guard = self.connection().await?;
        let rows = guard
            .rows("SELECT key, value FROM settings ORDER BY key", ())
            .await?;
        let mut settings = Map::new();
        for row in rows {
            let key: String = row.get(0).map_err(|err| self.fail(err))?;
            let value: String = row.get(1).map_err(|err| self.fail(err))?;
            // A value that does not parse is skipped rather than failing the rest.
            if let Ok(value) = serde_json::from_str(&value) {
                settings.insert(key, value);
            }
        }
        Ok(settings)
    }

    /// Sets one setting, or removes it when `value` is null.
    pub async fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        let mut guard = self.connection().await?;
        if value.is_null() {
            guard
                .execute("DELETE FROM settings WHERE key = ?1", (key,))
                .await?;
        } else {
            guard
                .execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    (key, value.to_string()),
                )
                .await?;
        }
        Ok(())
    }
}

impl Store {
    /// The roots opened lately, the latest first, with when each was opened
    /// in milliseconds since the epoch.
    pub async fn recent_roots(&self) -> Result<Vec<(String, i64)>> {
        let mut guard = self.connection().await?;
        let rows = guard
            .rows(
                "SELECT path, opened FROM roots ORDER BY opened DESC LIMIT ?1",
                (RECENT_ROOTS,),
            )
            .await?;
        rows.iter()
            .map(|row| Ok((row.get(0)?, row.get(1)?)))
            .collect::<turso::Result<_>>()
            .map_err(|err| self.fail(err))
    }

    /// Notes that `root` was opened at `at`, and forgets the oldest roots
    /// beyond the few that are kept.
    pub async fn opened_root(&self, root: &str, at: i64) -> Result<()> {
        let mut guard = self.connection().await?;
        guard
            .execute(
                "INSERT INTO roots (path, opened) VALUES (?1, ?2)
                 ON CONFLICT (path) DO UPDATE SET opened = excluded.opened",
                (root, at),
            )
            .await?;
        guard
            .execute(
                "DELETE FROM roots WHERE path NOT IN
                 (SELECT path FROM roots ORDER BY opened DESC LIMIT ?1)",
                (RECENT_ROOTS,),
            )
            .await?;
        Ok(())
    }

    /// Takes `root` off the recent list. Not having it is not an error.
    pub async fn forget_root(&self, root: &str) -> Result<()> {
        let mut guard = self.connection().await?;
        guard
            .execute("DELETE FROM roots WHERE path = ?1", (root,))
            .await?;
        Ok(())
    }
}

/// The open connection, held for the length of one operation.
struct Guard<'a> {
    store: &'a Store,
    open: MutexGuard<'a, Option<(Database, Connection)>>,
}

impl Guard<'_> {
    fn connection(&self) -> &Connection {
        &self.open.as_ref().expect("opened by Store::connection").1
    }

    /// A failed statement may have left the connection in any state, so the
    /// next operation starts from a fresh one.
    fn failed(&mut self, err: turso::Error) -> StoreError {
        *self.open = None;
        self.store.fail(err)
    }

    async fn execute(&mut self, sql: &str, params: impl turso::IntoParams) -> Result<u64> {
        match self.connection().execute(sql, params).await {
            Ok(changed) => Ok(changed),
            Err(err) => Err(self.failed(err)),
        }
    }

    async fn rows(&mut self, sql: &str, params: impl turso::IntoParams) -> Result<Vec<Row>> {
        let read = async {
            let mut rows = self.connection().query(sql, params).await?;
            let mut found = Vec::new();
            while let Some(row) = rows.next().await? {
                found.push(row);
            }
            Ok(found)
        };
        match read.await {
            Ok(found) => Ok(found),
            Err(err) => Err(self.failed(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(path: &str, version: &str, original: Option<&str>, content: &str) -> Update {
        Update {
            path: path.into(),
            eol: Eol::Lf,
            version: version.into(),
            original: original.map(str::to_owned),
            content: content.into(),
            edited: Some(1_700_000_000_000),
        }
    }

    #[tokio::test]
    async fn drafts_are_filed_by_root_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("nested/data"));
        assert_eq!(store.drafts("/a").await.unwrap(), vec![]);

        store
            .put_draft("/a", update("src/x.rs", "v1", Some("one\n"), "one!\n"))
            .await
            .unwrap();
        store
            .put_draft("/b", update("src/x.rs", "v9", Some("other\n"), "other?\n"))
            .await
            .unwrap();
        let mut crlf = update("notes.txt", "v2", Some("héllo\n"), "héllo ✓\n");
        crlf.eol = Eol::Crlf;
        crlf.edited = None;
        store.put_draft("/a", crlf).await.unwrap();

        let drafts = store.drafts("/a").await.unwrap();
        assert_eq!(
            drafts,
            vec![
                Draft {
                    path: "notes.txt".into(),
                    eol: Eol::Crlf,
                    version: "v2".into(),
                    original: "héllo\n".into(),
                    content: "héllo ✓\n".into(),
                    edited: None,
                },
                Draft {
                    path: "src/x.rs".into(),
                    eol: Eol::Lf,
                    version: "v1".into(),
                    original: "one\n".into(),
                    content: "one!\n".into(),
                    edited: Some(1_700_000_000_000),
                },
            ]
        );
        assert_eq!(store.drafts("/b").await.unwrap().len(), 1);

        let summary = store.draft_summary().await.unwrap();
        assert_eq!(summary.len(), 3);
        assert_eq!(
            summary[0],
            Summary {
                root: "/a".into(),
                path: "notes.txt".into(),
                edited: None,
                bytes: "héllo ✓\n".len() as i64,
            }
        );

        store.remove_draft("/a", "src/x.rs").await.unwrap();
        store.remove_draft("/a", "src/x.rs").await.unwrap();
        assert_eq!(store.drafts("/a").await.unwrap().len(), 1);
        assert_eq!(store.remove_drafts("/a").await.unwrap(), 1);
        assert_eq!(store.drafts("/a").await.unwrap(), vec![]);
        assert_eq!(store.drafts("/b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn original_is_only_needed_once_per_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let missing = store.put_draft("/a", update("x", "v1", None, "one!")).await;
        assert!(matches!(missing, Err(StoreError::NeedsOriginal)));

        store
            .put_draft("/a", update("x", "v1", Some("one"), "one!"))
            .await
            .unwrap();
        store
            .put_draft("/a", update("x", "v1", None, "one!!"))
            .await
            .unwrap();
        let draft = store.drafts("/a").await.unwrap().remove(0);
        assert_eq!(
            (draft.original.as_str(), draft.content.as_str()),
            ("one", "one!!")
        );

        // The file moved on, so the edits have a new starting point.
        let stale = store.put_draft("/a", update("x", "v2", None, "two!")).await;
        assert!(matches!(stale, Err(StoreError::NeedsOriginal)));
        store
            .put_draft("/a", update("x", "v2", Some("two"), "two!"))
            .await
            .unwrap();
        let draft = store.drafts("/a").await.unwrap().remove(0);
        assert_eq!(
            (draft.version.as_str(), draft.original.as_str()),
            ("v2", "two")
        );
    }

    #[tokio::test]
    async fn two_stores_share_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (Store::new(dir.path()), Store::new(dir.path()));
        one.put_draft("/a", update("x", "v1", Some("one"), "one!"))
            .await
            .unwrap();
        assert_eq!(two.drafts("/a").await.unwrap().len(), 1);
        two.put_draft("/a", update("y", "v1", Some("why"), "why?"))
            .await
            .unwrap();
        one.remove_draft("/a", "x").await.unwrap();
        let paths = |drafts: Vec<Draft>| drafts.into_iter().map(|d| d.path).collect::<Vec<_>>();
        assert_eq!(paths(one.drafts("/a").await.unwrap()), ["y"]);
        assert_eq!(paths(two.drafts("/a").await.unwrap()), ["y"]);
    }

    #[tokio::test]
    async fn settings_are_kept_as_json_for_every_server() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (Store::new(dir.path()), Store::new(dir.path()));
        assert!(one.settings().await.unwrap().is_empty());
        let tweaks = serde_json::json!({ "dark": { "lineTint": 30 }, "light": {} });
        one.set_setting("tweaks", &tweaks).await.unwrap();
        one.set_setting("theme", &Value::from("dark"))
            .await
            .unwrap();
        one.set_setting("theme", &Value::from("light"))
            .await
            .unwrap();
        let seen = two.settings().await.unwrap();
        assert_eq!(seen.get("tweaks"), Some(&tweaks));
        assert_eq!(seen.get("theme"), Some(&Value::from("light")));
        two.set_setting("theme", &Value::Null).await.unwrap();
        assert_eq!(
            one.settings().await.unwrap().keys().collect::<Vec<_>>(),
            ["tweaks"]
        );
    }

    #[tokio::test]
    async fn recent_roots_are_latest_first_and_few() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.recent_roots().await.unwrap(), vec![]);
        store.opened_root("/a", 10).await.unwrap();
        store.opened_root("/b", 20).await.unwrap();
        store.opened_root("/a", 30).await.unwrap();
        assert_eq!(
            store.recent_roots().await.unwrap(),
            vec![("/a".to_owned(), 30), ("/b".to_owned(), 20)]
        );
        store.forget_root("/a").await.unwrap();
        store.forget_root("/a").await.unwrap();
        assert_eq!(store.recent_roots().await.unwrap().len(), 1);
        for at in 0..RECENT_ROOTS + 5 {
            store
                .opened_root(&format!("/r{at}"), 100 + at)
                .await
                .unwrap();
        }
        let recent = store.recent_roots().await.unwrap();
        assert_eq!(recent.len() as i64, RECENT_ROOTS);
        assert_eq!(recent[0].0, format!("/r{}", RECENT_ROOTS + 4));
    }
}
