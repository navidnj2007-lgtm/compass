//! Opening things.
//!
//! This is the tool that most easily turns into `execute_any_command`, so it is
//! built to make that impossible rather than merely discouraged.
//!
//! Two, and only two, things can happen here.
//!
//! A name from `ALLOWED_APPS` is looked up in a table compiled into the binary.
//! The executable path comes from that table, resolved under `%SystemRoot%`
//! rather than searched for on `PATH`, so a planted `notepad.exe` earlier in the
//! path cannot be substituted. No arguments are passed, ever — not the model's,
//! not the user's — so there is no command line to inject into.
//!
//! Or a path is opened with whatever Windows has registered for that file type,
//! after the guard has confirmed it is inside an allowed folder and is not an
//! executable. "Opening" a .exe is running it, which is why `Guard::openable`
//! refuses every extension on the blocked list.
//!
//! There is deliberately no shell, no `cmd /c`, no PowerShell, and no way to
//! reach one. Asking for a terminal gets a refusal with a reason, because
//! silently doing nothing teaches the model to try again a different way.

use crate::agent::{Agent, ToolOut};
use crate::consent;
use crate::guard::{show, Intent};
use crate::policy::Risk;
use serde::Deserialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

#[derive(Debug, Deserialize)]
pub struct OpenReq {
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub path: String,
}

/// name the model may use → (human label, path relative to %SystemRoot%)
///
/// Everything here is a viewer or an editor with no scripting surface. Nothing
/// here takes a command line from us.
const ALLOWED_APPS: &[(&str, &str, &str)] = &[
    ("notepad", "Notepad", r"System32\notepad.exe"),
    ("calculator", "Calculator", r"System32\calc.exe"),
    ("calc", "Calculator", r"System32\calc.exe"),
    ("paint", "Paint", r"System32\mspaint.exe"),
    ("mspaint", "Paint", r"System32\mspaint.exe"),
    ("explorer", "File Explorer", r"explorer.exe"),
    ("files", "File Explorer", r"explorer.exe"),
    (
        "snipping tool",
        "Snipping Tool",
        r"System32\SnippingTool.exe",
    ),
    (
        "snippingtool",
        "Snipping Tool",
        r"System32\SnippingTool.exe",
    ),
    ("character map", "Character Map", r"System32\charmap.exe"),
    ("magnifier", "Magnifier", r"System32\Magnify.exe"),
    (
        "on-screen keyboard",
        "On-Screen Keyboard",
        r"System32\osk.exe",
    ),
];

/// Things people ask for that will never be granted. Named explicitly so the
/// refusal can explain itself instead of looking like a missing feature.
const REFUSED_APPS: &[&str] = &[
    "cmd",
    "command prompt",
    "powershell",
    "pwsh",
    "terminal",
    "windows terminal",
    "wsl",
    "bash",
    "regedit",
    "registry editor",
    "task manager",
    "taskmgr",
    "services",
    "msconfig",
    "gpedit",
    "control panel",
    "device manager",
    "diskpart",
    "wmic",
    "cscript",
    "wscript",
    "mshta",
    "rundll32",
    "certutil",
    "bitsadmin",
    "installer",
    "winget",
    "python",
    "node",
];

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

#[derive(Debug, Deserialize)]
pub struct OpenUrlReq {
    pub url: String,
}

/// Schemes the assistant may hand to the operating system.
///
/// A web page can open a link; a desktop webview cannot, so `open.app` and
/// `open.url` — which Compass has had since before the desktop existed — route
/// through here instead of `window.open`.
///
/// The allowlist is the whole point. `opener` will happily hand the OS anything
/// it has a handler registered for, and Windows registers a great many: `file:`
/// would open local files, `ms-msdt:` and `search-ms:` have both been used for
/// remote code execution, and `javascript:` or `data:` would execute in whatever
/// opened them. So rather than blocking known-bad schemes, which is a list that
/// is never finished, only these are permitted.
const ALLOWED_SCHEMES: &[&str] = &[
    "http", "https",  // the web
    "mailto", // his mail client
    "tg", "whatsapp", "spotify", "notion", // the apps Compass already knew about
];

