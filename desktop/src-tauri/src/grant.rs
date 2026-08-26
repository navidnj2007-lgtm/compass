//! Permission to drive the machine, and the windows that are never touched.
//!
//! Everything in `screen.rs` and `input.rs` is gated on a grant that lives here. The
//! grant exists because computer use is categorically different from every other tool
//! in this program: a file tool can only reach the folders in the policy, but
//! synthetic mouse and keyboard input can do anything the person sitting at the
//! keyboard could do, to any window, including applications Compass has no other
//! route to. There is no sandbox for that. The only available control is *when*, and
//! that is what this module is.
//!
//! WHY A SESSION GRANT RATHER THAN A PERMISSION
//!
//! A permission is a thing you grant once and forget. This is deliberately not one.
//! It is off at startup, it is turned on for a stretch of minutes with a visible
//! countdown, and it turns itself off again — on expiry, when the window loses focus
//! for long enough that nobody is watching, when the panic key is pressed, or when the
//! conversation ends. It is never written to disk, so it cannot survive a restart, and
//! there is no "don't ask again", because a persistent grant is precisely what an
//! injected prompt would try to obtain early and spend later.
//!
//! The step counter matters as much as the clock. A grant is for a job, and a job has
//! a size; a run that has issued four hundred clicks is not doing the job that was
//! approved, whatever the clock says.
//!
//! WHAT THE EXCLUSION LIST IS AND IS NOT
//!
//! Some windows are never captured and never clicked: Windows Security, UAC consent,
//! the credential manager, password managers. The list is compiled in, with a
//! user-extendable addition in the policy file, and it is checked on the window's
//! title and class.
//!
//! It must be said plainly that this is defence in depth and not the mechanism that
//! makes UAC safe. A UAC prompt runs on a separate, secure desktop: `SendInput` from
//! this process cannot reach it and a screen capture of it comes back black. That is
//! Windows doing the work, not this file. The list matters for the things that are
//! *not* protected that way — a password manager's unlock window is an ordinary
//! window on the ordinary desktop, and without this list it would be as clickable and
//! as screenshotable as anything else.

use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a grant lasts if nothing revokes it sooner.
pub const GRANT_SECS: u64 = 15 * 60;

/// The longest a grant may be asked for. A hand-edited request for eight hours is
/// not a decision, it is a way to make the grant permanent by another name.
pub const MAX_GRANT_SECS: u64 = 30 * 60;

/// Synthetic input events one grant may issue. A job has a size; four hundred clicks
/// is not the job that was approved.
pub const MAX_GRANT_STEPS: u32 = 300;

/// Events per minute. Nothing a person asks for needs more, and a runaway loop is
/// the thing this bounds.
///
/// Allowed dead for now, and the allow is deliberately narrow rather than blanket:
/// the only caller of `claim()` is synthetic input, which is the next task. The grant
/// machinery landed first on purpose — it is the part that has to be right before
/// anything can move the mouse, and shipping it separately means it could be tested
/// on its own rather than alongside the thing it restrains.
#[allow(dead_code)]
pub const MAX_EVENTS_PER_MIN: u32 = 90;

/// How long the app may be in the background before the grant is dropped. Short,
/// because the entire point of the visible indicator is that he is watching; if he
/// has gone to another application for a minute, he is not.
pub const BLUR_REVOKE_SECS: u64 = 60;

/// Window titles and classes that are never captured, never clicked and never typed
/// into. Substring match, lowercased, on both title and class.
///
/// Compiled in and not configurable — the policy may only *add*. A list that can be
/// narrowed at runtime is a list an injected prompt can ask to have narrowed.
#[allow(dead_code)] // used by the pc.* tools, which land next
pub const BLOCKED_WINDOWS: &[&str] = &[
    // Windows' own credential and security surfaces.
    "windows security",
    "windows defender",
    "credential manager",
    "sign in to your account",
    "user account control",
    "consent.exe",
    "credentialuibroker",
    "windows hello",
    "smartscreen",
    // The secure-desktop classes, listed for completeness. SendInput cannot reach
    // these anyway; see the module comment.
    "#32770 - security",
    "uacsettingschangedialog",
    // Password managers, by the names their windows actually use.
    "1password",
    "bitwarden",
    "keepass",
    "lastpass",
    "dashlane",
    "nordpass",
    "enpass",
    "keeper password",
    "proton pass",
    // Browser windows that are specifically a credential prompt rather than a page.
    "sign in - google accounts",
    "microsoft account",
    // The banking and authenticator apps most likely to be open on a student's PC.
    "authenticator",
    "mitid",
    "nemid",
];

