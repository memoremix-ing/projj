//! projj as a desktop app: the review page in a window of its own, with no
//! server. Electrobun draws the window and, through CEF, the page; the
//! page's requests come through the webview's message bridge and are
//! answered here, in process, by the same `projj::api` that `projj serve`
//! puts behind HTTP. projj listens on no port, so nothing else on the
//! device, and no web page, can reach the API. (Electrobun's own loopback
//! socket is another matter; see `open_window`.)
//!
//! Each window reviews one root. File > Open Folder… opens another, and the
//! roots open at quitting are reopened at the next start.

use std::ffi::c_char;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use electrobun::BundlePaths;
use electrobun::Core;
use electrobun::MessageBoxOptions;
use electrobun::OpenFileDialogOptions;
use electrobun::Rect;
use electrobun::Renderer;
use electrobun::WebviewCallbacks;
use electrobun::WebviewOptions;
use electrobun::WindowCallbacks;
use electrobun::WindowOptions;
use projj::api;
use projj::jj;
use projj::root;
use projj::store;
use serde_json::Value;
use serde_json::json;

/// The page, as copied into the bundle by electrobun.config.ts.
const PAGE: &str = "views://projj/index.html";

/// The page may go nowhere but its own files: no website, nothing else on
/// disk. Rules are globs; the last one that matches decides, and `^` blocks.
const NAVIGATION: &str = r#"["^*", "views://projj/*"]"#;

/// The roots that were open when the app last quit, kept with projj's other
/// settings in the device store.
const OPEN_ROOTS: &str = "desktop-roots";

/// How long quitting waits for the page's last writes (unsaved editor text,
/// say) to land.
const QUIT_GRACE: Duration = Duration::from_secs(3);

/// Where to look for an installed git besides PATH, which is nearly empty
/// for an app started from the Finder or a desktop menu. Without one, projj
/// uses its built-in git; jj is always built in.
const TOOL_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/nix/var/nix/profiles/default/bin",
    "~/.cargo/bin",
    "~/.local/bin",
    "~/.nix-profile/bin",
];

struct Desktop {
    core: &'static Core,
    bundle: BundlePaths,
    packaged: bool,
    runtime: tokio::runtime::Runtime,
    data_dir: PathBuf,
    settings: store::Store,
    windows: Mutex<Vec<Window>>,
    focused: AtomicU32,
    /// Requests from pages that have not been answered yet.
    busy: AtomicUsize,
    quitting: AtomicBool,
}

struct Window {
    window_id: u32,
    webview_id: u32,
    root: PathBuf,
    api: api::Local,
}

static DESKTOP: OnceLock<Desktop> = OnceLock::new();

fn desktop() -> &'static Desktop {
    DESKTOP.get().expect("the desktop app has started")
}

