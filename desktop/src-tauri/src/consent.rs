//! Asking the user, in a way the thing that wants permission cannot fake.
//!
//! Compass already shows an approval card in the chat before any write runs, and
//! for most actions that is the right prompt: it is in context, it lists the
//! whole job, and it has an Undo. But it is drawn by the web page. If the page
//! were compromised, the card could say "create a note" while the IPC call
//! underneath said "delete two hundred files", and the user would have approved
//! the wrong thing in good faith.
//!
//! So destructive actions get a second prompt, drawn by Windows, from text built
//! here out of the arguments Rust is actually about to act on. The frontend
//! cannot style it, suppress it, pre-click it, or change what it says.
//!
//! Two rules make it worth having. It fails closed: anything other than an
//! explicit yes — a timeout, a closed dialog, a channel that broke — is a no.
//! And it is never remembered: there is no "don't ask again", because a
//! persistent grant is exactly what an injected prompt would try to obtain
//! early and spend later. Nothing in this module writes a decision anywhere, so
//! there is no cache for a later version to be tempted into consulting.
//!
//! THE TWO ENTRY POINTS, and why one of them takes no policy
//!
//! `require` asks the policy whether this tier needs a dialog. That question is
//! only ever interesting for `Medium`, where a prompt on every small edit would
//! train someone to click through prompts; `High` and `Critical` answer yes
//! unconditionally inside `Policy::needs_confirm`.
//!
//! `require_always` does not take a `Policy` at all. That is not tidiness — it is
//! the point. A function that accepts a policy can be called with a policy that
//! says no, and the next person to add a tool has to notice which tier they
//! passed. Synthetic mouse and keyboard input goes through this one, so there is
//! no argument at the call site, and no field in any file, that could turn the
//! dialog off. The type signature is the guarantee.

use crate::policy::{Policy, Risk};
use std::sync::mpsc;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// Long enough to read a list of files and think about it; short enough that a
/// dialog nobody is sitting in front of does not pin a worker thread forever.
const DECISION_TIMEOUT: Duration = Duration::from_secs(300);

/// Ask Windows to confirm, unless the policy says this tier does not need it.
///
/// `title` is the action in a few words. `detail` should say exactly what will
/// happen and to how many things, because this is the last point at which the
/// user can still say no.
///
/// Note which way the delegation runs: this is a thin wrapper that may decline to
/// ask, and `require_always` is the part that does the asking. A tool that must
/// always prompt calls that one directly and never passes through here.
pub async fn require(
    app: &AppHandle,
    policy: &Policy,
    risk: Risk,
    title: &str,
    detail: &str,
) -> Result<(), String> {
    if !policy.needs_confirm(risk) {
        return Ok(());
    }
    require_always(app, title, detail).await
}

/// Ask Windows to confirm, with no way to opt out.
///
/// There is no `Policy` parameter and no risk tier, so there is nothing to pass
/// that skips the dialog. Used for synthetic mouse and keyboard input, where the
/// action is indistinguishable from the user doing it himself and the dialog is
/// the only thing that makes the difference visible.
pub async fn require_always(app: &AppHandle, title: &str, detail: &str) -> Result<(), String> {
    if ask(app, title, detail).await {
        Ok(())
    } else {
        Err("you didn't allow that, so nothing was changed".into())
    }
}

/// The dialog itself. Returns true only on an explicit yes.
///
/// Every other outcome is false: the user chose "Don't allow", the dialog was
/// closed, nobody was at the machine and it timed out, or the channel carrying
/// the answer broke. Failing closed on a broken channel matters more than it
/// looks — a panicking dialog thread would otherwise be indistinguishable from
/// consent.
async fn ask(app: &AppHandle, title: &str, detail: &str) -> bool {
    let app = app.clone();
    let title = title.to_string();
    let detail = detail.to_string();

    // The dialog callback lands on another thread and we need to wait for it.
    // Waiting happens on a blocking-pool thread so the async runtime and the UI
    // thread both stay free — a dialog that blocked the UI thread could not be
    // clicked, which would deadlock the very prompt we are waiting on.
    tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = mpsc::channel::<bool>();
        app.dialog()
            .message(&detail)
            .title(format!("Compass — {title}"))
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Allow".to_string(),
                "Don't allow".to_string(),
            ))
            .show(move |ok| {
                let _ = tx.send(ok);
            });

        rx.recv_timeout(DECISION_TIMEOUT).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// Render a list of paths for a confirmation dialog: enough to recognise the
/// files, capped so the dialog stays readable, and honest about the remainder.
pub fn summarise(paths: &[std::path::PathBuf], limit: usize) -> String {
    let shown: Vec<String> = paths
        .iter()
        .take(limit)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();

    let mut out = shown.join("\n");
    if paths.len() > shown.len() {
        out.push_str(&format!("\n…and {} more", paths.len() - shown.len()));
    }
    out
}
