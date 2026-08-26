//! The record of what the agent actually did.
//!
//! Two copies, for two different jobs. A bounded ring in memory is what the
//! "This PC" panel reads, so opening it is cheap and does not touch the disk. An
//! append-only JSON-lines file is the one that survives a restart and can be
//! read with any text editor, which is what makes it useful the day something
//! unexpected happened and the app has since been closed.
//!
//! Every entry is written by the command that ran, after it ran, with the
//! outcome — including refusals. Refusals are the interesting ones: a run of
//! "outside the folders Compass may use" is what an injection attempt looks like
//! from in here.
//!
//! The log lives in the app's config directory, which the path guard denies to
//! every file tool. The agent cannot read its own audit trail, and it cannot
//! quietly edit it either.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many entries the panel can show without reading the file.
const RING: usize = 200;

/// Stop the file growing without bound on a machine that is used heavily.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Milliseconds since the epoch. The frontend turns this into "5 min ago";
    /// keeping it numeric avoids a date library here and a parser there.
    pub at: u64,
    /// The tool as the model named it, e.g. `win.delete_file`.
    pub tool: String,
    pub ok: bool,
    /// One line, already written for a person to read.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether Windows put a confirmation dialog in front of this one.
    #[serde(default)]
    pub confirmed: bool,
}

pub struct Audit {
    ring: Mutex<VecDeque<Entry>>,
    file: Option<PathBuf>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Audit {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let file = dir.map(|d| {
            let _ = std::fs::create_dir_all(&d);
            d.join("agent-audit.jsonl")
        });
        let mut me = Self {
            ring: Mutex::new(VecDeque::with_capacity(RING)),
            file,
        };
        me.warm();
        me
    }

    /// Load the tail of the file so the panel is not empty after a restart.
    fn warm(&mut self) {
        let Some(f) = self.file.clone() else { return };
        let Ok(text) = std::fs::read_to_string(&f) else {
            return;
        };
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        for line in text.lines().rev().take(RING) {
            if let Ok(e) = serde_json::from_str::<Entry>(line) {
                ring.push_front(e);
            }
        }
    }

    pub fn record(
        &self,
        tool: &str,
        ok: bool,
        summary: String,
        error: Option<String>,
        confirmed: bool,
    ) {
        let entry = Entry {
            at: now_ms(),
            tool: tool.to_string(),
            ok,
            summary,
            error,
            confirmed,
        };

        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() == RING {
                ring.pop_back();
            }
            ring.push_front(entry.clone());
        }

        // A failure to write the log must never fail the tool that succeeded, so
        // this is deliberately best-effort and silent.
        if let Some(f) = &self.file {
            self.rotate_if_needed(f);
            if let Ok(line) = serde_json::to_string(&entry) {
                if let Ok(mut h) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(f)
                {
                    let _ = writeln!(h, "{line}");
                }
            }
        }
    }

    /// Keep one previous generation, so rotation never destroys the only copy of
    /// a recent event.
    fn rotate_if_needed(&self, f: &PathBuf) {
        let too_big = std::fs::metadata(f)
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(false);
        if too_big {
            let _ = std::fs::rename(f, f.with_extension("jsonl.1"));
        }
    }

    /// Newest first, which is the order the panel wants.
    pub fn recent(&self, limit: usize) -> Vec<Entry> {
        let limit = limit.clamp(1, RING);
        match self.ring.lock() {
            Ok(ring) => ring.iter().take(limit).cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}