/// Is this window off limits?
///
/// Checked against both title and class because neither alone is reliable: a password
/// manager may have a generic title on a distinctive class, and a credential dialog
/// may have a generic class with a distinctive title.
#[allow(dead_code)] // ditto: the matcher is tested on its own before it gates anything
pub fn is_blocked(title: &str, class: &str, extra: &[String]) -> bool {
    let t = title.to_ascii_lowercase();
    let c = class.to_ascii_lowercase();
    for pat in BLOCKED_WINDOWS {
        if t.contains(pat) || c.contains(pat) {
            return true;
        }
    }
    for pat in extra {
        let p = pat.trim().to_ascii_lowercase();
        // An empty or one-character pattern would match nearly everything, which
        // would look like the feature being broken rather than being strict.
        if p.len() < 2 {
            continue;
        }
        if t.contains(&p) || c.contains(&p) {
            return true;
        }
    }
    false
}

/// What the frontend is told about the grant.
#[derive(Debug, Clone, Serialize)]
pub struct GrantState {
    pub active: bool,
    /// Seconds remaining, zero when inactive.
    pub seconds_left: u64,
    pub steps_used: u32,
    pub steps_left: u32,
    /// Why it is not active, when it is not. A sentence, for the UI to show.
    pub reason: String,
}

struct Grant {
    until: Instant,
    steps: u32,
    /// Rolling window for the rate limit: when it started and how many since.
    minute_start: Instant,
    minute_count: u32,
    blurred_at: Option<Instant>,
}

pub struct Grants {
    inner: Mutex<Option<Grant>>,
    /// Why the last grant ended, so the UI can say "the panic key stopped it" rather
    /// than silently showing an off switch.
    last_reason: Mutex<String>,
}

impl Default for Grants {
    fn default() -> Self {
        Self::new()
    }
}