fn main() {
    // The snapshot button starts this program again to be jj.
    projj::bundled_jj::be_jj_if_asked();
    if let Err(err) = run() {
        eprintln!("projj: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    find_tools();
    let core: &'static Core = Box::leak(Box::new(Core::load()?));
    let bundle = electrobun::resolve_bundle_paths()?;
    let app_info = electrobun::resolve_app_info_from_bundle(&bundle)?;
    let data_dir = std::env::var_os("PROJJ_DATA_DIR")
        .map(PathBuf::from)
        .or_else(store::default_dir)
        .ok_or("no home directory to keep settings in")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start: {err}"))?;
    let desktop = Desktop {
        core,
        bundle,
        packaged: app_info.is_packaged(),
        runtime,
        settings: store::Store::new(&data_dir),
        data_dir,
        windows: Mutex::default(),
        focused: AtomicU32::new(0),
        busy: AtomicUsize::new(0),
        quitting: AtomicBool::new(false),
    };
    DESKTOP
        .set(desktop)
        .map_err(|_| "started twice".to_string())?;
    core.set_quit_requested_handler(Some(quit_requested))?;
    // Windows are made once the event loop below is running.
    thread::spawn(start);
    thread::spawn(answer_pages);
    core.run_main_thread(&app_info)?;
    wait_for_pages();
    Ok(())
}

/// Adds the usual homes of git to PATH, after what is there already.
fn find_tools() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    for dir in TOOL_DIRS {
        let dir = match (dir.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => home.join(rest),
            (Some(_), None) => continue,
            (None, _) => PathBuf::from(dir),
        };
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        // SAFETY: no other thread has started yet.
        unsafe { std::env::set_var("PATH", path) };
    }
}

fn start() {
    let d = desktop();
    // As Electrobun's own examples do: give the event loop a moment to start.
    thread::sleep(Duration::from_millis(150));
    if let Err(err) = d
        .core
        .configure_webview_runtime_from_executable_dir(&d.bundle, 0)
    {
        complain("projj cannot start", &err);
        let _ = d.core.stop_event_loop();
        return;
    }
    if let Err(err) = d
        .core
        .set_application_menu_json(&menu(d.packaged), Some(menu_clicked))
    {
        eprintln!("projj: no menu: {err}");
    }
    let mut opened = false;
    for root in remembered_roots() {
        opened |= Path::new(&root).is_dir() && open_root(Path::new(&root));
    }
    if !opened && !choose_root() {
        let _ = d.core.stop_event_loop();
    }
}

fn menu(packaged: bool) -> String {
    let mut view = vec![
        json!({ "label": "Reload", "action": "reload", "accelerator": "r" }),
        json!({ "role": "toggleFullScreen" }),
    ];
    if !packaged {
        view.push(json!({ "label": "Developer Tools", "action": "devtools", "accelerator": "i" }));
    }
    json!([
        { "label": "projj", "submenu": [
            { "role": "about" },
            { "type": "separator" },
            { "role": "hide" }, { "role": "hideOthers" }, { "role": "showAll" },
            { "type": "separator" },
            { "role": "quit" },
        ]},
        { "label": "File", "submenu": [
            { "label": "Open Folder…", "action": "open-folder", "accelerator": "o" },
            { "type": "separator" },
            { "role": "close" },
        ]},
        // Without these, copy and paste do nothing in the page.
        { "label": "Edit", "submenu": [
            { "role": "undo" }, { "role": "redo" },
            { "type": "separator" },
            { "role": "cut" }, { "role": "copy" }, { "role": "paste" }, { "role": "selectAll" },
        ]},
        { "label": "View", "submenu": view },
        { "label": "Window", "submenu": [{ "role": "minimize" }, { "role": "zoom" }] },
    ])
    .to_string()
}

extern "C" fn menu_clicked(_: u32, action: *const c_char) {
    let d = desktop();
    let focused = d.focused.load(Ordering::Acquire);
    let webview = d
        .windows
        .lock()
        .unwrap()
        .iter()
        .find(|window| window.window_id == focused)
        .map(|window| window.webview_id);
    match electrobun::c_string_to_string(action).as_str() {
        // Off the main thread: the dialog waits for the person.
        "open-folder" => drop(thread::spawn(choose_root)),
        "reload" => {
            if let Some(webview) = webview {
                let _ = d.core.reload_webview(webview);
            }
        }
        "devtools" => {
            if let Some(webview) = webview {
                let _ = d.core.toggle_webview_devtools(webview);
            }
        }
        _ => {}
    }
}

/// Asks for a folder and opens the root it is in. False if none was opened.
fn choose_root() -> bool {
    let d = desktop();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let picked = d.core.open_file_dialog(OpenFileDialogOptions {
        starting_folder: &home,
        allowed_file_types: "*",
        can_choose_files: false,
        can_choose_directory: true,
        allows_multiple_selection: false,
    });
    let picked = match picked {
        Ok(picked) if !picked.trim().is_empty() => picked,
        Ok(_) => return false,
        Err(err) => {
            complain("The folder could not be chosen", &err);
            return false;
        }
    };
    match serde_json::from_str::<Vec<String>>(&picked) {
        Ok(paths) => paths.first().is_some_and(|path| open_root(Path::new(path))),
        Err(err) => {
            complain("The folder could not be chosen", &err.to_string());
            false
        }
    }
}

/// Opens a window on the root containing `path`, or brings forward the one
/// already open on it. False, after telling the person why, if there is none.
fn open_root(path: &Path) -> bool {
    let d = desktop();
    let root = match root::find_root(path) {
        Ok(root) => root,
        Err(err) => {
            complain(
                "That folder is not in a projj root",
                &format!(
                    "{err:#}\n\nChoose the folder of a git repository that has a track/ folder, \
                     or any folder inside one."
                ),
            );
            return false;
        }
    };
    let open = d
        .windows
        .lock()
        .unwrap()
        .iter()
        .find(|window| window.root == root)
        .map(|window| window.window_id);
    if let Some(window_id) = open {
        let _ = d.core.show_window(window_id, true);
        return true;
    }
    match open_window(&root) {
        Ok(()) => {
            remember_roots();
            true
        }
        Err(err) => {
            complain(&format!("{} could not be opened", root.display()), &err);
            false
        }
    }
}

fn open_window(root: &Path) -> Result<(), String> {
    let d = desktop();
    let jj = jj::Jj::load().map_err(|err| format!("{err:#}"))?;
    let app = api::App::new(root.to_owned(), jj, None, store::Store::new(&d.data_dir));
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let title = format!("{name} — projj");
    let frame = Rect::new(120.0, 80.0, 1360.0, 900.0);
    let mut options = WindowOptions::new(&title, frame);
    options.centered = true;
    options.callbacks = WindowCallbacks {
        close: Some(window_closed),
        focus: Some(window_focused),
        ..WindowCallbacks::default()
    };
    let window_id = d.core.create_window(options)?;

    // Electrobun's core also listens on a loopback socket that pages may
    // send through instead of the native bridge (projj's page never does).
    // What arrives there is only taken if sealed with this window's key, so
    // the key must be one nothing else can know.
    let key = secret_key();
    let mut web = WebviewOptions::new(
        window_id,
        PAGE,
        Rect::new(0.0, 0.0, frame.width, frame.height),
    );
    web.renderer = Renderer::Cef;
    web.partition = "persist:default";
    web.secret_key = &key;
    // The page needs the message bridge, which sandboxed pages lack. It is
    // projj's own page and can go nowhere else (NAVIGATION).
    web.sandbox = false;
    web.callbacks = WebviewCallbacks {
        decide_navigation: Some(electrobun::allow_all_navigation),
        event: Some(electrobun::noop_webview_event),
        event_bridge: Some(electrobun::noop_webview_post_message),
        ..WebviewCallbacks::default()
    };
    let webview_id = match d.core.create_webview(web) {
        Ok(id) => id,
        Err(err) => {
            let _ = d.core.close_window(window_id);
            return Err(err);
        }
    };
    if let Err(err) = d.core.set_webview_navigation_rules(webview_id, NAVIGATION) {
        let _ = d.core.close_window(window_id);
        return Err(err);
    }
    d.windows.lock().unwrap().push(Window {
        window_id,
        webview_id,
        root: root.to_owned(),
        api: api::Local::new(Arc::new(app)),
    });
    d.focused.store(window_id, Ordering::Release);
    Ok(())
}

fn secret_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system has no randomness to give");
    bytes.map(|byte| byte.to_string()).join(",")
}

