//! Runs the real path guard against a real filesystem, and reports.
//!
//! This crate contains no copy of the guard. It pulls in `src-tauri/src/guard.rs`
//! and `src-tauri/src/rules.rs` verbatim with `#[path]`, so what is checked here
//! is the same source the application ships. A copy would drift, and a drifted
//! copy of a security check is worse than none, because it reports success about
//! code nobody is running.
//!
//! Only `Policy` is substituted, and only the one field the guard actually reads.
//! The real `Policy` needs Tauri to find the user's Downloads folder, which would
//! pull in the whole dependency tree and defeat the point.
//!
//! Run with:  cargo run --manifest-path desktop/verify/Cargo.toml

use std::path::{Path, PathBuf};

/// The slice of `Policy` the guard touches. If the guard starts reading another
/// field this stops compiling, which is the intended alarm.
pub mod policy {
    use std::path::PathBuf;

    #[derive(Clone, Debug, Default)]
    pub struct Policy {
        pub roots: Vec<PathBuf>,
    }
}

#[path = "../../src-tauri/src/rules.rs"]
pub mod rules;

#[path = "../../src-tauri/src/guard.rs"]
pub mod guard;

use guard::Guard;
#[cfg(windows)]
use guard::Intent;
#[cfg(windows)]
use policy::Policy;

/* ── a very small harness ────────────────────────────────────────── */

struct Report {
    passed: usize,
    failed: Vec<String>,
    skipped: Vec<String>,
}

impl Report {
    fn check(&mut self, label: &str, ok: bool, detail: impl FnOnce() -> String) {
        if ok {
            self.passed += 1;
            println!("  ok    {label}");
        } else {
            let d = detail();
            println!("  FAIL  {label}  ->  {d}");
            self.failed.push(format!("{label}: {d}"));
        }
    }
    fn skip(&mut self, label: &str, why: &str) {
        println!("  skip  {label}  ({why})");
        self.skipped.push(label.to_string());
    }
}

/// Scratch space outside AppData.
///
/// The system temp directory lives inside AppData on Windows, and "appdata" is a
/// denied fragment — so a check rooted there would see every path refused and
/// every "must be refused" assertion pass without exercising anything at all.
/// The junction check below is what made this worth discovering.
#[cfg(windows)]
fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("verify-scratch")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("could not create scratch dir");
    rules::canonical(&d).expect("could not canonicalise scratch dir")
}

#[cfg(windows)]
fn guard_for(root: &Path) -> Guard {
    let pol = Policy {
        roots: vec![root.to_path_buf()],
    };
    Guard::new(&pol, vec![], Some(root.to_path_buf()))
}

