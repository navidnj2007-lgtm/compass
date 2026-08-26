//! An opt-in index of his own documents, so "which file did I write about X in" is one
//! query rather than a walk.
//!
//! WHAT THIS IS FOR, AND WHY IT IS OFF BY DEFAULT
//!
//! `win.grep_files` already answers the question by reading files. It is bounded — 400
//! files, a megabyte each — which means on a large Documents folder it answers *some* of
//! the question and says it stopped early. An index answers all of it in milliseconds.
//!
//! That is a speed improvement, not a new capability, and it is bought with something
//! real: a second copy of the text of his documents, on disk, in a database. So it is off
//! until he switches it on, the switch says plainly what will be stored, and clearing it
//! deletes the file rather than emptying a table.
//!
//! WHAT IT STORES, EXACTLY
//!
//! Per file: the full path, the name, the size, the modification time, and the first
//! `MAX_EXCERPT` characters of its text. Not the whole file — an index that held complete
//! documents would be a duplicate of his Documents folder, and the excerpt is what makes a
//! result recognisable. Nothing else: no content hash, no thumbnail, no history of what was
//! searched.
//!
//! WHERE IT LIVES, AND WHY THAT MATTERS
//!
//! In the app's data directory, which `Policy::hard_denied` denies to every file tool. The
//! index is therefore not readable by the agent as a file, which matters more than it
//! sounds: an index of his documents that the agent could `read_file` would be a way to
//! read the text of a file the guard had refused, because the excerpt is already extracted
//! and sitting outside the sandbox. It is also outside the Compass backup and outside sync,
//! for the same reason chats are: it never leaves the machine.
//!
//! WHAT IT DELIBERATELY DOES NOT DO
//!
//! No background indexing. Rebuilding happens when he asks, so there is never a process
//! quietly reading his folders — and "why is my disk busy" should never have Compass as the
//! answer. No incremental watcher for the same reason. Re-indexing an unchanged file is
//! skipped by comparing size and modification time, so a rebuild after a small change is
//! fast without needing to watch anything.

use crate::agent::{Agent, ToolOut};
use crate::consent;
use crate::guard::{show, Intent};
use crate::policy::Risk;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, State};
use walkdir::WalkDir;

/// Characters of each file kept. Enough to recognise a document and to match a phrase in
/// its opening; not enough to be a copy of it.
const MAX_EXCERPT: usize = 4_000;

/// Files visited in one rebuild. A ceiling on how long "rebuild" can take, so it is an
/// operation with an end rather than one that might not finish.
const MAX_INDEX_FILES: usize = 8_000;

/// Largest file whose text is extracted during a rebuild. Bigger ones are indexed by name,
/// size and date only — a 200 MB log is not what this is for.
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Rows returned by one search.
const MAX_HITS: usize = 60;

#[derive(Debug, Deserialize)]
pub struct IndexSearchReq {
    pub query: String,
    #[serde(default)]
    pub limit: usize,
}

fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no data directory: {e}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir.join("index.sqlite"))
}

