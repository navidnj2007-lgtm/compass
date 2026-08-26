//! Showing what a write will change, and being able to put it back.
//!
//! `write_file` with `mode: "overwrite"` used to tell the user one thing: how many
//! characters were about to be written. That is not enough information to approve a
//! decision with. "Replace notes.md with 4,812 characters" is true of both a small
//! correction and the deletion of a term's work, and the approval card is the only
//! place anyone gets to tell those apart.
//!
//! So two things live here. A line diff, computed against what is on disk right now,
//! so the card can show what actually changes. And a backup of the previous contents,
//! so Undo restores the file rather than merely removing it from the chat.
//!
//! WHY UNDO NEEDED A BACKUP AT ALL
//!
//! The existing Undo is a snapshot of the *Compass state* — tasks, deadlines,
//! revision topics — taken before an action runs and swapped back afterwards. That
//! works because Compass state is small and lives in one object. A file does not: the
//! previous contents are gone the moment the write lands, so the old Undo could
//! honestly revert a task edit and could only pretend to revert a file overwrite.
//!
//! WHERE BACKUPS LIVE, AND WHY IT MATTERS
//!
//! In the app's own data directory, which `Policy::hard_denied` already denies to
//! every file tool. That is the important part: if backups sat in a documents folder,
//! the agent could read them (a backup of a file it was refused is a copy of a file
//! it was refused) and could overwrite them (an agent that can edit the undo history
//! can make a change unrevertable). Neither is possible where they are.
//!
//! They are bounded three ways — per file, in total, and by count — because an undo
//! history that grows without limit is a disk-filling bug waiting for someone with a
//! large file and a habit of editing it.

use crate::guard::show;
use std::path::{Path, PathBuf};

/// Largest file whose previous contents are kept. Above this, the write still
/// happens and the user is told plainly that Undo will not be available — a refusal
/// to write a large file would be worse, and a silent inability to undo worse still.
pub const MAX_BACKUP_BYTES: u64 = 4 * 1024 * 1024;

/// Backups kept at once. Oldest are removed first.
pub const MAX_BACKUPS: usize = 40;

/// Total bytes the backup store may occupy.
pub const MAX_BACKUP_TOTAL: u64 = 64 * 1024 * 1024;

/// Lines shown in a diff before it is summarised instead.
const MAX_DIFF_LINES: usize = 240;

/// Characters shown from any one line of a diff.
const MAX_DIFF_LINE: usize = 200;

pub fn backup_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let d = app.path().app_data_dir().ok()?.join("undo");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// A stable file name for a path's backup.
///
/// A hash rather than the path itself, because a path contains separators, colons
/// and characters that are not legal in a file name, and sanitising it would make two
/// different paths collide. FNV-1a is used because it is four lines and this is a
/// file name, not a security decision — a collision means one file's undo overwrites
/// another's, which is a bug, not a vulnerability.
fn key_for(p: &Path) -> String {
    let s = p.to_string_lossy().to_ascii_lowercase();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}.bak")
}

/// Keep a copy of what is at `p` now. Returns whether it worked, and why not.
///
/// Never fails the write it is protecting: a backup that could not be taken is worth
/// a sentence in the result, not a refusal to do what was asked.
pub fn save(app: &tauri::AppHandle, p: &Path) -> Result<(), String> {
    let Some(dir) = backup_dir(app) else {
        return Err("Compass has nowhere to keep the previous version".into());
    };
    let meta = std::fs::metadata(p).map_err(|e| format!("could not read it first: {e}"))?;
    if meta.len() > MAX_BACKUP_BYTES {
        return Err(format!(
            "it is {} and Compass keeps undo copies up to {}",
            meta.len(),
            MAX_BACKUP_BYTES
        ));
    }

    prune(&dir);

    let target = dir.join(key_for(p));
    std::fs::copy(p, &target).map_err(|e| format!("could not copy it: {e}"))?;
    // The original path, so a restore knows where it came from and cannot be
    // pointed somewhere else by an argument.
    std::fs::write(
        target.with_extension("path"),
        p.to_string_lossy().as_bytes(),
    )
    .map_err(|e| format!("could not record where it came from: {e}"))?;
    Ok(())
}

