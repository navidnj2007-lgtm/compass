//! Compass for Windows — the native shell and the agent's tool layer.
//!
//! What this program is: a real window, an installer, and a set of narrow,
//! audited capabilities that the Compass assistant can ask to use.
//!
//! What it deliberately is not: a second Compass. It ships no UI of its own. The
//! interface is the same `index.html` the browser loads, fetched live, so the web
//! and desktop versions cannot drift apart. See `shell.rs`.
//!
//! The security posture in one paragraph: the frontend is treated as untrusted.
//! It is remote by design, Tauri has had both an iframe IPC bypass and an origin
//! confusion bug, and the assistant driving it takes instructions from text that
//! may itself be hostile. So the ACL in `capabilities/` is defence in depth, and
//! the actual enforcement is in `guard.rs` (where the path is), `policy.rs` (what
//! is permitted at all), `consent.rs` (a dialog the page cannot draw) and
//! `audit.rs` (what happened). Every tool goes through all four.

mod agent;
mod audit;
mod consent;
mod guard;
mod policy;
mod rules;
mod shell;
mod tools;

use agent::Agent;
use audit::{Audit, Entry};
use policy::Policy;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

/// What the frontend is told at startup.
///
/// The roots and caps are reported rather than assumed, so the prompt the model
/// reads always describes the sandbox that is actually in force. If the user
/// narrows his allowed folders, the model is told about the narrower set on the
/// next turn without anyone having to remember to update a string.
#[derive(Debug, Serialize)]
pub struct Handshake {
    version: String,
    platform: String,
    home: String,
    roots: Vec<String>,
    writable: Vec<String>,
    max_read_chars: usize,
    max_results: usize,
    max_batch: usize,
    confirm_high: bool,
    auto_low: bool,
}

#[tauri::command]
async fn agent_handshake(app: AppHandle, state: State<'_, Agent>) -> Result<Handshake, String> {
    // Pick up an edit to the policy file without needing a restart.
    state.reload(&app);
    let pol = state.policy();
    let roots: Vec<String> = pol.roots.iter().map(|r| r.display().to_string()).collect();

    Ok(Handshake {
        version: app.package_info().version.to_string(),
        platform: "windows".into(),
        home: app
            .path()
            .home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_default(),
        writable: roots.clone(),
        roots,
        max_read_chars: pol.max_read_chars,
        max_results: pol.max_results,
        max_batch: pol.max_batch,
        // Not read from the policy, because the policy no longer decides it. The
        // destructive tier always confirms, so this is the constant `true` rather
        // than a field the frontend could see as false and quietly believe.
        confirm_high: true,
        auto_low: true,
    })
}

#[tauri::command]
async fn agent_audit(state: State<'_, Agent>, limit: Option<usize>) -> Result<Vec<Entry>, String> {
    Ok(state.audit.recent(limit.unwrap_or(40)))
}

/// Open the policy file so the user can widen or narrow what the agent may do.
///
/// A text file rather than a settings screen, on purpose: the set of folders the
/// assistant may touch is a decision worth making deliberately and being able to
/// read back later, and a JSON file with comments in the docs is easier to audit
/// than a row of toggles. It opens in whatever the user's default editor is; the
/// next handshake picks up the change.
#[tauri::command]
async fn agent_open_settings(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let f = Policy::policy_path(&app).ok_or("there is no settings file yet")?;
    if !f.exists() {
        // Make sure there is something to open on a first run.
        let pol = Policy::load(&app);
        pol.save(&app)?;
    }
    app.opener()
        .open_path(f.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| format!("could not open the settings file: {e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // A second launch focuses the window that already exists rather than
        // opening a rival one with its own view of the same synced state.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
                let _ = w.unminimize();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            agent_handshake,
            agent_audit,
            agent_open_settings,
            tools::files::list_files,
            tools::files::search_files,
            tools::files::grep_files,
            tools::docs::read_document,
            tools::files::diff_file,
            tools::files::restore_file,
            tools::files::read_file,
            tools::files::create_folder,
            tools::files::write_file,
            tools::files::move_file,
            tools::files::rename_file,
            tools::files::delete_file,
            tools::system::get_system_information,
            tools::clip::clipboard_read,
            tools::clip::clipboard_write,
            tools::clip::show_notification,
            tools::apps::open_application,
            tools::apps::open_url,
            tools::browser::browser_tabs,
            tools::browser::browser_open,
            tools::browser::browser_read,
            tools::browser::browser_click,
            tools::browser::browser_type,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Policy first: everything else is derived from it.
            let pol = Policy::load(&handle);
            let denied = Policy::hard_denied(&handle);
            let home = handle.path().home_dir().ok();
            let log_dir = handle.path().app_config_dir().ok();

            app.manage(Agent::new(pol, denied, home, Audit::new(log_dir)));

            // Decide live-or-bundled before the window exists, so the user never
            // sees a flash of the wrong one. The probe is off the main thread
            // because it does network I/O.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                let source = tauri::async_runtime::spawn_blocking(shell::probe)
                    .await
                    .unwrap_or(shell::Source::Bundled);

                if let Err(e) = shell::open_main(&h, source) {
                    eprintln!("could not open the Compass window: {e}");
                }

                // Native updates, checked quietly after the UI is up. This is a
                // separate channel from frontend updates entirely: it only fires
                // when the Rust layer itself has changed.
                check_for_updates(h).await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Compass");
}

/// Ask GitHub Releases whether there is a newer native build, and offer it.
///
/// Failures are silent by design. A missing `latest.json`, no network, or an
/// unsigned artefact all mean "no update today", and none of them is worth
/// interrupting someone's morning planning with an error box.
async fn check_for_updates(app: AppHandle) {
    use tauri_plugin_updater::UpdaterExt;

    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };

    // The signature is verified against the public key compiled into the app
    // before a single byte is executed; that check lives inside the plugin.
    let mut downloaded = 0usize;
    let ok = update
        .download_and_install(
            |chunk, _total| {
                downloaded += chunk;
            },
            || {},
        )
        .await;

    if let Err(e) = ok {
        eprintln!("update failed: {e}");
    }
}
