//! What the agent is allowed to do, and how much of it.
//!
//! Two kinds of rule live here and the split is deliberate.
//!
//! The tunable kind — which folders, how many files, how big — is written to a
//! JSON file the user owns and can edit. Widening it is his decision to make.
//!
//! The structural kind — that executables are never written, that credentials
//! are never read, that the app's own files are off limits — is compiled in and
//! has no setting. A configuration file is a thing that gets talked into being
//! edited, and "delete the line that stops you writing .exe files" is exactly
//! the kind of request an injected prompt would make. If it cannot be changed at
//! runtime, that conversation cannot happen.
//!
//! One rule has crossed that line since the first version, and it is worth
//! recording why. `confirm_high` used to be a tunable: set it to false and the
//! Windows confirmation for destructive actions went away. But that dialog is
//! the single check a compromised frontend cannot draw, suppress or pre-click,
//! which makes "set confirm_high to false" the most valuable sentence an injected
//! prompt could get someone to act on — and it was a one-line edit to a text
//! file, with no second opinion anywhere. So it moved into the structural set:
//! `needs_confirm` no longer reads the field, `clamp` writes it back as true so
//! the file cannot claim otherwise, and the field survives only so that policy
//! files written by older versions still parse. Synthetic input arrived later at
//! its own tier, `Critical`, which has never had a switch to begin with.

use crate::rules::canonical;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// How much a tool can cost the user if it turns out to be wrong.
///
/// `Low` is recoverable by ignoring it. `Medium` changes something he would
/// notice. `High` loses or relocates data. `Critical` drives the machine
/// itself — synthetic mouse and keyboard input, which can do anything a person
/// sitting at the keyboard could do, to any window, including ones Compass has
/// no other route to.
///
/// The two top tiers always earn a dialog drawn by Windows rather than by the
/// web page asking for it, and neither can be switched off. See `needs_confirm`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

// The hard, unconfigurable rules live in rules.rs, which depends on nothing but
// std so it can be compiled and tested on its own. Only the tunable numbers —
// the ones it is reasonable for the user to change — are here.

fn default_max_read_chars() -> usize {
    20_000
}
fn default_max_write_chars() -> usize {
    200_000
}
fn default_max_results() -> usize {
    200
}
fn default_max_batch() -> usize {
    100
}
fn default_max_file_bytes() -> u64 {
    8 * 1024 * 1024
}
fn default_max_walk_entries() -> usize {
    60_000
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// The only folders any file tool will look at. Note what is missing: the
    /// home folder itself, so dotfiles and AppData are out of reach; and the
    /// install directory, so the agent cannot rewrite the app that runs it.
    pub roots: Vec<PathBuf>,

    /// Characters returned to the model from one `read_file`.
    #[serde(default = "default_max_read_chars")]
    pub max_read_chars: usize,

    /// Characters accepted from the model in one `write_file`.
    #[serde(default = "default_max_write_chars")]
    pub max_write_chars: usize,

    /// Rows returned by one listing or search.
    #[serde(default = "default_max_results")]
    pub max_results: usize,

    /// Files touched by one move or delete. A bulk operation the user did not
    /// expect is bounded even if he approves it without reading.
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,

    /// Largest file that may be opened for reading at all.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,

    /// Directory entries a single search may visit before giving up, so a search
    /// of a deep tree cannot hang the assistant.
    #[serde(default = "default_max_walk_entries")]
    pub max_walk_entries: usize,

    /// Retained so policy files written by older versions still parse, and
    /// ignored everywhere else.
    ///
    /// This used to switch off the Windows confirmation for destructive actions.
    /// It was a mistake, and the reason is the whole threat model of this app in
    /// one line: "set confirm_high to false" is exactly the edit an injected
    /// prompt would try to talk someone into making, and it disabled the only
    /// prompt a compromised frontend cannot draw, suppress or pre-click. A rule
    /// that can be turned off by editing one line of JSON is not a defence, it is
    /// a suggestion — so this moved out of the tunables and into the structural
    /// set alongside "never write .exe" and "never read id_rsa".
    ///
    /// `clamp` forces it back to `true`, so the file on disk never shows a value
    /// that is not being honoured. The cost is real and was accepted knowingly:
    /// there is no longer any way to silence the dialog on a file move or delete.
    /// If that becomes tiresome the fix is fewer and better-batched destructive
    /// proposals, not a switch.
    #[serde(default = "default_true")]
    pub confirm_high: bool,

    /// Ask Windows to confirm merely consequential ones too. Off by default:
    /// every write already carries an in-app approval card, and a second prompt
    /// on every small edit trains people to click through prompts.
    #[serde(default)]
    pub confirm_medium: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_read_chars: default_max_read_chars(),
            max_write_chars: default_max_write_chars(),
            max_results: default_max_results(),
            max_batch: default_max_batch(),
            max_file_bytes: default_max_file_bytes(),
            max_walk_entries: default_max_walk_entries(),
            confirm_high: default_true(),
            confirm_medium: false,
        }
    }
}

