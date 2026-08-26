//! The path guard. Every filesystem tool goes through `resolve`, and nothing
//! else in this program is allowed to turn a string from the model into a path.
//!
//! The threat is not a clever model, it is a compromised frontend. Assume the
//! JavaScript is hostile and can send any string it likes to any command. What
//! must still hold afterwards:
//!
//!   * the path is inside one of the configured roots, after symlinks,
//!     junctions and `..` have been resolved, not before;
//!   * it is not a credential store, not the app's own data, not a startup
//!     folder;
//!   * writes cannot produce something Windows will execute.
//!
//! The order matters and is the part that is easy to get wrong. Checking a
//! string for `..` and then opening it is not a defence, because
//! `Downloads\photos` may be a junction to `C:\Windows`. So the path is resolved
//! to what the filesystem says it really is *first*, and only the resolved path
//! is ever compared against the roots or handed to an operation.

use crate::policy::Policy;
use crate::rules::{canonical, is_blocked_ext, DENIED_DIRS, DENIED_FRAGMENTS, DEVICE_NAMES};
use std::path::{Component, Path, PathBuf};

/// What the caller intends to do, because a path can be legal to read and
/// illegal to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Must already exist.
    Read,
    /// May or may not exist; its parent must.
    Write,
    /// Must already exist and will be sent to the Recycle Bin.
    Remove,
}

pub struct Guard {
    roots: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    home: Option<PathBuf>,
}

