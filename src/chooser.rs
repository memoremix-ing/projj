//! The operating system's own dialog for choosing a folder, for pages served
//! by `projj serve`. A browser can let someone pick a folder, but never says
//! where it is, so the server asks for one itself; it runs on the same
//! device, for the person at it. The desktop app brings its own dialog.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

/// Asks for a folder, starting in the one given. `None` if the person
/// cancelled. Blocks until they have answered.
pub type Chooser = Arc<dyn Fn(&Path) -> Result<Option<PathBuf>> + Send + Sync>;

const PROMPT: &str = "Choose a folder to review, or one to start a new root in";

/// The dialog of this platform, if it has one projj knows how to show.
/// `PROJJ_FOLDER_CHOOSER` names a program to run instead: it gets the
/// starting folder as its argument and prints the chosen one, or nothing.
pub fn system() -> Option<Chooser> {
    if let Some(program) = std::env::var_os("PROJJ_FOLDER_CHOOSER").filter(|p| !p.is_empty()) {
        return Some(Arc::new(move |start: &Path| {
            let mut command = Command::new(&program);
            command.arg(start);
            run(command, &[])
        }));
    }
    if cfg!(target_os = "macos") {
        return Some(Arc::new(|start: &Path| {
            let mut command = Command::new("osascript");
            // Passed as arguments, not spliced into the script, so no folder
            // name can change what runs.
            for line in [
                "on run argv",
                // Brings the dialog in front of the browser.
                "activate",
                "POSIX path of (choose folder with prompt (item 1 of argv) default location (POSIX file (item 2 of argv)))",
                "end run",
            ] {
                command.args(["-e", line]);
            }
            command.arg(PROMPT).arg(start);
            // -128 is "User canceled".
            run(command, &["(-128)"])
        }));
    }
    if cfg!(windows) {
        return Some(Arc::new(|start: &Path| {
            let mut command = Command::new("powershell");
            command
                .args(["-NoProfile", "-STA", "-Command"])
                .arg(
                    "Add-Type -AssemblyName System.Windows.Forms; \
                     $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
                     $d.Description = $env:PROJJ_PROMPT; $d.SelectedPath = $env:PROJJ_START; \
                     if ($d.ShowDialog() -eq 'OK') { $d.SelectedPath }",
                )
                .env("PROJJ_PROMPT", PROMPT)
                .env("PROJJ_START", start);
            run(command, &[])
        }));
    }
    let found = |program: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    if found("zenity") {
        return Some(Arc::new(|start: &Path| {
            let mut command = Command::new("zenity");
            command
                .args(["--file-selection", "--directory"])
                .arg(format!("--title={PROMPT}"))
                .arg(format!("--filename={}/", start.display()));
            run(command, &[])
        }));
    }
    if found("kdialog") {
        return Some(Arc::new(|start: &Path| {
            let mut command = Command::new("kdialog");
            command
                .arg("--title")
                .arg(PROMPT)
                .arg("--getexistingdirectory")
                .arg(start);
            run(command, &[])
        }));
    }
    None
}

/// Runs a dialog. It prints the folder; failing without one of `cancelled`
/// in what it says, or saying nothing at all, is an error.
fn run(mut command: Command, cancelled: &[&str]) -> Result<Option<PathBuf>> {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot show a folder chooser")?;
    let chosen = String::from_utf8_lossy(&output.stdout);
    let chosen = chosen.trim_end_matches(['\n', '\r']);
    if output.status.success() {
        return Ok((!chosen.is_empty()).then(|| PathBuf::from(chosen)));
    }
    let said = String::from_utf8_lossy(&output.stderr);
    // zenity and kdialog say nothing when cancelled.
    if said.trim().is_empty() || cancelled.iter().any(|sign| said.contains(sign)) {
        return Ok(None);
    }
    bail!("the folder chooser failed: {}", said.trim())
}
