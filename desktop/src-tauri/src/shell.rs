//! Deciding which Compass to show.
//!
//! This is the file that makes "push to GitHub and the desktop app updates" true.
//! The window loads the live GitHub Pages URL — the same bytes the browser gets —
//! so an ordinary HTML, CSS or JavaScript change reaches the desktop with no
//! rebuild, no installer and no release. The native layer below is the only part
//! that needs a new build when it changes.
//!
//! When the network is not there, it falls back to a copy of the frontend
//! bundled inside the installed application.
//!
//! WHY THE FALLBACK IS BUNDLED AND NOT CACHED
//!
//! The obvious design is to save each successful remote load into the app's data
//! directory and serve that when offline. It would even keep the offline copy
//! current. It is also a privilege escalation, and it took noticing once to rule
//! out permanently: this app grants the frontend access to native file tools, and
//! those tools can write files. A frontend cached in a user-writable directory is
//! a frontend the agent could rewrite — at which point the next launch runs
//! attacker-authored JavaScript with the whole native tool surface behind it.
//!
//! So the offline copy lives in the install directory, which a standard user
//! cannot write without elevation, and the path guard denies that directory to
//! every file tool regardless. The cost is that the offline copy is only as new
//! as the last installer. That is the right trade: the offline path is the rare
//! one, and a stale planner is a much smaller problem than an executable
//! frontend.

use std::sync::atomic::{AtomicUsize, Ordering};

use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

/// The live frontend. This is the source of truth for the UI on every platform.
///
/// The `/compass/` path is not a guess any more. Checked directly: the bare
/// origin `https://navidnj2007-lgtm.github.io/` returns 404, and
/// `https://navidnj2007-lgtm.github.io/compass/` serves the app. So Pages is
/// publishing a project site, not a user site, and this constant is correct as
/// written. If that ever changes the symptom is unmistakable — every launch
/// falls back to the bundled snapshot, because the probe below only checks that
/// the host answers on 443 and a 404 page answers just as cheerfully as the real
/// one.
///
/// It can still be set at build time without editing this file:
///
/// ```text
/// $env:COMPASS_REMOTE_URL = "https://navidnj2007-lgtm.github.io/"
/// npm run build
/// ```
///
/// The capability in `capabilities/remote.json` grants IPC by *origin* with a
/// wildcard path, so it already covers this path and any other path on the same
/// host — but it must be updated if the origin itself ever changes.
pub const REMOTE_URL: &str = match option_env!("COMPASS_REMOTE_URL") {
    Some(u) => u,
    None => "https://navidnj2007-lgtm.github.io/compass/",
};

/// How long to wait before deciding the network is not there. Short, because
/// this delay sits in front of the window appearing, and a slow start looks like
/// a broken app.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The live frontend from GitHub Pages.
    Live,
    /// The copy that shipped with this installer.
    Bundled,
}

/// The host part of `REMOTE_URL`, worked out at runtime from the constant.
///
/// Hand-parsed rather than pulled from a URL crate: this is one known-good
/// compile-time string, not untrusted input, and the alternative was carrying a
/// parser to read a value we already know.
fn remote_host() -> &'static str {
    let s = match REMOTE_URL.split_once("://") {
        Some((_, rest)) => rest,
        None => REMOTE_URL,
    };
    match s.split_once('/') {
        Some((host, _)) => host,
        None => s,
    }
}

/// Can we reach the live frontend right now?
///
/// A DNS lookup plus a TCP connect, using nothing but `std`. It deliberately does
/// not make an HTTPS request: doing so would mean carrying a TLS stack — `rustls`
/// and its C and assembly dependencies — into a security-sensitive binary purely
/// to answer "is the network up", and it would not even buy the thing it looks
/// like it buys. A captive portal completes the TLS handshake and returns a
/// cheerful 200 for its own login page, so an HTTP probe would call that "live"
/// too unless it inspected the body.
///
/// So the honest boundary is drawn here: this answers "is there a network", and
/// nothing more. On a captive portal the user gets the portal page in the window,
/// which is visible, recoverable, and no worse than any browser.
pub fn probe() -> Source {
    use std::net::{TcpStream, ToSocketAddrs};

    let host = remote_host();
    let Ok(addrs) = (host, 443u16).to_socket_addrs() else {
        return Source::Bundled;
    };

    for addr in addrs {
        if TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok() {
            return Source::Live;
        }
    }
    Source::Bundled
}