/// Put back what was saved. The destination comes from the recorded path, never from
/// the caller, so this cannot be turned into a write-anywhere primitive.
pub fn restore(app: &tauri::AppHandle, p: &Path) -> Result<PathBuf, String> {
    let dir = backup_dir(app).ok_or("Compass has no undo store")?;
    let src = dir.join(key_for(p));
    if !src.exists() {
        return Err(format!("there is no saved copy of {} to put back", show(p)));
    }
    let recorded = std::fs::read_to_string(src.with_extension("path"))
        .map_err(|_| "the saved copy has lost track of where it came from".to_string())?;
    if recorded.trim().to_ascii_lowercase() != p.to_string_lossy().to_ascii_lowercase() {
        return Err("the saved copy belongs to a different file, so nothing was restored".into());
    }
    std::fs::copy(&src, p).map_err(|e| format!("could not put it back: {e}"))?;
    Ok(p.to_path_buf())
}

pub fn has_backup(app: &tauri::AppHandle, p: &Path) -> bool {
    backup_dir(app)
        .map(|d| d.join(key_for(p)).exists())
        .unwrap_or(false)
}

/// Keep the store inside its bounds, oldest first.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "bak").unwrap_or(false))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    files.sort_by_key(|(t, _, _)| *t);

    let mut total: u64 = files.iter().map(|(_, n, _)| *n).sum();
    let mut count = files.len();

    for (_, len, path) in files {
        if count < MAX_BACKUPS && total < MAX_BACKUP_TOTAL {
            break;
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("path"));
        total = total.saturating_sub(len);
        count = count.saturating_sub(1);
    }
}

/* ── the diff ────────────────────────────────────────────────────── */