impl Grants {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            last_reason: Mutex::new(String::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Grant>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a grant. Returns the state so the caller cannot assume what it got.
    pub fn grant(&self, seconds: u64) -> GrantState {
        let secs = seconds.clamp(60, MAX_GRANT_SECS);
        let now = Instant::now();
        *self.lock() = Some(Grant {
            until: now + Duration::from_secs(secs),
            steps: 0,
            minute_start: now,
            minute_count: 0,
            blurred_at: None,
        });
        self.set_reason("");
        self.state()
    }

    /// End it. `why` is shown to the user, so it says who ended it.
    pub fn revoke(&self, why: &str) {
        *self.lock() = None;
        self.set_reason(why);
    }

    fn set_reason(&self, why: &str) {
        if let Ok(mut r) = self.last_reason.lock() {
            *r = why.to_string();
        }
    }

    fn reason(&self) -> String {
        self.last_reason
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// The window gained or lost focus. Losing it does not revoke immediately — a
    /// click that Compass itself performs moves focus to the target window, so an
    /// instant revoke would make the feature revoke itself on first use. It revokes
    /// if focus stays away.
    pub fn set_focus(&self, focused: bool) {
        let mut g = self.lock();
        if let Some(grant) = g.as_mut() {
            grant.blurred_at = if focused { None } else { Some(Instant::now()) };
        }
    }

    /// Is a grant live right now? Expiry and blur are evaluated here rather than on a
    /// timer, so there is no window in which a stale grant is still usable because a
    /// timer had not fired yet.
    pub fn active(&self) -> bool {
        let mut g = self.lock();
        let Some(grant) = g.as_ref() else {
            return false;
        };

        if Instant::now() >= grant.until {
            *g = None;
            drop(g);
            self.set_reason("the 15 minutes ran out");
            return false;
        }
        if let Some(since) = grant.blurred_at {
            if since.elapsed() >= Duration::from_secs(BLUR_REVOKE_SECS) {
                *g = None;
                drop(g);
                self.set_reason("Compass was in the background, so it stopped");
                return false;
            }
        }
        if grant.steps >= MAX_GRANT_STEPS {
            *g = None;
            drop(g);
            self.set_reason("it used all the actions one session allows");
            return false;
        }
        true
    }

    /// Claim one synthetic input event. Fails closed, and the error is written to be
    /// shown to the user and read by the model.
    #[allow(dead_code)] // the only caller is synthetic input, which lands next
    pub fn claim(&self) -> Result<(), String> {
        if !self.active() {
            let why = self.reason();
            return Err(if why.is_empty() {
                "computer control is not switched on. Turn it on in the chat header first, and it \
                 stays on for 15 minutes."
                    .into()
            } else {
                format!("computer control is not switched on \u{2014} {why}.")
            });
        }

        let mut g = self.lock();
        let Some(grant) = g.as_mut() else {
            return Err("computer control is not switched on".into());
        };

        // Rolling minute.
        if grant.minute_start.elapsed() >= Duration::from_secs(60) {
            grant.minute_start = Instant::now();
            grant.minute_count = 0;
        }
        if grant.minute_count >= MAX_EVENTS_PER_MIN {
            return Err(format!(
                "that is more than {MAX_EVENTS_PER_MIN} actions in a minute, which is faster than \
                 any real task needs, so Compass stopped."
            ));
        }

        grant.minute_count += 1;
        grant.steps += 1;
        Ok(())
    }

    pub fn state(&self) -> GrantState {
        let live = self.active();
        let g = self.lock();
        match g.as_ref() {
            Some(grant) if live => GrantState {
                active: true,
                seconds_left: grant
                    .until
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
                steps_used: grant.steps,
                steps_left: MAX_GRANT_STEPS.saturating_sub(grant.steps),
                reason: String::new(),
            },
            _ => GrantState {
                active: false,
                seconds_left: 0,
                steps_used: 0,
                steps_left: 0,
                reason: self.reason(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_granted_to_begin_with() {
        let g = Grants::new();
        assert!(!g.active());
        assert!(g.claim().is_err());
        assert!(!g.state().active);
    }

    #[test]
    fn a_grant_lets_input_through_and_counts_it() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        assert!(g.active());
        assert!(g.claim().is_ok());
        assert!(g.claim().is_ok());
        assert_eq!(g.state().steps_used, 2);
    }

    #[test]
    fn revoking_takes_effect_at_once_and_says_why() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        g.revoke("the panic key stopped it");
        assert!(!g.active());
        let err = g.claim().unwrap_err();
        assert!(err.contains("panic key"), "{err}");
    }

    #[test]
    fn an_expired_grant_is_dead_without_waiting_for_a_timer() {
        let g = Grants::new();
        // One second, then wound back past its end by construction.
        g.grant(60);
        {
            let mut inner = g.lock();
            let grant = inner.as_mut().unwrap();
            grant.until = Instant::now() - Duration::from_secs(1);
        }
        assert!(!g.active(), "an expired grant must not be usable");
        assert!(g.claim().is_err());
        assert!(g.state().reason.contains("ran out"), "{:?}", g.state());
    }

    #[test]
    fn losing_focus_briefly_does_not_revoke() {
        // It must not: a click Compass performs moves focus to the target window, so
        // an instant revoke would make the feature revoke itself on first use.
        let g = Grants::new();
        g.grant(GRANT_SECS);
        g.set_focus(false);
        assert!(g.active());
        assert!(g.claim().is_ok());
    }

    #[test]
    fn losing_focus_for_long_enough_does_revoke() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        g.set_focus(false);
        {
            let mut inner = g.lock();
            let grant = inner.as_mut().unwrap();
            grant.blurred_at = Some(Instant::now() - Duration::from_secs(BLUR_REVOKE_SECS + 1));
        }
        assert!(!g.active());
        assert!(g.state().reason.contains("background"), "{:?}", g.state());
    }

    #[test]
    fn regaining_focus_clears_the_blur() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        g.set_focus(false);
        g.set_focus(true);
        {
            let inner = g.lock();
            assert!(inner.as_ref().unwrap().blurred_at.is_none());
        }
        assert!(g.active());
    }

    #[test]
    fn the_step_budget_ends_the_grant() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        {
            let mut inner = g.lock();
            inner.as_mut().unwrap().steps = MAX_GRANT_STEPS;
        }
        assert!(!g.active());
        assert!(g.claim().is_err());
        assert!(
            g.state().reason.contains("all the actions"),
            "{:?}",
            g.state()
        );
    }

    #[test]
    fn the_rate_limit_refuses_without_ending_the_grant() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        for _ in 0..MAX_EVENTS_PER_MIN {
            assert!(g.claim().is_ok());
        }
        let err = g.claim().unwrap_err();
        assert!(err.contains("in a minute"), "{err}");
        // Still granted: a burst is not a betrayal, it is a loop that needs slowing.
        assert!(g.active());
    }