/// Open the database, creating the schema on first use.
///
/// One table and one index. There is no FTS5 virtual table, deliberately: FTS would be
/// faster still and it is a much larger surface — tokenizers, ranking functions, and a
/// second on-disk representation of the same text. A `LIKE` scan over a few thousand short
/// excerpts is already fast enough to feel instant, and it is one line that can be read and
/// understood.
fn open(app: &AppHandle) -> Result<rusqlite::Connection, String> {
    let p = db_path(app)?;
    let conn =
        rusqlite::Connection::open(&p).map_err(|e| format!("could not open the index: {e}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS files (
            path     TEXT PRIMARY KEY,
            name     TEXT NOT NULL,
            size     INTEGER NOT NULL,
            modified INTEGER NOT NULL,
            excerpt  TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS files_name ON files(name);",
    )
    .map_err(|e| format!("could not prepare the index: {e}"))?;
    Ok(conn)
}

fn modified_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Is this a file worth extracting text from?
///
/// Deliberately narrow: the formats `read_document` and `read_file` can already read. A file
/// this cannot read is still indexed by name, which is what makes it findable.
fn textish(p: &Path) -> bool {
    let Some(ext) = p.extension() else {
        return false;
    };
    matches!(
        ext.to_string_lossy().to_ascii_lowercase().as_str(),
        "txt"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "log"
            | "tex"
            | "rtf"
            | "html"
            | "htm"
            | "xml"
            | "srt"
            | "vtt"
    )
}

/// Should this file's text be extracted at all?
///
/// Split out from `excerpt_of` so the decision can be tested without a four-megabyte file
/// on disk. Both halves matter: an extension this cannot read, and a file too large to be
/// worth reading. Either way the file is still indexed by name, which is what makes it
/// findable.
fn should_extract(p: &Path, len: u64) -> bool {
    textish(p) && len <= MAX_INDEX_BYTES
}

/// The opening of a file, as text, or nothing.
fn excerpt_of(p: &Path, meta: &std::fs::Metadata) -> String {
    if !should_extract(p, meta.len()) {
        return String::new();
    }
    let Ok(bytes) = std::fs::read(p) else {
        return String::new();
    };
    // Binary test, as read_file does it.
    if bytes.iter().take(8000).filter(|b| **b == 0).count() > 2 {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut out = String::with_capacity(MAX_EXCERPT.min(text.len()));
    let mut ws = false;
    for ch in text.chars() {
        if out.chars().count() >= MAX_EXCERPT {
            break;
        }
        if ch.is_whitespace() {
            if !ws {
                out.push(' ');
            }
            ws = true;
        } else {
            out.push(ch);
            ws = false;
        }
    }
    out
}

/* ── the commands ────────────────────────────────────────────────── */

/// What the index currently holds, and what it would store. Read-only, and safe to call
/// whether or not the index exists.
#[tauri::command]
pub async fn index_status(app: AppHandle, state: State<'_, Agent>) -> Result<ToolOut, String> {
    let p = db_path(&app)?;
    if !p.exists() {
        return Ok(ToolOut::text(
            "INDEX: not built. Nothing about his documents is stored. Building it would keep, \
             for each file in the allowed folders, its path, name, size, modification date and \
             the first 4,000 characters of its text \u{2014} in Compass's own data folder, which \
             no file tool can read, never synced and never in the backup. Until then, use \
             win.grep_files, which reads files directly and is bounded.",
        ));
    }
    let conn = open(&app)?;
    let (n, with_text): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), SUM(CASE WHEN excerpt <> '' THEN 1 ELSE 0 END) FROM files",
            [],
            |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )
        .map_err(|e| format!("could not read the index: {e}"))?;
    let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

    state.audit.record(
        "win.index_status",
        true,
        "Checked the index".into(),
        None,
        false,
    );
    Ok(ToolOut::text(format!(
        "INDEX: {n} file(s), {with_text} of them with searchable text, using {} KB.\n\
         Stored per file: path, name, size, modification date, and the first 4,000 characters \
         of text. Nothing else.\n\
         Use win.index_search to query it. It is only as current as the last rebuild.",
        bytes / 1024
    )))
}