impl Policy {
    /// The folders a person actually keeps documents in — and nothing else.
    ///
    /// These come from the OS known-folder API rather than being built from the
    /// username, so a machine where Documents and Desktop are redirected into
    /// OneDrive gets the real locations instead of empty stubs that would make
    /// every tool mysteriously refuse.
    pub fn default_roots(app: &AppHandle) -> Vec<PathBuf> {
        let p = app.path();
        [
            p.download_dir(),
            p.document_dir(),
            p.desktop_dir(),
            p.picture_dir(),
            p.audio_dir(),
            p.video_dir(),
        ]
        .into_iter()
        .flatten()
        .filter(|d| d.is_dir())
        .filter_map(|d| canonical(&d).ok())
        .fold(Vec::new(), |mut acc, d| {
            if !acc.contains(&d) {
                acc.push(d);
            }
            acc
        })
    }

    /// Places that are never allowed even if they somehow sit inside a root:
    /// the installed program, and the app's own data and configuration.
    ///
    /// The config directory matters more than it looks. The audit log lives
    /// there, and so does this policy file. An agent that could write to it
    /// could grant itself the whole disk and erase the record of having done so.
    pub fn hard_denied(app: &AppHandle) -> Vec<PathBuf> {
        let p = app.path();
        let mut out: Vec<PathBuf> = Vec::new();
        let mut add = |d: Option<PathBuf>| {
            if let Some(d) = d {
                let c = canonical(&d).unwrap_or(d);
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        };
        add(p.app_config_dir().ok());
        add(p.app_data_dir().ok());
        add(p.app_local_data_dir().ok());
        add(p.app_cache_dir().ok());
        add(p.app_log_dir().ok());
        add(std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf)));
        out
    }

    fn file(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("no config directory: {e}"))?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        Ok(dir.join("agent-policy.json"))
    }

    /// Read the saved policy, or write out the defaults on first run.
    ///
    /// A corrupt or hand-mangled file falls back to defaults rather than
    /// refusing to start, but it never falls back to something *wider* than the
    /// defaults, because every field it cannot parse takes the default value.
    pub fn load(app: &AppHandle) -> Self {
        let mut pol = match Self::file(app)
            .and_then(|f| std::fs::read_to_string(&f).map_err(|e| e.to_string()))
        {
            Ok(text) => serde_json::from_str::<Policy>(&text).unwrap_or_default(),
            Err(_) => Policy::default(),
        };

        if pol.roots.is_empty() {
            pol.roots = Self::default_roots(app);
        }
        pol.clamp();
        let _ = pol.save(app);
        pol
    }

    /// Keep hand-edited numbers inside sane bounds. Someone who sets
    /// `max_batch` to four billion has not made a decision, they have made a
    /// typo, and the agent should not act on it.
    pub fn clamp(&mut self) {
        self.max_read_chars = self.max_read_chars.clamp(500, 400_000);
        self.max_write_chars = self.max_write_chars.clamp(1, 2_000_000);
        self.max_results = self.max_results.clamp(10, 5_000);
        self.max_batch = self.max_batch.clamp(1, 1_000);
        self.max_file_bytes = self.max_file_bytes.clamp(1024, 256 * 1024 * 1024);
        self.max_walk_entries = self.max_walk_entries.clamp(1_000, 2_000_000);

        // Not a clamp so much as a correction. `confirm_high` is no longer
        // consulted by anything, so a file left saying `false` would describe a
        // protection that is in force as though it were switched off. Writing it
        // back as `true` means the next person to open the file — which is the
        // documented way to inspect this policy — reads what is actually
        // happening rather than a leftover from a previous version.
        self.confirm_high = true;

        // Roots must exist, must be directories, and are stored canonical so the
        // path guard can compare them without doing it again per call.
        let mut seen: Vec<PathBuf> = Vec::new();
        for r in std::mem::take(&mut self.roots) {
            if let Ok(c) = canonical(&r) {
                if c.is_dir() && !seen.contains(&c) {
                    seen.push(c);
                }
            }
        }
        self.roots = seen;
    }

    pub fn save(&self, app: &AppHandle) -> Result<(), String> {
        let f = Self::file(app)?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&f, text).map_err(|e| format!("could not write {}: {e}", f.display()))
    }

    /// Does this risk level need Windows to ask?
    ///
    /// Only the `Medium` answer comes from this struct. `High` and `Critical`
    /// return true unconditionally, and deliberately read no field at all: there
    /// is no value anyone could put in the policy file, and nothing the frontend
    /// could pass, that removes the dialog in front of losing data or driving the
    /// keyboard. `Low` never asks, because a folder listing behind a prompt is how
    /// people learn to click through prompts.
    pub fn needs_confirm(&self, risk: Risk) -> bool {
        match risk {
            Risk::Low => false,
            Risk::Medium => self.confirm_medium,
            Risk::High | Risk::Critical => true,
        }
    }

    pub fn policy_path(app: &AppHandle) -> Option<PathBuf> {
        Self::file(app).ok()
    }
}