    #[test]
    fn the_rate_window_rolls() {
        let g = Grants::new();
        g.grant(GRANT_SECS);
        for _ in 0..MAX_EVENTS_PER_MIN {
            let _ = g.claim();
        }
        {
            let mut inner = g.lock();
            let grant = inner.as_mut().unwrap();
            grant.minute_start = Instant::now() - Duration::from_secs(61);
        }
        assert!(g.claim().is_ok(), "the window should have rolled");
    }

    #[test]
    fn a_grant_cannot_be_asked_for_longer_than_the_ceiling() {
        let g = Grants::new();
        let s = g.grant(60 * 60 * 8);
        assert!(s.seconds_left <= MAX_GRANT_SECS, "{}", s.seconds_left);
    }

    #[test]
    fn a_grant_cannot_be_asked_for_absurdly_short_either() {
        let g = Grants::new();
        let s = g.grant(0);
        assert!(s.seconds_left >= 55, "{}", s.seconds_left);
    }

    /* ── the exclusion matcher ───────────────────────────────────── */

    #[test]
    fn credential_surfaces_are_blocked_by_title() {
        for t in [
            "Windows Security",
            "windows security",
            "User Account Control",
            "Credential Manager",
            "1Password 8",
            "Bitwarden - Vault",
            "KeePassXC - Passwords.kdbx",
            "Microsoft Authenticator",
            "MitID",
        ] {
            assert!(is_blocked(t, "", &[]), "{t} should be blocked");
        }
    }

    #[test]
    fn a_class_alone_is_enough_to_block() {
        assert!(is_blocked("", "CredentialUIBroker", &[]));
        assert!(is_blocked("Untitled", "consent.exe", &[]));
    }

    #[test]
    fn ordinary_windows_are_not_blocked() {
        for t in [
            "Chemistry notes.docx - Word",
            "Downloads",
            "Compass",
            "Untitled - Notepad",
            "Inbox - Outlook",
            // Near misses that must NOT trip: the words appear but not the pattern.
            "How Windows security works.pdf - Adobe",
        ] {
            // The last one deliberately does contain "windows security" as a phrase,
            // so it IS blocked. That is the intended trade: a false refusal costs a
            // sentence, a false allow costs a password.
            let blocked = is_blocked(t, "", &[]);
            if t.starts_with("How Windows security") {
                assert!(blocked, "a document about security still trips the filter");
            } else {
                assert!(!blocked, "{t} should not be blocked");
            }
        }
    }

    #[test]
    fn the_policy_can_add_but_the_test_shows_it_cannot_narrow() {
        let extra = vec!["my bank".to_string()];
        assert!(is_blocked("My Bank - Online", "", &extra));
        // And the compiled list still applies whatever the policy says.
        assert!(is_blocked("Windows Security", "", &extra));
    }

    #[test]
    fn a_too_short_extra_pattern_is_ignored() {
        // "a" would match nearly every window, which would look like the feature
        // being broken rather than being strict.
        let extra = vec!["a".to_string(), "".to_string(), " ".to_string()];
        assert!(!is_blocked("Downloads", "", &extra));
    }

    #[test]
    fn matching_is_case_insensitive_both_ways() {
        assert!(is_blocked("BITWARDEN", "", &[]));
        assert!(is_blocked("bitwarden", "", &[]));
        let extra = vec!["MyBank".to_string()];
        assert!(is_blocked("mybank ltd", "", &extra));
    }
}