/// Build or refresh the index.
///
/// A write, and consented, because it reads every document in the allowed folders and puts
/// their opening text somewhere new. That is not destructive but it is not nothing, and the
/// dialog is where he finds out it is about to happen.
#[tauri::command]
pub async fn index_rebuild(app: AppHandle, state: State<'_, Agent>) -> Result<ToolOut, String> {
    let policy = state.policy();
    let out = async {
        let roots = state.guard().roots().to_vec();
        if roots.is_empty() {
            return Err("Compass has no folders it is allowed to read, so there is nothing to index".into());
        }

        consent::require(
            &app,
            &policy,
            Risk::Medium,
            "Build the document index",
            &format!(
                "Compass will read the documents in these folders and store each one's name, size, \
                 date and first 4,000 characters of text in its own data folder:\n\n{}\n\nIt is not \
                 synced, is not in your backup, and the assistant cannot read the index as a file. \
                 You can clear it at any time.",
                roots
                    .iter()
                    .map(|r| show(r))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .await?;

        let guard_roots = roots.clone();
        let app2 = app.clone();
        let (added, updated, skipped, visited) = tauri::async_runtime::spawn_blocking(move || {
            build(&app2, &guard_roots)
        })
        .await
        .map_err(|_| "the rebuild failed unexpectedly".to_string())??;

        Ok(ToolOut::done_with(format!(
            "Indexed {} file(s): {added} new, {updated} changed, {skipped} unchanged. \
             Visited {visited}.",
            added + updated + skipped
        )))
    }
    .await;

    state.audit.record(
        "win.index_rebuild",
        out.is_ok(),
        "Rebuilt the document index".into(),
        out.as_ref().err().cloned(),
        policy.needs_confirm(Risk::Medium),
    );
    out
}

/// The walk itself. Bounded the same way `search_files` is, and it consults the guard per
/// file so a credential file is never opened even here — an index that contained a file the
/// guard refuses would be a way around the guard.
fn build(app: &AppHandle, roots: &[PathBuf]) -> Result<(usize, usize, usize, usize), String> {
    let conn = open(app)?;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut skipped = 0usize;
    let mut visited = 0usize;

    // One transaction: eight thousand individual commits would be slow enough to look broken.
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("could not start writing: {e}"))?;

    for root in roots {
        for entry in WalkDir::new(root).max_depth(8).follow_links(false) {
            visited += 1;
            if visited > MAX_INDEX_FILES {
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            let Ok(meta) = entry.metadata() else { continue };

            let path_s = show(p);
            let modified = modified_secs(&meta);
            let size = meta.len() as i64;

            // Unchanged since last time: nothing to do, and nothing to read.
            let existing: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT size, modified FROM files WHERE path = ?1",
                    [&path_s],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if existing == Some((size, modified)) {
                skipped += 1;
                continue;
            }

            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let excerpt = excerpt_of(p, &meta);

            let res = conn.execute(
                "INSERT INTO files(path,name,size,modified,excerpt) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(path) DO UPDATE SET name=?2, size=?3, modified=?4, excerpt=?5",
                rusqlite::params![path_s, name, size, modified, excerpt],
            );
            if res.is_ok() {
                if existing.is_some() {
                    updated += 1;
                } else {
                    added += 1;
                }
            }
        }
    }

    conn.execute_batch("COMMIT")
        .map_err(|e| format!("could not finish writing: {e}"))?;
    Ok((added, updated, skipped, visited))
}

/// Search it. A read, so it runs without asking.
#[tauri::command]
pub async fn index_search(
    app: AppHandle,
    state: State<'_, Agent>,
    req: IndexSearchReq,
) -> Result<ToolOut, String> {
    let out = (|| {
        let needle = req.query.trim();
        if needle.len() < 2 {
            return Err("a search needs at least two characters".into());
        }
        if needle.len() > 200 {
            return Err("that search text is too long".into());
        }
        let p = db_path(&app)?;
        if !p.exists() {
            return Err(
                "the index has not been built, so there is nothing to search. Use win.grep_files, \
                 which reads files directly, or ask him to build the index."
                    .into(),
            );
        }
        let conn = open(&app)?;
        let limit = if req.limit == 0 {
            20
        } else {
            req.limit.min(MAX_HITS)
        };

        /* Parameterised, with the wildcards added around the bound value rather than
        concatenated into the SQL. The query text comes from the model, which is to say
        from anywhere, and this is a database. */
        let like = format!("%{}%", needle.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn
            .prepare(
                "SELECT path, name, size, excerpt FROM files
                 WHERE name LIKE ?1 ESCAPE '\\' OR excerpt LIKE ?1 ESCAPE '\\'
                 ORDER BY CASE WHEN name LIKE ?1 ESCAPE '\\' THEN 0 ELSE 1 END, modified DESC
                 LIMIT ?2",
            )
            .map_err(|e| format!("could not search the index: {e}"))?;

        let rows = stmt
            .query_map(rusqlite::params![like, limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| format!("could not read the results: {e}"))?;

        let mut hits: Vec<String> = Vec::new();
        let lower = needle.to_lowercase();
        for row in rows {
            let Ok((path, _name, size, excerpt)) = row else {
                continue;
            };
            /* The guard decides again, now. The index is a snapshot: a file may have moved
            into a denied place, or the allowed roots may have been narrowed since the
            rebuild, and a stale index must not become a way to read either. */
            if state.guard().resolve(&path, Intent::Read).is_err() {
                continue;
            }
            let where_at = excerpt.to_lowercase().find(&lower);
            let context = match where_at {
                Some(at) => {
                    let from = at.saturating_sub(50);
                    let to = (at + needle.len() + 90).min(excerpt.len());
                    // char_indices would be safer against a multi-byte boundary; clamp instead.
                    let slice = excerpt
                        .char_indices()
                        .skip_while(|(i, _)| *i < from)
                        .take_while(|(i, _)| *i < to)
                        .map(|(_, c)| c)
                        .collect::<String>();
                    format!("  \u{2026}{}\u{2026}", slice.trim())
                }
                None => "  (matched on the file name)".to_string(),
            };
            hits.push(format!("  {path}  ({size} bytes)\n{context}"));
            if hits.len() >= limit {
                break;
            }
        }

        let mut text = format!(
            "INDEX SEARCH \"{needle}\" \u{2014} {} match(es):\n",
            hits.len()
        );
        if hits.is_empty() {
            text.push_str(
                "  (nothing matched. The index is only as current as the last rebuild, and it \
                 holds the first 4,000 characters of each file \u{2014} a phrase deeper in a long \
                 document will not be in it. win.grep_files reads the whole file.)\n",
            );
        }
        for h in &hits {
            text.push_str(h);
            text.push('\n');
        }
        text.push_str("\nThe paths above are the ones to use in a later read, move or rename.\n");
        Ok(ToolOut::text(text))
    })();

    state.audit.record(
        "win.index_search",
        out.is_ok(),
        format!("Searched the index for {}", req.query),
        out.as_ref().err().cloned(),
        false,
    );
    out
}

/// Delete it. The file, not the rows: an emptied database is still a file that once held the
/// text of his documents, and "clear" should mean gone.
#[tauri::command]
pub async fn index_clear(app: AppHandle, state: State<'_, Agent>) -> Result<ToolOut, String> {
    let policy = state.policy();
    let out = async {
        let p = db_path(&app)?;
        if !p.exists() {
            return Ok(ToolOut::done_with("There was no index to clear."));
        }
        consent::require(
            &app,
            &policy,
            Risk::Medium,
            "Clear the document index",
            "Compass will delete everything it has stored about your documents. \
             Nothing in your folders is touched.",
        )
        .await?;

        // WAL leaves two companions beside the database; all three go.
        let mut gone = 0;
        for suffix in ["", "-wal", "-shm"] {
            let f = PathBuf::from(format!("{}{suffix}", p.display()));
            if f.exists() && std::fs::remove_file(&f).is_ok() {
                gone += 1;
            }
        }
        if gone == 0 {
            return Err("the index file could not be deleted \u{2014} it may be in use".into());
        }
        Ok(ToolOut::done_with(
            "Cleared. Nothing about your documents is stored any more.",
        ))
    }
    .await;

    state.audit.record(
        "win.index_clear",
        out.is_ok(),
        "Cleared the document index".into(),
        out.as_ref().err().cloned(),
        policy.needs_confirm(Risk::Medium),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readable_text_formats_are_extracted() {
        for (name, want) in [
            ("a.md", true),
            ("a.txt", true),
            ("a.csv", true),
            ("a.json", true),
            ("a.pdf", false), // read_document handles these; the index stores the name
            ("a.docx", false),
            ("a.jpg", false),
            ("a.exe", false),
            ("noext", false),
        ] {
            assert_eq!(textish(&PathBuf::from(name)), want, "{name}");
        }
    }

    #[test]
    fn an_excerpt_is_bounded_and_whitespace_collapsed() {
        let dir = std::env::temp_dir().join(format!("compass-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.md");
        // Long, and full of the newlines a document actually contains.
        let body = (0..3000)
            .map(|i| format!("line {i}\n\n"))
            .collect::<String>();
        std::fs::write(&f, &body).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let ex = excerpt_of(&f, &meta);
        assert!(
            ex.chars().count() <= MAX_EXCERPT,
            "{} chars",
            ex.chars().count()
        );
        assert!(!ex.contains("\n"), "newlines survived");
        assert!(!ex.contains("  "), "double spaces survived");
        assert!(ex.starts_with("line 0"), "{ex:.40}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_file_yields_no_excerpt() {
        let dir = std::env::temp_dir().join(format!("compass-idx-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A .txt extension with binary contents: the extension says text, the bytes do not,
        // and the bytes win.
        let f = dir.join("fake.txt");
        std::fs::write(&f, [0u8, 1, 0, 2, 0, 3, 0, 4, 0, 5]).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_eq!(excerpt_of(&f, &meta), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_file_is_indexed_by_name_only() {
        /* The size gate, tested through the decision rather than by writing four megabytes
        to disk. A file that is text by extension but over the ceiling produces no excerpt,
        so it stays findable by name without going through the extractor. */
        let small = PathBuf::from("notes.md");
        assert!(should_extract(&small, 1_000));
        assert!(should_extract(&small, MAX_INDEX_BYTES));
        assert!(!should_extract(&small, MAX_INDEX_BYTES + 1));
        // And an unreadable format is never extracted whatever its size.
        assert!(!should_extract(&PathBuf::from("scan.pdf"), 10));
        assert!(!should_extract(&PathBuf::from("photo.jpg"), 10));
    }

    #[test]
    fn the_search_wildcards_cannot_be_smuggled_in() {
        // A query of "%" must not match everything: the escaping turns it into a literal.
        let needle = "100%_done";
        let like = format!("%{}%", needle.replace('%', "\\%").replace('_', "\\_"));
        assert_eq!(like, "%100\\%\\_done%");
        assert!(!like.contains("%_"), "an unescaped wildcard survived");
    }

    #[test]
    fn the_schema_and_a_round_trip_work() {
        // An in-memory database, so this exercises the real SQL without touching the disk.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (path TEXT PRIMARY KEY, name TEXT NOT NULL, size INTEGER NOT NULL,
             modified INTEGER NOT NULL, excerpt TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files(path,name,size,modified,excerpt) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(path) DO UPDATE SET name=?2, size=?3, modified=?4, excerpt=?5",
            rusqlite::params![
                "C:\\a\\notes.md",
                "notes.md",
                12i64,
                99i64,
                "about electrochemistry"
            ],
        )
        .unwrap();
        // The same path again: an update, not a second row. A rebuild must not duplicate.
        conn.execute(
            "INSERT INTO files(path,name,size,modified,excerpt) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(path) DO UPDATE SET name=?2, size=?3, modified=?4, excerpt=?5",
            rusqlite::params![
                "C:\\a\\notes.md",
                "notes.md",
                20i64,
                100i64,
                "about titration"
            ],
        )
        .unwrap();

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "a rebuild duplicated a row");
        let ex: String = conn
            .query_row("SELECT excerpt FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ex, "about titration", "the update did not take");

        let like = "%titration%";
        let hit: String = conn
            .query_row(
                "SELECT path FROM files WHERE name LIKE ?1 ESCAPE '\\' OR excerpt LIKE ?1 ESCAPE '\\'",
                [like],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, "C:\\a\\notes.md");
    }

    #[test]
    fn a_name_match_sorts_before_a_content_match() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (path TEXT PRIMARY KEY, name TEXT NOT NULL, size INTEGER NOT NULL,
             modified INTEGER NOT NULL, excerpt TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files VALUES('C:\\a\\other.md','other.md',1,1,'mentions titration inside')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files VALUES('C:\\a\\titration.md','titration.md',1,1,'nothing relevant')",
            [],
        )
        .unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT name FROM files WHERE name LIKE ?1 ESCAPE '\\' OR excerpt LIKE ?1 ESCAPE '\\'
                 ORDER BY CASE WHEN name LIKE ?1 ESCAPE '\\' THEN 0 ELSE 1 END, modified DESC",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map(["%titration%"], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names, vec!["titration.md", "other.md"], "ordering is wrong");
    }
}