/// A line diff between what is on disk and what is proposed.
///
/// Longest-common-subsequence over lines, computed on a bounded window. Not a
/// character diff: for prose and notes a line is the unit a person reads a change in,
/// and a character diff of a reflowed paragraph is unreadable noise. Not a real
/// `diff(1)` either — no hunks, no context radius — because the consumer is an
/// approval card that needs to answer "what am I about to lose", and every changed
/// line is part of that answer.
///
/// The LCS table is O(n·m), so both sides are capped first. A 50,000-line file
/// against a 50,000-line proposal would allocate 2.5 billion cells, which is a
/// hang rather than a diff.
pub fn diff_lines(before: &str, after: &str) -> DiffSummary {
    const MAX_SIDE: usize = 1_200;

    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let too_big = a.len() > MAX_SIDE || b.len() > MAX_SIDE;

    if too_big {
        // Still useful, still honest: counts rather than lines.
        return DiffSummary {
            added: b.len().saturating_sub(a.len().min(b.len())),
            removed: a.len().saturating_sub(a.len().min(b.len())),
            lines: Vec::new(),
            truncated: true,
            before_lines: a.len(),
            after_lines: b.len(),
        };
    }

    // LCS lengths.
    let mut table = vec![vec![0u16; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut lines: Vec<DiffLine> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut truncated = false;

    while i < a.len() || j < b.len() {
        if lines.len() >= MAX_DIFF_LINES {
            truncated = true;
            // Keep counting even once the display stops, so the summary is right.
            while i < a.len() || j < b.len() {
                if i < a.len() && j < b.len() && a[i] == b[j] {
                    i += 1;
                    j += 1;
                } else if j < b.len() && (i >= a.len() || table[i][j + 1] >= table[i + 1][j]) {
                    added += 1;
                    j += 1;
                } else {
                    removed += 1;
                    i += 1;
                }
            }
            break;
        }
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i >= a.len() || table[i][j + 1] >= table[i + 1][j]) {
            lines.push(DiffLine {
                add: true,
                text: clip_line(b[j]),
            });
            added += 1;
            j += 1;
        } else {
            lines.push(DiffLine {
                add: false,
                text: clip_line(a[i]),
            });
            removed += 1;
            i += 1;
        }
    }

    DiffSummary {
        added,
        removed,
        lines,
        truncated,
        before_lines: a.len(),
        after_lines: b.len(),
    }
}

fn clip_line(s: &str) -> String {
    let t = s.trim_end();
    if t.chars().count() <= MAX_DIFF_LINE {
        return t.to_string();
    }
    t.chars().take(MAX_DIFF_LINE).collect::<String>() + "\u{2026}"
}

#[derive(Debug, serde::Serialize)]
pub struct DiffLine {
    pub add: bool,
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
    pub before_lines: usize,
    pub after_lines: usize,
}

impl DiffSummary {
    /// One line for a person, used in the write result and the native dialog.
    pub fn headline(&self) -> String {
        if self.added == 0 && self.removed == 0 {
            return "nothing would change".into();
        }
        let mut bits = Vec::new();
        if self.added > 0 {
            bits.push(format!(
                "{} line{} added",
                self.added,
                if self.added == 1 { "" } else { "s" }
            ));
        }
        if self.removed > 0 {
            bits.push(format!(
                "{} line{} removed",
                self.removed,
                if self.removed == 1 { "" } else { "s" }
            ));
        }
        bits.join(", ")
    }

    /// The diff as text, for the model and for the audit line.
    pub fn as_text(&self) -> String {
        if self.lines.is_empty() {
            return format!(
                "{} ({} lines before, {} after)",
                self.headline(),
                self.before_lines,
                self.after_lines
            );
        }
        let mut out = String::new();
        for l in &self.lines {
            out.push(if l.add { '+' } else { '-' });
            out.push(' ');
            out.push_str(&l.text);
            out.push('\n');
        }
        if self.truncated {
            out.push_str("\u{2026} (diff shortened)\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identical_file_has_no_diff() {
        let d = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!((d.added, d.removed), (0, 0));
        assert!(d.lines.is_empty());
        assert_eq!(d.headline(), "nothing would change");
    }

    #[test]
    fn an_added_line_is_an_addition_and_nothing_else() {
        let d = diff_lines("a\nc", "a\nb\nc");
        assert_eq!((d.added, d.removed), (1, 0));
        assert_eq!(d.lines.len(), 1);
        assert!(d.lines[0].add);
        assert_eq!(d.lines[0].text, "b");
    }

    #[test]
    fn a_removed_line_is_a_removal_and_nothing_else() {
        let d = diff_lines("a\nb\nc", "a\nc");
        assert_eq!((d.added, d.removed), (0, 1));
        assert!(!d.lines[0].add);
        assert_eq!(d.lines[0].text, "b");
    }

    #[test]
    fn a_changed_line_reads_as_one_out_and_one_in() {
        let d = diff_lines("a\nold\nc", "a\nnew\nc");
        assert_eq!((d.added, d.removed), (1, 1));
        assert_eq!(d.lines.len(), 2);
    }

    #[test]
    fn emptying_a_file_shows_every_line_leaving() {
        // The case the whole feature exists for: this must not read as a small edit.
        let before = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = diff_lines(&before, "");
        assert_eq!(d.removed, 30);
        assert_eq!(d.added, 0);
        assert!(
            d.headline().contains("30 lines removed"),
            "{}",
            d.headline()
        );
    }

    #[test]
    fn writing_into_an_empty_file_is_all_additions() {
        let d = diff_lines("", "one\ntwo");
        assert_eq!((d.added, d.removed), (2, 0));
    }

    #[test]
    fn a_huge_file_is_summarised_rather_than_diffed() {
        let big = (0..5000)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = diff_lines(&big, "one line");
        assert!(d.truncated, "should have refused to build the table");
        assert!(d.lines.is_empty(), "should not have produced lines");
        assert!(d.as_text().contains("lines before"), "{}", d.as_text());
    }

    #[test]
    fn a_long_diff_is_shortened_but_still_counted_correctly() {
        let before = (0..1000)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (0..1000)
            .map(|i| format!("b{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = diff_lines(&before, &after);
        assert!(d.truncated, "should be shortened");
        assert!(
            d.lines.len() <= 241,
            "too many lines kept: {}",
            d.lines.len()
        );
        // The counts must be right even though the display stopped early, or the
        // headline would understate what is about to happen.
        assert_eq!(d.added, 1000);
        assert_eq!(d.removed, 1000);
    }

    #[test]
    fn a_very_long_line_is_clipped_not_dropped() {
        let long = "x".repeat(5000);
        let d = diff_lines("", &long);
        assert_eq!(d.added, 1);
        assert!(d.lines[0].text.chars().count() <= MAX_DIFF_LINE + 1);
        assert!(d.lines[0].text.ends_with('\u{2026}'));
    }

    #[test]
    fn the_backup_key_is_stable_and_case_insensitive() {
        let a = key_for(Path::new("C:\\Users\\N\\Documents\\notes.md"));
        let b = key_for(Path::new("c:\\users\\n\\documents\\NOTES.MD"));
        assert_eq!(
            a, b,
            "Windows paths differing only in case are the same file"
        );
        let c = key_for(Path::new("C:\\Users\\N\\Documents\\other.md"));
        assert_ne!(a, c);
        assert!(a.ends_with(".bak"));
    }

    #[test]
    fn the_backup_key_is_always_a_legal_file_name() {
        for p in [
            "C:\\a\\b.txt",
            "~/x",
            "C:\\a b\\c:d",
            "\\\\server\\share\\f",
            "a/b/c",
        ] {
            let k = key_for(Path::new(p));
            assert!(
                !k.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']),
                "{k} is not a legal file name"
            );
        }
    }
}