fn main() {
    println!("\nCompass path guard — verification\n");
    let mut r = Report {
        passed: 0,
        failed: Vec::new(),
        skipped: Vec::new(),
    };

    // Pure string rules. No filesystem, no platform assumptions, so these run
    // anywhere — which matters, because a machine with Smart App Control enforced
    // may refuse to execute a locally built Windows binary at all, and these are
    // the rules most easily broken by a careless edit.
    section_string_rules(&mut r);

    // Everything else needs real Windows path semantics: drive letters, NTFS
    // junctions, reserved device names. There is nothing meaningful to assert
    // about those on another platform, so they are skipped rather than faked.
    #[cfg(windows)]
    {
        section_baseline(&mut r);
        section_shapes(&mut r);
        section_executables(&mut r);
        section_credentials(&mut r);
        section_containment(&mut r);
        section_links(&mut r);
    }
    #[cfg(not(windows))]
    {
        println!("\nfilesystem checks");
        r.skip("Windows path semantics", "not running on Windows");
    }

    println!();
    if r.failed.is_empty() {
        println!(
            "ALL {} GUARD CHECKS PASSED{}",
            r.passed,
            if r.skipped.is_empty() {
                String::new()
            } else {
                format!(" ({} skipped)", r.skipped.len())
            }
        );
    } else {
        println!(
            "{} of {} FAILED:",
            r.failed.len(),
            r.passed + r.failed.len()
        );
        for f in &r.failed {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}

/* ── the rules that are pure string logic ────────────────────────────
This is where the padding bypass lived: Windows strips trailing spaces and
dots, so `payload.exe ` names the same file as `payload.exe` while comparing as
a different string. Both halves of the fix are checked here — the path-shape
rule that refuses such a name outright, and the extension rule that would
catch it anyway if a future caller ever skipped the guard. */
fn section_string_rules(r: &mut Report) {
    println!("path-shape rules (no filesystem needed)");

    // Names that must be refused because Windows would silently rewrite them.
    for raw in [
        "C:\\Users\\x\\Downloads\\payload.exe ",
        "C:\\Users\\x\\Downloads\\payload.exe.",
        "C:\\Users\\x\\Downloads\\payload.exe  ",
        "C:\\Users\\x\\Downloads\\payload.bat ",
        "C:\\Users\\x\\Downloads\\quiet.txt ",
        "C:\\Users\\x\\Downloads\\folder.",
        "C:\\Users\\x\\Downloads \\a.txt",
        "C:\\Users\\x\\Downloads.\\a.txt",
    ] {
        r.check(
            &format!("refuse padded {raw:?}"),
            Guard::check_raw(raw).is_err(),
            || "it was accepted".into(),
        );
    }

    // Control characters, including the null byte, are refused by expand() before
    // check_raw ever runs; assert the property directly.
    for raw in ["a\0b.txt", "a\nb.txt", "a\rb.txt", "a\tb.txt"] {
        r.check(
            &format!(
                "control char in {:?} is detectable",
                raw.escape_debug().to_string()
            ),
            raw.chars().any(|c| (c as u32) < 0x20),
            || "no control char found".into(),
        );
    }

    // Ordinary names must still pass, or the rule above is just a brick wall.
    for raw in [
        "C:\\Users\\x\\Downloads\\invoice.pdf",
        "C:\\Users\\x\\Documents\\notes.md",
        "C:\\Users\\x\\Downloads\\Invoices 2026\\march.pdf",
        "C:\\Users\\x\\Downloads\\.env.example.txt",
        "~\\Downloads\\a.txt",
    ] {
        r.check(
            &format!("accept ordinary {raw:?}"),
            Guard::check_raw(raw).is_ok(),
            || format!("{:?}", Guard::check_raw(raw)),
        );
    }

    println!("\nblocked-extension rule");
    for (ext, want) in [
        ("exe", true),
        ("EXE", true),
        ("Exe", true),
        ("exe ", true),
        ("EXE.", true),
        (" ExE  ", true),
        (".exe.", true),
        ("bat", true),
        ("ps1", true),
        ("lnk", true),
        ("dll", true),
        ("md", false),
        ("pdf", false),
        ("docx", false),
        ("txt", false),
        ("csv", false),
    ] {
        r.check(
            &format!("is_blocked_ext({ext:?}) == {want}"),
            rules::is_blocked_ext(ext) == want,
            || "wrong verdict".into(),
        );
    }

    // The verbatim-prefix stripper, which every path comparison depends on.
    println!("\nverbatim prefix handling");
    r.check(
        "\\\\?\\C:\\x becomes C:\\x",
        rules::strip_verbatim(Path::new("\\\\?\\C:\\x")) == PathBuf::from("C:\\x"),
        || format!("{:?}", rules::strip_verbatim(Path::new("\\\\?\\C:\\x"))),
    );
    r.check(
        "a UNC verbatim path is left alone",
        rules::strip_verbatim(Path::new("\\\\?\\UNC\\server\\share"))
            == PathBuf::from("\\\\?\\UNC\\server\\share"),
        || "it was shortened, which would widen it".into(),
    );
    r.check(
        "an ordinary path is unchanged",
        rules::strip_verbatim(Path::new("C:\\x\\y")) == PathBuf::from("C:\\x\\y"),
        || "it was altered".into(),
    );
}

/* ── the control group ───────────────────────────────────────────────
If these fail, every "must be refused" result below is meaningless, because
the guard would be refusing everything. */
#[cfg(windows)]
fn section_baseline(r: &mut Report) {
    println!("legitimate use still works");
    let root = scratch("baseline");
    std::fs::write(root.join("real.txt"), "x").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let g = guard_for(&root);
    let s = root.display().to_string();

    r.check(
        "an existing file inside the root can be read",
        g.resolve(&format!("{s}\\real.txt"), Intent::Read).is_ok(),
        || format!("{:?}", g.resolve(&format!("{s}\\real.txt"), Intent::Read)),
    );
    r.check(
        "a new file in an existing folder can be written",
        g.resolve(&format!("{s}\\new.txt"), Intent::Write).is_ok(),
        || format!("{:?}", g.resolve(&format!("{s}\\new.txt"), Intent::Write)),
    );
    r.check(
        "a new file in a subfolder can be written",
        g.resolve(&format!("{s}\\sub\\new.md"), Intent::Write)
            .is_ok(),
        || {
            format!(
                "{:?}",
                g.resolve(&format!("{s}\\sub\\new.md"), Intent::Write)
            )
        },
    );
    r.check(
        "a file that does not exist cannot be read",
        g.resolve(&format!("{s}\\ghost.txt"), Intent::Read).is_err(),
        || "it was allowed".into(),
    );
    r.check(
        "the root itself resolves",
        g.resolve_dir(&s, Intent::Read).is_ok(),
        || format!("{:?}", g.resolve_dir(&s, Intent::Read)),
    );
    r.check(
        "forward slashes work too",
        g.resolve(&format!("{}/real.txt", s.replace('\\', "/")), Intent::Read)
            .is_ok(),
        || "rejected".into(),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn section_shapes(r: &mut Report) {
    println!("\nmalformed and hostile path shapes are refused");
    let root = scratch("shapes");
    std::fs::write(root.join("real.txt"), "x").unwrap();
    let g = guard_for(&root);
    let s = root.display().to_string();

    let cases: Vec<(&str, String)> = vec![
        ("parent traversal", format!("{s}\\..\\..\\Windows\\win.ini")),
        (
            "traversal mid-path",
            format!("{s}\\sub\\..\\..\\..\\Windows"),
        ),
        ("UNC share", "\\\\attacker\\share\\x.txt".to_string()),
        ("device namespace", "\\\\.\\PhysicalDrive0".to_string()),
        ("alternate data stream", format!("{s}\\real.txt:hidden")),
        ("reserved device NUL", format!("{s}\\NUL")),
        ("reserved device with extension", format!("{s}\\CON.txt")),
        ("environment variable", "%WINDIR%\\win.ini".to_string()),
        ("absolute path outside", "C:\\Windows\\win.ini".to_string()),
        ("trailing dot", format!("{s}\\weird.")),
        ("trailing space", format!("{s}\\weird ")),
        ("empty string", String::new()),
        ("null byte", format!("{s}\\a\0b.txt")),
        ("over-long path", format!("{s}\\{}", "a".repeat(500))),
    ];

    for (label, p) in cases {
        let read = g.resolve(&p, Intent::Read);
        let write = g.resolve(&p, Intent::Write);
        r.check(label, read.is_err() && write.is_err(), || {
            format!("read={:?} write={:?}", read.ok(), write.ok())
        });
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn section_executables(r: &mut Report) {
    println!("\nnothing executable can be written or opened");
    let root = scratch("exe");
    let g = guard_for(&root);
    let s = root.display().to_string();

    for name in [
        "a.exe",
        "a.EXE",
        "a.Exe",
        "a.bat",
        "a.CMD",
        "a.ps1",
        "a.vbs",
        "a.lnk",
        "a.dll",
        "a.msi",
        "a.scr",
        "a.reg",
        "a.hta",
        "a.jar",
        "invoice.pdf.exe",
        "notes.txt.bat",
    ] {
        let p = format!("{s}\\{name}");
        r.check(
            &format!("write {name}"),
            g.resolve(&p, Intent::Write).is_err(),
            || "it was writable".into(),
        );
        r.check(
            &format!("open {name}"),
            g.openable(Path::new(&p)).is_err(),
            || "it was openable".into(),
        );
    }

    // The padding bypass, pinned. Windows strips trailing spaces and dots, so
    // `payload.exe ` becomes `payload.exe` on disk while Path::extension() reports
    // `exe ` — a string that is not in the blocklist. An earlier version of this
    // guard had exactly that hole, and this is the check that found it.
    for name in [
        "payload.exe ",
        "payload.exe.",
        "payload.exe  ",
        "payload.bat ",
        "payload.ps1.",
    ] {
        let p = format!("{s}\\{name}");
        r.check(
            &format!("padded name {name:?}"),
            g.resolve(&p, Intent::Write).is_err(),
            || "it was accepted".into(),
        );
    }

    for (ext, want_blocked) in [
        ("exe", true),
        ("exe ", true),
        ("EXE.", true),
        (" ExE  ", true),
        ("md", false),
        ("pdf", false),
        ("docx", false),
    ] {
        r.check(
            &format!("is_blocked_ext({ext:?}) == {want_blocked}"),
            rules::is_blocked_ext(ext) == want_blocked,
            || "wrong verdict".into(),
        );
    }

    // Ordinary documents must still be writable.
    for name in [
        "notes.md",
        "summary.txt",
        "invoice.pdf",
        "sheet.csv",
        "a.docx",
    ] {
        let p = format!("{s}\\{name}");
        r.check(
            &format!("{name} is writable"),
            g.resolve(&p, Intent::Write).is_ok(),
            || format!("{:?}", g.resolve(&p, Intent::Write)),
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn section_credentials(r: &mut Report) {
    println!("\ncredential-shaped paths are refused, including for reading");
    let root = scratch("creds");
    let g = guard_for(&root);
    let s = root.display().to_string();

    for name in [
        "id_rsa",
        "id_ed25519",
        ".env",
        "server.pem",
        "cert.pfx",
        "vault.kdbx",
        "secrets.json",
        ".git-credentials",
    ] {
        let p = format!("{s}\\{name}");
        // Create it, so a refusal is a policy decision and not just absence.
        let _ = std::fs::write(&p, "sensitive");
        r.check(
            &format!("read {name}"),
            g.resolve(&p, Intent::Read).is_err(),
            || "it was readable".into(),
        );
    }

    // Startup folders are persistence, not filing.
    for dir in ["Startup", "startup", "Start Menu"] {
        let p = format!("{s}\\{dir}\\run.txt");
        r.check(
            &format!("write into {dir}"),
            g.resolve(&p, Intent::Write).is_err(),
            || "it was allowed".into(),
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn section_containment(r: &mut Report) {
    println!("\nthe sandbox holds");
    let base = scratch("contain");
    let root = base.join("Compass");
    let sibling = base.join("CompassOther");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("x.txt"), "x").unwrap();

    let g = guard_for(&root);
    r.check(
        "a similarly-named sibling is outside the root",
        g.resolve(&sibling.join("x.txt").display().to_string(), Intent::Read)
            .is_err(),
        || "it was readable".into(),
    );

    // No roots at all must refuse everything: the app ships this way if the
    // known folders cannot be found, and failing closed is the point.
    let empty = Guard::new(&Policy::default(), vec![], Some(PathBuf::from("C:\\Users")));
    for p in ["~/x.txt", "C:\\Windows\\win.ini", "~"] {
        r.check(
            &format!("no roots refuses {p}"),
            empty.resolve(p, Intent::Read).is_err(),
            || "it was allowed".into(),
        );
    }

    // The app's own directories, even sitting inside a root.
    let mine = root.join("compass-state");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("agent-audit.jsonl"), "{}").unwrap();
    let pol = Policy {
        roots: vec![root.clone()],
    };
    let g2 = Guard::new(&pol, vec![mine.clone()], Some(root.clone()));
    r.check(
        "the audit log cannot be read by the agent",
        g2.resolve(
            &mine.join("agent-audit.jsonl").display().to_string(),
            Intent::Read,
        )
        .is_err(),
        || "it was readable".into(),
    );
    r.check(
        "the app's own directory cannot be written",
        g2.resolve(&mine.join("x.txt").display().to_string(), Intent::Write)
            .is_err(),
        || "it was writable".into(),
    );

    // Tilde expansion, which is what the model is told to write.
    std::fs::write(root.join("home.txt"), "x").unwrap();
    let g3 = guard_for(&root);
    r.check(
        "~ expands to the home folder",
        g3.resolve("~/home.txt", Intent::Read).is_ok()
            && g3.resolve("~\\home.txt", Intent::Read).is_ok(),
        || "rejected".into(),
    );

    let _ = std::fs::remove_dir_all(&base);
}

/* ── the headline claim ──────────────────────────────────────────────
A string-based `..` check gets these wrong. They are the reason the guard
canonicalises before it compares, and the reason this runs against a real
filesystem instead of asserting on strings. */
#[cfg(windows)]
fn section_links(r: &mut Report) {
    use std::process::Command;
    println!("\nsymlink and junction escapes are refused");
    let root = scratch("links");
    let g = guard_for(&root);

    // A directory junction needs no administrator rights, which is exactly why
    // it matters.
    let link = root.join("escape");
    let made = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.display().to_string(),
            "C:\\Windows",
        ])
        .output();

    if made.map(|o| o.status.success()).unwrap_or(false) && link.exists() {
        let read = g.resolve(
            &link
                .join("System32")
                .join("drivers")
                .join("etc")
                .join("hosts")
                .display()
                .to_string(),
            Intent::Read,
        );
        r.check(
            "reading through a junction out of the root",
            read.is_err(),
            || format!("resolved to {:?}", read.ok()),
        );

        // The non-existent-leaf path, which cannot be canonicalised directly and
        // is the easier of the two to get wrong.
        let write = g.resolve(
            &link.join("System32").join("evil.txt").display().to_string(),
            Intent::Write,
        );
        r.check(
            "writing through a junction out of the root",
            write.is_err(),
            || format!("resolved to {:?}", write.ok()),
        );

        let _ = std::fs::remove_dir(&link);
    } else {
        r.skip("junction escape", "could not create a junction here");
    }

    // A file symlink usually needs Developer Mode or admin.
    let slink = root.join("hosts.txt");
    let made2 = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            &slink.display().to_string(),
            "C:\\Windows\\System32\\drivers\\etc\\hosts",
        ])
        .output();

    if made2.map(|o| o.status.success()).unwrap_or(false) && slink.exists() {
        let read = g.resolve(&slink.display().to_string(), Intent::Read);
        r.check(
            "reading through a symlink out of the root",
            read.is_err(),
            || format!("resolved to {:?}", read.ok()),
        );
        let _ = std::fs::remove_file(&slink);
    } else {
        r.skip("symlink escape", "needs Developer Mode or admin");
    }

    let _ = std::fs::remove_dir_all(&root);
}