/// Build the one window the application has.
pub fn open_main(app: &AppHandle, source: Source) -> tauri::Result<()> {
    let url = match source {
        Source::Live => WebviewUrl::External(
            REMOTE_URL
                .parse()
                .expect("REMOTE_URL is a compile-time constant and must parse"),
        ),
        Source::Bundled => WebviewUrl::App("index.html".into()),
    };

    let popup_app = app.clone();
    let nav_app = app.clone();
    let win = WebviewWindowBuilder::new(app, "main", url)
        .additional_browser_args(BROWSER_ARGS)
        .on_new_window(move |url, features| new_window(&popup_app, url, features))
        .on_navigation(move |url| {
            // A link to spotify:, tg:, mailto: and so on is a hand-off to another
            // app, never a page this window should try to show.
            if is_app_scheme(url.scheme()) {
                let _ = nav_app.opener().open_url(url.as_str(), None::<&str>);
                return false;
            }
            true
        })
        .title("Compass")
        .inner_size(1180.0, 820.0)
        .min_inner_size(420.0, 560.0)
        .resizable(true)
        .center()
        .visible(true)
        // The frontend is a document-shaped app, not a game: let Windows draw the
        // frame so it behaves like every other window on the machine.
        .decorations(true)
        .build()?;

    if source == Source::Bundled {
        // Say so, once, in the app's own voice rather than a dialog. The user
        // needs to know the planner is a snapshot; they do not need a modal about
        // it before they can start typing.
        let _ = win.eval(
            r#"
            (function(){
              function note(){
                if(document.getElementById("offlineNote")) return;
                var d=document.createElement("div");
                d.id="offlineNote";
                d.textContent="Offline — showing the copy that came with the app. Reconnect and reopen Compass for the latest version.";
                d.setAttribute("style","position:fixed;left:0;right:0;bottom:0;z-index:99999;"+
                  "padding:9px 14px;font:12.5px/1.45 system-ui,-apple-system,'Segoe UI',sans-serif;"+
                  "text-align:center;background:#3a3a38;color:#f4f4f1");
                document.body.appendChild(d);
              }
              if(document.readyState==="loading") document.addEventListener("DOMContentLoaded",note);
              else note();
            })();
            "#,
        );
    }

    let _ = app;
    Ok(())
}

/// WebView2 flags for every Compass window.
///
/// The first part repeats wry's defaults, which setting any flags replaces. The
/// autoplay flag lets Compass's own play button start the Spotify embed: without
/// it WebView2 wants a click *inside* the embed's frame first, and the focus
/// player's play button silently does nothing.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
--autoplay-policy=no-user-gesture-required";

/// Schemes that belong to another application. The same list `open_url`
/// accepts, minus the web: handing one of these to Windows is what a browser does
/// with the same link.
fn is_app_scheme(scheme: &str) -> bool {
    matches!(scheme, "spotify" | "tg" | "whatsapp" | "notion" | "mailto")
}

/// Pages that open *inside* Compass, in a small window of their own.
///
/// Only logins whose cookies the main window needs. The Spotify player in the
/// focus page is Spotify's embed, and it plays whole songs only when the
/// webview itself is logged in to Spotify; a login in his normal browser keeps
/// its cookies there. So this window shares the main window's WebView2
/// environment, and with it the cookie jar.
fn opens_inside(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("accounts.spotify.com")
}

static POPUPS: AtomicUsize = AtomicUsize::new(0);

/// `window.open` from any Compass window.
///
/// Version 1.0 had no handler here, and without one WebView2 refuses every new
/// window silently, which is why the Spotify login button did nothing. Now:
///   * a Spotify login opens in a Compass window that shares cookies;
///   * any other web address opens in his default browser, as a link would;
///   * spotify:, tg:, mailto: and friends go to their own apps;
///   * anything else (file:, javascript:, data:) is refused.
///
/// The login window is labelled `login-N`, and the capability files grant IPC to
/// the window labelled `main` only, so a page loaded in it cannot reach any
/// native command.
fn new_window(
    app: &AppHandle,
    url: Url,
    features: tauri::webview::NewWindowFeatures,
) -> NewWindowResponse<tauri::Wry> {
    if opens_inside(&url) {
        let label = format!("login-{}", POPUPS.fetch_add(1, Ordering::Relaxed));
        let blank: Url = "about:blank".parse().expect("about:blank parses");
        let open_app = app.clone();
        let built = WebviewWindowBuilder::new(app, label, WebviewUrl::External(blank))
            .window_features(features)
            .additional_browser_args(BROWSER_ARGS)
            .title("Log in to Spotify")
            .inner_size(480.0, 720.0)
            .center()
            .resizable(true)
            // Links out of the login page (help, sign-up) go to his browser.
            .on_new_window(move |u, _| {
                open_outside(&open_app, &u);
                NewWindowResponse::Deny
            })
            // The login finishes on open.spotify.com. Once it gets there the cookie
            // is set, so the window has done its job; Compass sees it close and
            // rebuilds the player.
            .on_page_load(|w, p| {
                if p.event() == PageLoadEvent::Finished
                    && p.url().host_str() == Some("open.spotify.com")
                {
                    let _ = w.close();
                }
            })
            .build();
        return match built {
            Ok(window) => NewWindowResponse::Create { window },
            Err(_) => NewWindowResponse::Deny,
        };
    }
    open_outside(app, &url);
    NewWindowResponse::Deny
}

/// Hand an address to Windows if it is one a browser would also hand over.
fn open_outside(app: &AppHandle, url: &Url) {
    let s = url.scheme();
    if s == "http" || s == "https" || is_app_scheme(s) {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
    }
}