extern "C" fn window_focused(window_id: u32) {
    desktop().focused.store(window_id, Ordering::Release);
}

extern "C" fn window_closed(window_id: u32) {
    let d = desktop();
    let mut windows = d.windows.lock().unwrap();
    windows.retain(|window| window.window_id != window_id);
    // Closing the last window quits, and its root is to be reopened.
    let remember = !windows.is_empty() && !d.quitting.load(Ordering::Acquire);
    drop(windows);
    if remember {
        remember_roots();
    }
}

/// How long the queue of page messages may sit unread: briefly while pages
/// are busy, longer once they have gone quiet.
const POLL_BUSY: Duration = Duration::from_millis(2);
const POLL_IDLE: Duration = Duration::from_millis(25);
const QUIET_AFTER: Duration = Duration::from_secs(1);

/// Electrobun's core files every message from a page in one queue, to be
/// collected here.
fn answer_pages() {
    let d = desktop();
    let mut last = Instant::now();
    loop {
        match d.core.pop_next_queued_host_message_string() {
            Some((webview_id, message)) => {
                answer(webview_id, &message);
                last = Instant::now();
            }
            None if last.elapsed() < QUIET_AFTER => thread::sleep(POLL_BUSY),
            None => thread::sleep(POLL_IDLE),
        }
    }
}

