use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use projj::api;
use projj::gate;
use projj::jj;
use projj::root;
use projj::store;
use tokio::io::AsyncBufReadExt as _;

/// Tools for "projj-style" roots: one git repository for human review on top
/// of jj repositories that agents work in.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the local code review web app.
    Serve {
        /// Any path inside the root to review. Defaults to the current
        /// directory; git is asked which repository contains it. Outside
        /// any repository, the page helps choose a root, or start one.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Port to listen on; 0 picks a free one.
        #[arg(long, default_value_t = 4747)]
        port: u16,
        /// Serve web assets from this directory instead of the embedded copy.
        #[arg(long, env = "PROJJ_WEB_DIR")]
        web_dir: Option<PathBuf>,
        #[command(flatten)]
        data: DataDir,
    },
    /// List the repositories and workspaces tracked under a root.
    List {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Run jj: the version built into projj (0.43.0), which reads and
    /// writes repositories exactly as projj does.
    #[command(disable_help_flag = true)]
    Jj {
        /// Passed to jj as they are; `projj jj --help` is jj's help.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<OsString>,
    },
    /// List the unsaved editor text kept on this device, for every root.
    Drafts {
        /// Throw away what is kept for this root instead. Takes the root's
        /// path as listed, which need not exist any more.
        #[arg(long, value_name = "ROOT")]
        discard: Option<String>,
        #[command(flatten)]
        data: DataDir,
    },
}

#[derive(clap::Args)]
struct DataDir {
    /// Directory of the store that every projj server on this device shares.
    /// Defaults to the per-user data directory of the platform.
    #[arg(long, env = "PROJJ_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

impl DataDir {
    fn store(self) -> Result<store::Store> {
        let dir = self
            .data_dir
            .or_else(store::default_dir)
            .context("no home directory to keep unsaved edits and settings in; pass --data-dir")?;
        Ok(store::Store::new(&dir))
    }
}

fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    let start = match root {
        Some(root) => root,
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    root::find_root(&start)
}

fn main() -> Result<()> {
    // Before anything else: this may be a process started to be jj.
    projj::bundled_jj::be_jj_if_asked();
    let command = Cli::parse().command;
    if let Command::Jj { args } = command {
        let err = projj::bundled_jj::replace_process(args);
        return Err(err).context("cannot run the built-in jj");
    }
    tokio::runtime::Runtime::new()
        .context("cannot start")?
        .block_on(run(command))
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::List { root } => {
            let root = resolve_root(root)?;
            let mut listing = format!("{}\n", root.display());
            for repo in root::discover(&root)? {
                listing += &format!("  {}\n", repo.id);
                if repo.detached {
                    listing += &format!("    repository kept in {}\n", repo.path);
                }
                for workspace in repo.workspaces {
                    listing += &format!("    {}\n", workspace.path);
                    if let Some(problem) = workspace.problem {
                        listing += &format!("      cannot be opened: {problem}\n");
                    }
                }
            }
            print(&listing);
            Ok(())
        }
        Command::Serve {
            root,
            port,
            web_dir,
            data,
        } => serve(root, port, web_dir, data.store()?).await,
        Command::Drafts { discard, data } => drafts(data.store()?, discard).await,
        Command::Jj { .. } => unreachable!("handled before the runtime starts"),
    }
}

/// A reader that stops early (`| head`) is not an error.
fn print(text: &str) {
    let _ = std::io::stdout().write_all(text.as_bytes());
}

async fn drafts(store: store::Store, discard: Option<String>) -> Result<()> {
    if let Some(root) = discard {
        // A root that still exists may be named any way that reaches it.
        let root = store::root_key(std::path::Path::new(&root));
        let count = store.remove_drafts(&root).await?;
        print(&format!(
            "discarded unsaved edits to {count} file(s) in {root}\n"
        ));
        return Ok(());
    }
    let mut listing = format!("{}\n", store.file().display());
    let mut root = None;
    for draft in store.draft_summary().await? {
        if root.as_ref() != Some(&draft.root) {
            let gone = !std::path::Path::new(&draft.root).is_dir();
            listing += &format!(
                "  {}{}\n",
                draft.root,
                if gone { " (no longer there)" } else { "" }
            );
            root = Some(draft.root);
        }
        let edited = draft
            .edited
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|at| {
                at.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "at an unknown time".into());
        listing += &format!(
            "    {}  edited {edited}, {} bytes\n",
            draft.path, draft.bytes
        );
    }
    if root.is_none() {
        listing += "  nothing unsaved\n";
    }
    print(&listing);
    Ok(())
}

/// The root to open first, and where projj was started if that is in no
/// repository. A `--root` that leads to no repository is a mistake to stop
/// at; the current directory leading nowhere just means choosing one.
fn starting_point(root: Option<PathBuf>) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    if let Some(asked) = root {
        return match root::resolve(&asked)? {
            Some(resolved) => Ok((Some(resolved.root), None)),
            None => anyhow::bail!("{} is in no git repository", asked.display()),
        };
    }
    let here = std::env::current_dir().context("cannot read the current directory")?;
    Ok(match root::resolve(&here)? {
        Some(resolved) => (Some(resolved.root), None),
        None => (None, Some(here)),
    })
}

async fn serve(
    root: Option<PathBuf>,
    port: u16,
    web_dir: Option<PathBuf>,
    store: store::Store,
) -> Result<()> {
    let (root, start_dir) = starting_point(root)?;
    // Loopback only: this server stages changes on behalf of whoever asks.
    let requested = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = tokio::net::TcpListener::bind(requested)
        .await
        .with_context(|| format!("cannot listen on {requested}"))?;
    let addr = listener.local_addr()?;
    let app = api::App::new(jj::Jj::load()?, web_dir, store)
        .with_default_root(root)
        .with_start_dir(start_dir)
        .with_chooser(projj::chooser::system());
    let app = Arc::new(app);
    let gate = Arc::new(gate::Gate::new(addr));
    let git = projj::Engine::detect().describe();
    match &app.default_root {
        Some(root) => println!("projj: reviewing {}, with {git}", root.display()),
        None => {
            println!("projj: not in a git repository; the page will help choose a root, with {git}")
        }
    }
    println!(
        "projj: unsaved edits and settings are kept in {}",
        app.store.file().display()
    );
    println!("projj: open {}", gate.link());
    if std::io::stdin().is_terminal() {
        println!("projj: that link lets one browser in, once; press Enter here for another");
        let gate = gate.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(_)) = lines.next_line().await {
                println!("projj: open {}", gate.link());
            }
        });
    } else {
        println!("projj: that link lets one browser in, once");
    }
    let router = api::router(app).layer(axum::middleware::from_fn_with_state(gate, gate::guard));
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