/// These check the rules that have no setting, which is the only reason they are
/// worth writing: a test that a configurable value is configurable tells you
/// nothing, whereas a test that an *un*configurable one has stayed that way is
/// what catches someone helpfully making it settable again.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination of the two fields a person can actually edit, so nothing
    /// below can pass by accident on one lucky default.
    fn permutations() -> Vec<Policy> {
        let mut out = Vec::new();
        for confirm_high in [true, false] {
            for confirm_medium in [true, false] {
                out.push(Policy {
                    confirm_high,
                    confirm_medium,
                    ..Default::default()
                });
            }
        }
        out
    }

    #[test]
    fn high_and_critical_always_confirm() {
        for pol in permutations() {
            assert!(
                pol.needs_confirm(Risk::High),
                "High must confirm with confirm_high={}, confirm_medium={}",
                pol.confirm_high,
                pol.confirm_medium
            );
            assert!(
                pol.needs_confirm(Risk::Critical),
                "Critical must confirm with confirm_high={}, confirm_medium={}",
                pol.confirm_high,
                pol.confirm_medium
            );
        }
    }

    #[test]
    fn low_never_confirms_and_medium_follows_its_setting() {
        for pol in permutations() {
            assert!(!pol.needs_confirm(Risk::Low));
            assert_eq!(pol.needs_confirm(Risk::Medium), pol.confirm_medium);
        }
    }

    /// The migration case: a file written by a version where this was a real
    /// switch, with the switch turned off.
    #[test]
    fn a_legacy_policy_file_still_parses_and_still_confirms() {
        let legacy = r#"{
            "roots": [],
            "max_read_chars": 20000,
            "confirm_high": false,
            "confirm_medium": false
        }"#;
        let mut pol: Policy = serde_json::from_str(legacy).expect("legacy policy must still parse");
        assert!(
            !pol.confirm_high,
            "the field should have been read exactly as written"
        );
        assert!(
            pol.needs_confirm(Risk::High),
            "a legacy false must not disable the dialog"
        );

        // ...and the file is corrected, rather than left describing a protection
        // that is in force as though it were switched off.
        pol.clamp();
        assert!(pol.confirm_high, "clamp must write the truth back");
        assert!(pol.needs_confirm(Risk::High));
    }

    #[test]
    fn an_empty_policy_file_lands_on_the_safe_side() {
        let mut pol: Policy = serde_json::from_str("{}").expect("an empty object must parse");
        pol.clamp();
        assert!(pol.needs_confirm(Risk::High));
        assert!(pol.needs_confirm(Risk::Critical));
        assert!(
            !pol.needs_confirm(Risk::Medium),
            "medium confirmation stays off by default"
        );
    }

    #[test]
    fn the_risk_tiers_serialise_as_the_names_the_rest_of_the_app_uses() {
        assert_eq!(serde_json::to_string(&Risk::High).unwrap(), "\"high\"");
        assert_eq!(
            serde_json::to_string(&Risk::Critical).unwrap(),
            "\"critical\""
        );
    }
}
