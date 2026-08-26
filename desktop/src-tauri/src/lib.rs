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
mod grant;
mod guard;
mod policy;
mod rules;
mod shell;
mod tools;

use agent::Agent;
use audit::{Audit, Entry};
use policy::Policy;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

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

/* ── computer control ────────────────────────────────────────────────
Three commands, and the shape of them is the design. Granting is something only a
person does, from a control in the chat header — there is deliberately no tool the
model can call to grant itself anything, which is why these are not in the tool
registry and why the model is never told they exist.

Revoking, by contrast, is something everything can do: the countdown, the panic key,
losing focus, ending the conversation. That asymmetry is the whole safety property. */

/// Turn on computer control for a stretch of minutes.
#[tauri::command]
async fn pc_grant(
    state: State<'_, Agent>,
    seconds: Option<u64>,
) -> Result<grant::GrantState, String> {
    let s = state.grants.grant(seconds.unwrap_or(grant::GRANT_SECS));
    state.audit.record(
        "pc.grant",
        true,
        format!(
            "Computer control switched on for {} seconds",
            s.seconds_left
        ),
        None,
        false,
    );
    Ok(s)
}

/// Turn it off. Called by the header control, by the panic key, and when a
/// conversation ends.
#[tauri::command]
async fn pc_revoke(
    state: State<'_, Agent>,
    why: Option<String>,
) -> Result<grant::GrantState, String> {
    let why = why.unwrap_or_else(|| "you switched it off".into());
    state.grants.revoke(&why);
    state.audit.record(
        "pc.revoke",
        true,
        format!("Computer control off: {why}"),
        None,
        false,
    );
    Ok(state.grants.state())
}

/// How much is left. Polled by the header so the countdown is Rust's answer rather
/// than a timer the page runs for itself — a page-side timer would keep counting
/// after the grant had actually expired.
#[tauri::command]
async fn pc_grant_state(state: State<'_, Agent>) -> Result<grant::GrantState, String> {
    Ok(state.grants.state())
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

/// The panic key.
///
/// Ctrl+Alt+Shift+Esc revokes any computer-control grant, aborts what the agent is
/// doing, and says so with a native notification. Registered globally so it works
/// while another application has focus, which is the situation it exists for — if
/// Compass is driving Word, the keyboard is pointed at Word.
///
/// TWO HONEST LIMITS, both worth stating where the code is rather than only in the UI.
///
/// A global hotkey can fail to register, because another process may already own the
/// combination. If it does, this returns an error and the frontend refuses to hand out
/// a grant at all: an escape hatch nobody has tested is worse than no escape hatch,
/// because it changes how carefully people behave.
///
/// And a `RegisterHotKey`-style shortcut does not fire while a secure desktop has
/// focus — a UAC prompt, the Ctrl+Alt+Del screen. Synthetic input cannot reach those
/// either, so there is nothing to stop in that moment, but it means this is an escape
/// hatch for the ordinary desktop and not a universal kill switch.
const PANIC_KEY: &str = "ctrl+alt+shift+Escape";

fn arm_panic_key(app: &AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    let shortcut = Shortcut::new(
        Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
        Code::Escape,
    );

    app.global_shortcut()
        .on_shortcut(shortcut, move |app, _sc, event| {
            // Only on press. Firing on release as well would revoke twice and log
            // twice for one keystroke.
            if event.state() != ShortcutState::Pressed {
                return;
            }
            if let Some(agent) = app.try_state::<Agent>() {
                let was_active = agent.grants.state().active;
                agent.grants.revoke("the panic key stopped it");
                agent.audit.record(
                    "pc.panic",
                    true,
                    "Panic key pressed — computer control revoked".into(),
                    None,
                    false,
                );
                // Tell the frontend, so the loop stops and the indicator goes away
                // without waiting for the next poll.
                let _ = app.emit("compass://panic", was_active);
            }
            use tauri_plugin_notification::NotificationExt;
            let _ = app
                .notification()
                .builder()
                .title("Compass stopped")
                .body("Computer control is off. Nothing else will be clicked or typed.")
                .show();
        })
        .map_err(|e| format!("could not register {PANIC_KEY}: {e}"))
}

/// Is the panic key armed? The frontend asks before offering a grant.
#[tauri::command]
async fn pc_panic_armed(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    Ok(app.global_shortcut().is_registered(PANIC_KEY))
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
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            agent_handshake,
            agent_audit,
            agent_open_settings,
            pc_grant,
            pc_revoke,
            pc_grant_state,
            pc_panic_armed,
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

            // The escape hatch is armed before anything can need it. A failure is
            // recorded rather than fatal; the frontend refuses to hand out a grant
            // when it is not armed.
            if let Err(e) = arm_panic_key(&handle) {
                eprintln!("panic key not registered: {e}");
            }

            // Track whether anyone is looking. A grant survives a brief loss of focus
            // — a click Compass performs moves focus to the target window — but not a
            // sustained one, because the visible indicator only protects someone who
            // is there to see it.
            if let Some(win) = app.get_webview_window("main") {
                let h2 = handle.clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::Focused(focused) = ev {
                        if let Some(agent) = h2.try_state::<Agent>() {
                            agent.grants.set_focus(*focused);
                        }
                    }
                });
            }

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