#[tauri::command]
pub async fn open_url(
    app: AppHandle,
    state: State<'_, Agent>,
    req: OpenUrlReq,
) -> Result<ToolOut, String> {
    let policy = state.policy();
    let confirmed = policy.needs_confirm(Risk::Medium);

    let out = async {
        let url = req.url.trim();
        if url.is_empty() || url.len() > 2000 {
            return Err("that is not a usable address".to_string());
        }
        // No control characters: a newline in a URL is how a handler gets fed a
        // second, unlogged argument.
        if url.chars().any(|c| (c as u32) < 0x20) {
            return Err("that address contained a control character".to_string());
        }

        let scheme = url
            .split_once(':')
            .map(|(s, _)| s.to_ascii_lowercase())
            .ok_or_else(|| "that address has no scheme".to_string())?;

        if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
            return Err(format!(
                "Compass will not open a {scheme}: address. Only {} are allowed.",
                ALLOWED_SCHEMES.join(", ")
            ));
        }

        consent::require(
            &app,
            &policy,
            Risk::Medium,
            "Open a link",
            &format!("The assistant wants to open:\n\n{url}"),
        )
        .await?;

        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| format!("Windows would not open that: {e}"))?;

        Ok(ToolOut::done_with(format!("Opened {url}")))
    }
    .await;

    state.audit.record(
        "open.url",
        out.is_ok(),
        format!("Open {}", req.url.chars().take(120).collect::<String>()),
        out.as_ref().err().cloned(),
        confirmed,
    );
    out
}

#[tauri::command]
pub async fn open_application(
    app: AppHandle,
    state: State<'_, Agent>,
    req: OpenReq,
) -> Result<ToolOut, String> {
    let policy = state.policy();
    let confirmed = policy.needs_confirm(Risk::Medium);

    let out = async {
        let wanted = req.app.trim().to_ascii_lowercase();

        // A path takes precedence when both are given, because "open this file"
        // is the specific request and the app name would just be a guess at how.
        if !req.path.trim().is_empty() {
            let target = state.guard().resolve(&req.path, Intent::Read)?;
            state.guard().openable(&target)?;

            consent::require(
                &app,
                &policy,
                Risk::Medium,
                "Open a file",
                &format!("The assistant wants Windows to open:\n\n{}", show(&target)),
            )
            .await?;

            app.opener()
                .open_path(target.to_string_lossy().to_string(), None::<&str>)
                .map_err(|e| format!("Windows would not open that: {e}"))?;

            return Ok(ToolOut::done_with(format!("Opened {}", show(&target))));
        }

        if wanted.is_empty() {
            return Err("nothing was named to open".into());
        }
        if REFUSED_APPS.iter().any(|r| *r == wanted) {
            return Err(format!(
                "Compass will not open {wanted}. Terminals, registry and system tools are \
                 outside what the assistant is allowed to start, by design — this is not a \
                 setting that can be changed from here."
            ));
        }

        let Some((_, label, rel)) = ALLOWED_APPS.iter().find(|(k, _, _)| *k == wanted) else {
            let known: Vec<&str> = ALLOWED_APPS.iter().map(|(k, _, _)| *k).collect();
            return Err(format!(
                "Compass can only start a short list of programs, and {wanted} is not on it. \
                 The list is: {}",
                known.join(", ")
            ));
        };

        let exe = system_root().join(rel);
        if !exe.is_file() {
            return Err(format!(
                "{label} does not seem to be installed on this computer"
            ));
        }

        consent::require(
            &app,
            &policy,
            Risk::Medium,
            "Start a program",
            &format!("The assistant wants to start {label}."),
        )
        .await?;

        // No arguments. The absence of an args list here is the security
        // property, not an omission — please do not add one.
        std::process::Command::new(&exe)
            .spawn()
            .map_err(|e| format!("could not start {label}: {e}"))?;

        Ok(ToolOut::done_with(format!("Started {label}")))
    }
    .await;

    state.audit.record(
        "win.open_application",
        out.is_ok(),
        if req.path.trim().is_empty() {
            format!("Open app {}", req.app)
        } else {
            format!("Open {}", req.path)
        },
        out.as_ref().err().cloned(),
        confirmed,
    );
    out
}