impl Guard {
    pub fn new(policy: &Policy, denied: Vec<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            roots: policy.roots.clone(),
            denied,
            home,
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Turn a string from the model into a path that is safe to act on, or say
    /// why not. The error text is written to be read by the model and repeated
    /// to the user, so it explains the rule rather than just refusing.
    pub fn resolve(&self, raw: &str, intent: Intent) -> Result<PathBuf, String> {
        if self.roots.is_empty() {
            return Err(
                "Compass has no folders it is allowed to use, so this was refused. \
                        Add one in the desktop settings."
                    .into(),
            );
        }

        let expanded = self.expand(raw)?;
        self.check_shape(&expanded)?;

        let resolved = match intent {
            Intent::Read | Intent::Remove => canonical(&expanded)
                .map_err(|_| format!("there is nothing at {}", show(&expanded)))?,
            Intent::Write => self.resolve_for_create(&expanded)?,
        };

        // Only now, against the real path, is the sandbox decided.
        self.check_inside_root(&resolved)?;
        self.check_not_denied(&resolved)?;

        if matches!(intent, Intent::Write) {
            self.check_writable_name(&resolved)?;
        }

        Ok(resolved)
    }

    /// A directory that must exist and be usable as a destination.
    pub fn resolve_dir(&self, raw: &str, intent: Intent) -> Result<PathBuf, String> {
        let p = self.resolve(raw, intent)?;
        if p.exists() && !p.is_dir() {
            return Err(format!("{} is a file, not a folder", show(&p)));
        }
        Ok(p)
    }

    /// `~` is what the model is told to write, so it has to mean something here.
    /// Nothing else is expanded: no environment variables, because `%APPDATA%`
    /// would be a way to name a denied directory without spelling it.
    fn expand(&self, raw: &str) -> Result<PathBuf, String> {
        // Only the *leading* whitespace is trimmed. Trimming the trailing kind
        // would silently erase the very thing `check_raw` exists to catch:
        // `payload.exe ` would become `payload.exe`, pass the executable check as
        // a different string, and then be created by Windows as an executable.
        // A stray trailing space is therefore an error rather than something
        // tidied away, which costs a clear message and buys the whole rule.
        let s = raw.trim_start();
        if s.is_empty() {
            return Err("that was an empty path".into());
        }
        if s.len() > 400 {
            return Err("that path is unreasonably long".into());
        }
        if let Some(c) = s.chars().find(|c| (*c as u32) < 0x20) {
            return Err(format!(
                "that path contained a control character (0x{:02x})",
                c as u32
            ));
        }
        if s.contains('%') {
            return Err(
                "environment variables are not expanded in paths — write the folder out, \
                        or use ~ for the home folder"
                    .into(),
            );
        }

        let unified = s.replace('/', "\\");
        Self::check_raw(&unified)?;

        if unified == "~" {
            return self
                .home
                .clone()
                .ok_or_else(|| "there is no home folder".to_string());
        }
        if let Some(rest) = unified.strip_prefix("~\\") {
            let home = self
                .home
                .clone()
                .ok_or_else(|| "there is no home folder".to_string())?;
            return Ok(home.join(rest));
        }
        Ok(PathBuf::from(unified))
    }

    /// Checks that have to happen on the raw string, because `Path` throws the
    /// evidence away.
    ///
    /// Windows silently strips trailing spaces and dots from file names, and
    /// Rust's `Path::components` does the same — so by the time a path is a
    /// `Path`, `payload.exe ` has already become `payload.exe` and a component
    /// inspection cannot tell they were ever different.
    ///
    /// That difference is exploitable. `Path::extension()` on `payload.exe `
    /// yields `exe ` with the space attached, which is not in the blocked list, so
    /// the executable check would pass — and then Windows would create
    /// `payload.exe` and happily run it. The same trick works with a trailing dot.
    /// A test found this; it is not theoretical.
    ///
    /// Public so it can be verified on its own. It is pure string logic with no
    /// filesystem access, which means it can be checked on any platform even
    /// though the rule it enforces is a Windows one.
    pub fn check_raw(raw: &str) -> Result<(), String> {
        for seg in raw.split('\\') {
            if seg.is_empty() || seg == "." || seg == ".." {
                continue; // handled by the component walk, which reports better
            }
            if seg.ends_with(' ') || seg.ends_with('.') {
                return Err(
                    "a file or folder name cannot end with a space or a dot — Windows silently \
                     removes them, which would make this path mean something other than it says"
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// Refuse shapes that are either an attack or a mistake, before touching the
    /// disk. This is not the sandbox — it is a filter that keeps the interesting
    /// cases small enough to reason about.
    fn check_shape(&self, p: &Path) -> Result<(), String> {
        let s = p.as_os_str().to_string_lossy();

        // UNC and device namespaces reach the network and raw devices, and
        // neither belongs in a documents folder.
        if s.starts_with("\\\\") {
            return Err("network and device paths are not allowed".into());
        }

        // NTFS alternate data streams: C:\a\b.txt:hidden. One colon, and only as
        // the drive separator.
        let colons = s.matches(':').count();
        let drive_colon = s.as_bytes().get(1) == Some(&b':');
        if colons > usize::from(drive_colon) {
            return Err(
                "that path contained a stream or drive separator that is not allowed".into(),
            );
        }

        for c in p.components() {
            match c {
                // `..` never survives to the filesystem. Even though
                // canonicalisation would resolve it, resolving it and then
                // finding it landed outside gives a confusing error; and for
                // paths that do not exist yet there is nothing to resolve.
                Component::ParentDir => {
                    return Err("`..` is not allowed in a path".into());
                }
                Component::Normal(name) => {
                    let n = name.to_string_lossy().to_ascii_lowercase();
                    let stem = n.split('.').next().unwrap_or("");
                    if DEVICE_NAMES.contains(&stem) {
                        return Err(format!("`{n}` is a reserved Windows device name"));
                    }
                    if n.ends_with(' ') || n.ends_with('.') {
                        return Err("a file or folder name cannot end with a space or a dot".into());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Resolve a path that may not exist yet, by canonicalising the deepest part
    /// that does and re-attaching the rest.
    ///
    /// This is what closes the junction hole for new files. `write_file` into
    /// `Downloads\reports\out.txt` cannot be canonicalised, because `out.txt` is
    /// not there — but `Downloads\reports` can be, and if that is a junction
    /// pointing at `C:\Windows\System32` the canonical form says so and the root
    /// check below rejects it.
    fn resolve_for_create(&self, p: &Path) -> Result<PathBuf, String> {
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cur = p.to_path_buf();

        loop {
            if cur.exists() {
                let mut out = canonical(&cur)
                    .map_err(|e| format!("could not resolve {}: {e}", show(&cur)))?;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            let name = match cur.file_name() {
                Some(n) => n.to_os_string(),
                // Ran out of path without finding anything real: a bad drive.
                None => return Err(format!("{} is not a place on this computer", show(p))),
            };
            tail.push(name);
            match cur.parent() {
                Some(par) if !par.as_os_str().is_empty() => cur = par.to_path_buf(),
                _ => return Err(format!("{} is not a place on this computer", show(p))),
            }
            // A path 40 levels of non-existent directories deep is not a real
            // request; refusing it keeps this loop obviously finite.
            if tail.len() > 40 {
                return Err("that path is nested too deeply".into());
            }
        }
    }

    /// The sandbox itself. Component-wise, so `...\Navid2` can never be mistaken
    /// for a child of `...\Navid`, which a string prefix check would allow.
    fn check_inside_root(&self, resolved: &Path) -> Result<(), String> {
        if self.roots.iter().any(|r| resolved.starts_with(r)) {
            return Ok(());
        }
        Err(format!(
            "{} is outside the folders Compass may use. Allowed: {}",
            show(resolved),
            self.roots
                .iter()
                .map(|r| show(r))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// Denied even inside a root: the app's own state, credentials, and the
    /// places Windows runs things from at logon.
    fn check_not_denied(&self, resolved: &Path) -> Result<(), String> {
        for d in &self.denied {
            if resolved.starts_with(d) {
                return Err("that is Compass's own data, which the assistant may not touch".into());
            }
        }

        let lower = resolved.as_os_str().to_string_lossy().to_ascii_lowercase();
        for frag in DENIED_FRAGMENTS {
            if lower.contains(frag) {
                return Err(format!(
                    "that path looks like private credentials or system data ({frag}), so it is refused"
                ));
            }
        }
        for c in resolved.components() {
            if let Component::Normal(name) = c {
                let n = name.to_string_lossy().to_ascii_lowercase();
                if DENIED_DIRS.contains(&n.as_str()) {
                    return Err(format!(
                        "`{n}` is a Windows startup location and is refused"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Nothing the agent creates may be something Windows will run.
    fn check_writable_name(&self, resolved: &Path) -> Result<(), String> {
        if let Some(ext) = resolved.extension() {
            let e = ext.to_string_lossy();
            if is_blocked_ext(&e) {
                return Err(format!(
                    "Compass will not create or change a .{e} file — programs and scripts are \
                     outside what the assistant may write"
                ));
            }
        }
        Ok(())
    }

    /// Is this file safe to hand to Windows' default handler? Opening a document
    /// is useful; "opening" an executable is running it.
    pub fn openable(&self, resolved: &Path) -> Result<(), String> {
        if resolved.is_dir() {
            return Ok(());
        }
        match resolved.extension() {
            Some(ext) => {
                let e = ext.to_string_lossy();
                if is_blocked_ext(&e) {
                    Err(format!(
                        "Compass will not open a .{e} file, because opening one runs it"
                    ))
                } else {
                    Ok(())
                }
            }
            // No extension means Windows decides, and that decision is not ours
            // to gamble with.
            None => Err("Compass will only open files that have a known file type".into()),
        }
    }
}

/// Paths are shown to the model and to the user, so they are shown plainly.
pub fn show(p: &Path) -> String {
    p.display().to_string()
}

/// These assert Windows path semantics — drive letters, backslash separators,
/// reserved device names — so there is nothing meaningful for them to check on
/// another platform. The broader suite lives in `desktop/verify`, which runs the
/// same source against a real filesystem including NTFS junctions.
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::policy::Policy;

    fn guard(root: &Path) -> Guard {
        let pol = Policy {
            roots: vec![root.to_path_buf()],
            ..Default::default()
        };
        Guard::new(&pol, vec![root.join("private")], Some(root.to_path_buf()))
    }

    /// A scratch directory that is deliberately NOT under the system temp dir.
    ///
    /// On Windows `std::env::temp_dir()` lives inside AppData, and "appdata" is a
    /// denied fragment — so every path under it is refused and a test using it
    /// passes for entirely the wrong reason. An "is_err" assertion that succeeds
    /// because the whole directory is banned tests nothing at all, so this note
    /// is longer than the code it explains.
    fn tmp() -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("guard-scratch")
            .join(format!("p{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        canonical(&d).unwrap()
    }

    #[test]
    fn accepts_a_file_inside_the_root() {
        let root = tmp();
        std::fs::write(root.join("ok.txt"), "hi").unwrap();
        let g = guard(&root);
        assert!(g
            .resolve(&root.join("ok.txt").display().to_string(), Intent::Read)
            .is_ok());
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = tmp();
        let g = guard(&root);
        let attack = format!("{}\\..\\..\\Windows\\System32", root.display());
        assert!(g.resolve(&attack, Intent::Read).is_err());
    }

    #[test]
    fn rejects_outside_the_root() {
        let root = tmp();
        let g = guard(&root);
        assert!(g
            .resolve("C:\\Windows\\System32\\drivers\\etc\\hosts", Intent::Read)
            .is_err());
    }

    #[test]
    fn rejects_unc_and_streams_and_devices() {
        let root = tmp();
        let g = guard(&root);
        assert!(g.resolve("\\\\evil\\share\\x.txt", Intent::Read).is_err());
        assert!(g
            .resolve(&format!("{}\\a.txt:hidden", root.display()), Intent::Write)
            .is_err());
        assert!(g
            .resolve(&format!("{}\\NUL", root.display()), Intent::Write)
            .is_err());
        assert!(g
            .resolve(&format!("{}\\con.txt", root.display()), Intent::Write)
            .is_err());
    }

    #[test]
    fn rejects_environment_variables() {
        let root = tmp();
        let g = guard(&root);
        assert!(g.resolve("%APPDATA%\\x.txt", Intent::Read).is_err());
    }

    #[test]
    fn refuses_to_write_executables() {
        let root = tmp();
        let g = guard(&root);
        for name in ["evil.exe", "evil.bat", "evil.ps1", "evil.lnk", "evil.dll"] {
            let p = format!("{}\\{name}", root.display());
            assert!(
                g.resolve(&p, Intent::Write).is_err(),
                "{name} should be refused"
            );
        }
        // ...but an ordinary document is fine.
        assert!(g
            .resolve(&format!("{}\\notes.md", root.display()), Intent::Write)
            .is_ok());
    }

    #[test]
    fn allows_a_new_file_in_an_existing_folder() {
        let root = tmp();
        let g = guard(&root);
        let p = format!("{}\\brand-new.txt", root.display());
        assert!(g.resolve(&p, Intent::Write).is_ok());
        // Reading it, though, must fail while it does not exist.
        assert!(g.resolve(&p, Intent::Read).is_err());
    }

    #[test]
    fn rejects_the_denied_subtree_and_credential_names() {
        let root = tmp();
        std::fs::create_dir_all(root.join("private")).unwrap();
        std::fs::write(root.join("private").join("a.txt"), "x").unwrap();
        let g = guard(&root);
        assert!(g
            .resolve(
                &root.join("private").join("a.txt").display().to_string(),
                Intent::Read
            )
            .is_err());
        assert!(g
            .resolve(&format!("{}\\id_rsa", root.display()), Intent::Read)
            .is_err());
        assert!(g
            .resolve(&format!("{}\\.env", root.display()), Intent::Write)
            .is_err());
    }

    #[test]
    fn a_sibling_root_prefix_is_not_inside_the_root() {
        // The classic string-prefix bug: C:\x\Navid2 must not count as a child
        // of C:\x\Navid.
        let base = tmp();
        let root = base.join("Navid");
        let sibling = base.join("Navid2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("x.txt"), "x").unwrap();
        let g = guard(&root);
        assert!(g
            .resolve(&sibling.join("x.txt").display().to_string(), Intent::Read)
            .is_err());
    }

    #[test]
    fn tilde_expands_to_the_home_folder() {
        let root = tmp();
        std::fs::write(root.join("home.txt"), "x").unwrap();
        let g = guard(&root);
        assert!(g.resolve("~/home.txt", Intent::Read).is_ok());
        assert!(g.resolve("~\\home.txt", Intent::Read).is_ok());
    }

    #[test]
    fn empty_roots_refuse_everything() {
        let pol = Policy::default();
        let g = Guard::new(&pol, vec![], Some(tmp()));
        assert!(g.resolve("~/anything.txt", Intent::Read).is_err());
    }
}