/// A page's request: `{ type: "projj", id, method, path, body }`, answered
/// with `{ type: "projj", id, status, body }`. See `desktopBridge` in app.js.
fn answer(webview_id: u32, message: &str) {
    let d = desktop();
    let Ok(message) = serde_json::from_str::<Value>(message) else {
        return;
    };
    if message["type"] != "projj" {
        return;
    }
    let Some(api) = d
        .windows
        .lock()
        .unwrap()
        .iter()
        .find(|window| window.webview_id == webview_id)
        .map(|window| window.api.clone())
    else {
        return;
    };
    let id = message["id"].clone();
    let method = message["method"].as_str().unwrap_or("GET").to_owned();
    let path = message["path"].as_str().unwrap_or_default().to_owned();
    let body = message["body"].as_str().unwrap_or_default().to_owned();
    d.busy.fetch_add(1, Ordering::AcqRel);
    d.runtime.spawn(async move {
        let (status, body) = if path.starts_with("/api/") {
            api.call(&method, &path, body).await
        } else {
            (404, json!({ "error": "not an API path" }).to_string())
        };
        let reply = json!({ "type": "projj", "id": id, "status": status, "body": body });
        let d = desktop();
        if let Err(err) = d
            .core
            .send_host_message_to_webview_json(webview_id, &reply.to_string())
        {
            // The window closed while the request was under way.
            eprintln!("projj: cannot answer the page: {err}");
        }
        d.busy.fetch_sub(1, Ordering::AcqRel);
    });
}

/// Cmd+Q, closing the last window, or a signal: let every page hand over
/// what it has not saved, then stop.
extern "C" fn quit_requested() {
    let d = desktop();
    if d.quitting.swap(true, Ordering::AcqRel) {
        return;
    }
    let webviews: Vec<u32> = d
        .windows
        .lock()
        .unwrap()
        .iter()
        .map(|window| window.webview_id)
        .collect();
    for webview_id in webviews {
        let _ = d
            .core
            .send_host_message_to_webview_json(webview_id, r#"{"type":"projj-flush"}"#);
    }
    // Off the main thread, which the pages' answers may need.
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(200));
        wait_for_pages();
        let _ = desktop().core.stop_event_loop();
    });
}

fn wait_for_pages() {
    let d = desktop();
    let since = Instant::now();
    while d.busy.load(Ordering::Acquire) > 0 && since.elapsed() < QUIT_GRACE {
        thread::sleep(Duration::from_millis(20));
    }
}

fn remembered_roots() -> Vec<String> {
    let d = desktop();
    match d.runtime.block_on(d.settings.settings()) {
        Ok(settings) => settings
            .get(OPEN_ROOTS)
            .and_then(Value::as_array)
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(|root| root.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        Err(err) => {
            eprintln!("projj: cannot read settings: {err:#}");
            Vec::new()
        }
    }
}

fn remember_roots() {
    let d = desktop();
    let roots: Vec<String> = d
        .windows
        .lock()
        .unwrap()
        .iter()
        .map(|window| window.root.display().to_string())
        .collect();
    d.busy.fetch_add(1, Ordering::AcqRel);
    d.runtime.spawn(async move {
        let d = desktop();
        if let Err(err) = d.settings.set_setting(OPEN_ROOTS, &json!(roots)).await {
            eprintln!("projj: cannot keep the open folders: {err:#}");
        }
        d.busy.fetch_sub(1, Ordering::AcqRel);
    });
}

fn complain(message: &str, detail: &str) {
    eprintln!("projj: {message}: {detail}");
    let _ = desktop().core.show_message_box(MessageBoxOptions {
        box_type: "warning",
        title: "projj",
        message,
        detail,
        buttons: &["OK"],
        default_id: 0,
        cancel_id: 0,
    });
}
