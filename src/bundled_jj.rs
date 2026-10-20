//! The jj command line, built into projj, so that nothing has to be
//! installed for projj to run jj, and what it runs is the very version of
//! jj-lib that projj reads repositories with.
//!
//! jj's command line takes its arguments from the process and sets up
//! process-wide state (logging, the terminal), so it runs in a process of its
//! own: projj starts itself again with `AS_JJ` set, and `be_jj_if_asked`, the
//! first thing `main` does, turns that process into jj.

use std::ffi::OsString;
use std::process::Command;

/// Set in the environment of a projj process that is to be jj instead.
const AS_JJ: &str = "PROJJ_AS_JJ";

/// What `jj --version` says.
const VERSION: &str = concat!("0.43.0, built into projj ", env!("CARGO_PKG_VERSION"));

/// If this process was started to be jj, is jj, and exits with its status.
/// Call it before starting any threads.
pub fn be_jj_if_asked() {
    if std::env::var_os(AS_JJ).is_none() {
        return;
    }
    // SAFETY: called first thing, while this process has a single thread.
    unsafe { std::env::remove_var(AS_JJ) };
    let status = jj_cli::cli_util::CliRunner::init().version(VERSION).run();
    std::process::exit(status.into());
}

/// A command that runs the built-in jj with `args`.
pub fn command<I: IntoIterator<Item = S>, S: Into<OsString>>(args: I) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.env(AS_JJ, "1");
    command.args(args.into_iter().map(Into::into));
    Ok(command)
}

/// Runs the built-in jj in place of this process, as `projj vcs …` does, and
/// returns only if it cannot.
pub fn replace_process(args: Vec<OsString>) -> std::io::Error {
    let mut command = match command(args) {
        Ok(command) => command,
        Err(err) => return err,
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.exec()
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => err,
        }
    }
}
