//! The rules that have no setting, and the one filesystem primitive the guard is
//! built on.
//!
//! This module depends on nothing but `std`. That is deliberate and worth
//! keeping. It means the security-critical half of the path guard can be
//! compiled and tested entirely on its own — see `verify/` — and it means the
//! rules that decide whether a file can be written are not sitting behind a
//! supply chain.
//!
//! Everything here is `const`. There is no runtime path that widens any of it,
//! because "delete the line that stops you writing .exe files" is precisely the
//! request an injected prompt would make, and a rule that cannot be changed at
//! runtime cannot be argued with.

use std::path::{Path, PathBuf};

/// Extensions the agent may never create, write, rename to, or hand to the
/// system's default handler.
///
/// Writing one of these turns "organise my invoices" into "plant a program and
/// let Windows run it", which is the most valuable thing an attacker could get
/// out of this feature. The list is broad on purpose: a false refusal costs a
/// sentence of explanation, a false allow costs the machine.
pub const BLOCKED_EXT: &[&str] = &[
    "exe",
    "com",
    "scr",
    "pif",
    "msi",
    "msp",
    "msix",
    "appx",
    "appinstaller",
    "dll",
    "ocx",
    "sys",
    "drv",
    "cpl",
    "bat",
    "cmd",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "ws",
    "psc1",
    "ps1",
    "ps1xml",
    "psd1",
    "psm1",
    "reg",
    "inf",
    "hta",
    "chm",
    "lnk",
    "url",
    "scf",
    "jar",
    "gadget",
    "application",
    "vsix",
    "cab",
    "iso",
    "img",
    "vhd",
    "vhdx",
    "service",
    "desktop",
];

/// Path fragments that mean credentials, keys or private configuration.
///
/// Refused for reading as well as for writing, which is the part that matters:
/// `read_file` puts a file's contents into the model's context, and the model's
/// context goes over the network to a provider. An agent that can read
/// `id_rsa` is an exfiltration tool with extra steps.
pub const DENIED_FRAGMENTS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".gpg",
    ".azure",
    ".kube",
    ".docker",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "appdata",
    "application data",
    "credentials",
    "secrets",
    ".env",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    "keystore",
    "keychain",
    ".pem",
    ".pfx",
    ".p12",
    ".jks",
    ".keytab",
    ".kdbx",
    "ntuser.dat",
    "$recycle.bin",
    "system volume information",
];

/// Folders Windows runs things from at logon. Even inside an allowed root,
/// writing here is persistence rather than filing.
pub const DENIED_DIRS: &[&str] = &["startup", "start menu", "programs", "autostart"];

/// Names Windows still treats as devices, in any directory and with any
/// extension. Opening one can block on a handle rather than fail cleanly.
pub const DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Resolve a path to what the filesystem says it really is, with the Windows
/// verbatim prefix stripped.
///
/// This is the primitive the whole sandbox rests on. `std::fs::canonicalize`
/// follows symlinks and NTFS junctions and collapses `.` and `..`, which is
/// exactly what has to happen *before* the result is compared against the
/// allowed roots — checking a string for `..` and then opening it is not a
/// defence, because `Downloads\photos` may be a junction to `C:\Windows`.
///
/// The `\\?\` prefix is removed because it would otherwise appear in every path
/// shown to the user and to the model, and because a prefixed path and a plain
/// one do not compare equal even when they name the same file.
pub fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    Ok(strip_verbatim(&c))
}

/// `\\?\C:\x` becomes `C:\x`; `\\?\UNC\server\share` is left alone, because a
/// UNC path shortened to `\\server\share` would silently become a *different*
/// and more permissive thing than what was resolved.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        // A drive-letter verbatim path: safe to shorten.
        Some(rest) if rest.len() >= 2 && rest.as_bytes()[1] == b':' => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Is this extension one the agent may never produce or execute?
///
/// Trailing spaces and dots are stripped before comparing. Windows removes them
/// when it opens a file, so `payload.exe ` and `payload.exe` name the same thing
/// on disk while comparing as different strings — and a check that missed that
/// would be a blocklist with a one-character bypass. The guard already refuses
/// such names outright; this is the second lock on the same door, because this
/// function is public and the next caller may not go through the guard.
pub fn is_blocked_ext(ext: &str) -> bool {
    let e = ext.trim_matches([' ', '.']).to_ascii_lowercase();
    BLOCKED_EXT.contains(&e.as_str())
}
