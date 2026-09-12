//! Persistent, per-project audit store (`04-threat-model-security.md` §5).
//!
//! The audit trail is the forensic record: "what happened, and who or what did
//! it". Held only in memory it answers that question until the process restarts,
//! which is exactly when someone is most likely to be asking.
//!
//! # Append-only
//!
//! There is no method here that rewrites or removes an entry. The file is
//! opened in append mode, so every write goes to the end and no seek-and-
//! overwrite path exists to reach for. A trail that can be edited is not
//! evidence of anything.
//!
//! # Per-project by construction
//!
//! The store is opened on one project's data directory and stamps that project
//! into the file's header. Reopening a file whose header names a different
//! project fails — so a misconfigured data directory is a loud startup error
//! rather than two projects' forensic trails quietly interleaved. This mirrors
//! the isolation rule the rest of the daemon follows: no code path accepts work
//! spanning two project identifiers (`04-threat-model-security.md` §3).

use base64::Engine as _;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use theta_core::DataKey;

use serde::{Deserialize, Serialize};

use crate::audit::{AuditEntry, RiskLevel};

/// Entries kept in memory for fast reads.
///
/// A weekly review reads the recent tail; anything older is a forensic query
/// that can afford to touch the disk.
const RECENT_CAPACITY: usize = 512;

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum AuditStoreError {
    #[error("audit store i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("audit log is not readable as an audit log: {0}")]
    Malformed(String),

    /// A sealed entry that will not open under the key this store was given.
    ///
    /// Reported as a wrong key rather than as corruption, and never treated as
    /// a truncated tail. The reader stops at a half-written line because a
    /// crash mid-append leaves one; doing that here would mean starting with
    /// the wrong key silently produced an empty audit trail that looked
    /// complete.
    #[error(
        "an audit entry in {path} will not open under this key; \
         the log is sealed and {}",
        "either the key is wrong or it belongs to another project"
    )]
    WrongKey { path: std::path::PathBuf },

    #[error(
        "audit log at {path} belongs to project `{found}`, but this instance serves `{expected}`"
    )]
    WrongProject {
        path: PathBuf,
        found: String,
        expected: String,
    },
}

type Result<T> = std::result::Result<T, AuditStoreError>;

/// First line of the file: what it is, and whose it is.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    thetabase_audit_log: u32,
    project_id: String,
}

pub struct AuditStore {
    path: PathBuf,
    project_id: String,
    file: File,
    /// The key new entries are sealed with, if this deployment has one.
    ///
    /// `None` means the log is written in clear, which is what every
    /// deployment did before this existed and what one without
    /// `THETA_DATA_KEY` still does. The engine says so at startup, at `warn`,
    /// the same way it does for the segment store.
    key: Option<DataKey>,
    recent: VecDeque<AuditEntry>,
    total: u64,
}

impl std::fmt::Debug for AuditStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditStore")
            .field("path", &self.path)
            .field("project_id", &self.project_id)
            .field("total", &self.total)
            .finish()
    }
}

impl AuditStore {
    /// Open (or create) the audit log for one project under `dir`.
    ///
    /// Unsealed. Kept so the many callers that never had a key do not all grow
    /// a `None`, and so the clear-text behaviour stays the thing you get by
    /// asking for it rather than by forgetting something.
    pub fn open(dir: &Path, project_id: &str) -> Result<Self> {
        Self::open_with_key(dir, project_id, None)
    }

    /// Open (or create) the audit log, sealing new entries under `key`.
    ///
    /// An existing log does not have to be all one thing. Entries already
    /// written in clear stay readable and new ones are sealed, so encryption
    /// can be switched on for a project that has been running -- the same
    /// property the segment store has, for the same reason.
    pub fn open_with_key(dir: &Path, project_id: &str, key: Option<DataKey>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("audit.jsonl");

        let (recent, total) = match path.exists() {
            true => Self::read_existing(&path, project_id, key.as_ref())?,
            false => {
                let mut file = File::create(&path)?;
                let header = Header {
                    thetabase_audit_log: FORMAT_VERSION,
                    project_id: project_id.to_string(),
                };
                writeln!(file, "{}", serde_json::to_string(&header).expect("header"))?;
                file.sync_all()?;
                (VecDeque::new(), 0)
            }
        };

        let file = OpenOptions::new().append(true).open(&path)?;

        Ok(Self {
            path,
            project_id: project_id.to_string(),
            file,
            key,
            recent,
            total,
        })
    }

    /// Read an existing log, checking it belongs to this project.
    fn read_existing(
        path: &Path,
        project_id: &str,
        key: Option<&DataKey>,
    ) -> Result<(VecDeque<AuditEntry>, u64)> {
        let reader = BufReader::new(File::open(path)?);
        let mut lines = reader.lines();

        let header_line = lines
            .next()
            .transpose()?
            .ok_or_else(|| AuditStoreError::Malformed("the file is empty".into()))?;
        let header: Header = serde_json::from_str(&header_line)
            .map_err(|e| AuditStoreError::Malformed(format!("unreadable header: {e}")))?;

        if header.project_id != project_id {
            return Err(AuditStoreError::WrongProject {
                path: path.to_path_buf(),
                found: header.project_id,
                expected: project_id.to_string(),
            });
        }

        let mut recent = VecDeque::new();
        let mut total = 0;
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry = match Self::read_line(&line, key) {
                // A half-written final line is what a crash mid-append leaves.
                // Stop there and keep everything before it: a truncated tail
                // loses the last event, where refusing to open loses the whole
                // trail.
                LineOutcome::Unreadable => break,
                LineOutcome::Entry(entry) => *entry,
                // Not a truncation. A sealed line that will not decrypt means
                // the key is wrong, and continuing would hand back a short
                // trail that looks complete.
                LineOutcome::WrongKey => {
                    return Err(AuditStoreError::WrongKey {
                        path: path.to_path_buf(),
                    })
                }
            };
            total += 1;
            recent.push_back(entry);
            if recent.len() > RECENT_CAPACITY {
                recent.pop_front();
            }
        }
        Ok((recent, total))
    }

    /// Record one entry, durably.
    ///
    /// Synced before returning. Audit entries are written only on gated events —
    /// rare by construction — so the cost is irrelevant and losing the record of
    /// the event that preceded a crash is not.
    pub fn append(&mut self, entry: AuditEntry) -> Result<()> {
        let line = serde_json::to_string(&entry)
            .map_err(|e| AuditStoreError::Malformed(format!("unserializable entry: {e}")))?;
        let line = match &self.key {
            Some(key) => Self::seal_line(key, &line)?,
            None => line,
        };
        writeln!(self.file, "{line}")?;
        self.file.sync_all()?;

        self.total += 1;
        self.recent.push_back(entry);
        if self.recent.len() > RECENT_CAPACITY {
            self.recent.pop_front();
        }
        Ok(())
    }

    /// The most recent entries, newest last. Served from memory.
    pub fn recent(&self) -> impl Iterator<Item = &AuditEntry> {
        self.recent.iter()
    }

    /// Entries at or above `floor`, most severe first, then newest first.
    ///
    /// The shape a weekly review wants: the worst thing that happened, at the
    /// top, without reading anything else (`07-agent-safety-layer.md` §8).
    pub fn review(&self, floor: RiskLevel, limit: usize) -> Vec<&AuditEntry> {
        let mut entries: Vec<&AuditEntry> =
            self.recent.iter().filter(|e| e.risk >= floor).collect();
        entries.sort_by(|a, b| {
            b.risk
                .cmp(&a.risk)
                .then(b.timestamp_ms.cmp(&a.timestamp_ms))
        });
        entries.truncate(limit);
        entries
    }

    /// Every entry ever written, read from disk.
    ///
    /// For forensics rather than review: this is the call that answers "what
    /// happened three months ago", and it is allowed to be slow.
    pub fn read_all(&self) -> Result<Vec<AuditEntry>> {
        let (_, _) = (&self.path, &self.project_id);
        let reader = BufReader::new(File::open(&self.path)?);
        let mut out = Vec::new();
        for line in reader.lines().skip(1) {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) else {
                break;
            };
            out.push(entry);
        }
        Ok(out)
    }

    /// How many entries this log holds.
    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }
}

/// What one line of the log turned out to be.
enum LineOutcome {
    Entry(Box<AuditEntry>),
    /// Not parseable at all -- a half-written tail from a crash mid-append.
    Unreadable,
    /// Sealed, and the key will not open it.
    WrongKey,
}

/// The wrapper a sealed entry is written as.
///
/// An object rather than bare base64, so the file is still JSONL and anything
/// reading it line by line still parses. Bare base64 would have discriminated
/// perfectly well -- it cannot begin with `{` -- and would have broken every
/// tool that assumes a line is an object.
#[derive(serde::Serialize, serde::Deserialize)]
struct SealedLine {
    sealed: String,
}

impl AuditStore {
    /// Seal one serialised entry into the line that gets written.
    fn seal_line(key: &DataKey, line: &str) -> Result<String> {
        let sealed = key
            .seal(line.as_bytes())
            .map_err(|e| AuditStoreError::Malformed(format!("could not seal an entry: {e}")))?;
        let wrapper = SealedLine {
            sealed: base64::engine::general_purpose::STANDARD.encode(sealed),
        };
        serde_json::to_string(&wrapper)
            .map_err(|e| AuditStoreError::Malformed(format!("unserializable entry: {e}")))
    }

    /// Read one line, sealed or clear.
    ///
    /// Clear lines are accepted whether or not a key is configured: a log that
    /// predates encryption is still this project's audit trail, and refusing to
    /// read it would make turning encryption on destroy the history it is meant
    /// to protect.
    fn read_line(line: &str, key: Option<&DataKey>) -> LineOutcome {
        if let Ok(wrapper) = serde_json::from_str::<SealedLine>(line) {
            let Some(key) = key else {
                // Sealed, and this store has no key. Indistinguishable from the
                // wrong key as far as the reader is concerned, and the same
                // answer: not a truncation.
                return LineOutcome::WrongKey;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&wrapper.sealed)
            else {
                return LineOutcome::Unreadable;
            };
            let Ok(plain) = key.open(&bytes) else {
                return LineOutcome::WrongKey;
            };
            return match serde_json::from_slice::<AuditEntry>(&plain) {
                Ok(entry) => LineOutcome::Entry(Box::new(entry)),
                // Opened under the key and still not an entry: the key is
                // right and the content is wrong, which is corruption.
                Err(_) => LineOutcome::Unreadable,
            };
        }

        match serde_json::from_str::<AuditEntry>(line) {
            Ok(entry) => LineOutcome::Entry(Box::new(entry)),
            Err(_) => LineOutcome::Unreadable,
        }
    }
}

#[cfg(test)]
mod tests {
    use theta_core::Author;

    use super::*;

    fn entry(risk: RiskLevel, at: i64, summary: &str) -> AuditEntry {
        AuditEntry {
            risk,
            summary: summary.into(),
            author: Author::System,
            timestamp_ms: at,
            detail: serde_json::json!({}),
        }
    }

    #[test]
    fn entries_survive_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut store = AuditStore::open(dir.path(), "project-a").expect("open");
        store
            .append(entry(RiskLevel::High, 1, "agent tried to drop users.email"))
            .expect("append");
        drop(store);

        // The question the trail exists to answer is usually asked *after* a
        // restart.
        let reopened = AuditStore::open(dir.path(), "project-a").expect("reopen");
        assert_eq!(reopened.len(), 1);
        assert_eq!(
            reopened.recent().next().map(|e| e.summary.as_str()),
            Some("agent tried to drop users.email")
        );
    }

    #[test]
    fn a_log_belonging_to_another_project_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        AuditStore::open(dir.path(), "project-a").expect("open");

        // Cross-project isolation is structural: a data directory pointed at
        // the wrong project is a loud startup failure, not two forensic trails
        // quietly interleaved (`04-threat-model-security.md` §3).
        let wrong = AuditStore::open(dir.path(), "project-b");
        assert!(
            matches!(wrong, Err(AuditStoreError::WrongProject { .. })),
            "another project's audit log was opened: {wrong:?}"
        );
    }

    #[test]
    fn appending_never_rewrites_what_is_already_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuditStore::open(dir.path(), "p").expect("open");
        for i in 0..10 {
            store
                .append(entry(RiskLevel::Info, i, &format!("event {i}")))
                .expect("append");
        }

        let all = store.read_all().expect("read");
        assert_eq!(all.len(), 10);
        for (i, e) in all.iter().enumerate() {
            assert_eq!(e.summary, format!("event {i}"), "entries were reordered");
        }
    }

    #[test]
    fn a_review_puts_the_worst_thing_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuditStore::open(dir.path(), "p").expect("open");
        store.append(entry(RiskLevel::Info, 100, "info")).unwrap();
        store.append(entry(RiskLevel::High, 1, "high")).unwrap();
        store.append(entry(RiskLevel::Low, 50, "low")).unwrap();

        let review = store.review(RiskLevel::Info, 10);
        assert_eq!(review[0].summary, "high", "the worst entry was not first");
    }

    #[test]
    fn a_review_can_ignore_everything_below_a_risk_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuditStore::open(dir.path(), "p").expect("open");
        store.append(entry(RiskLevel::Info, 1, "info")).unwrap();
        store.append(entry(RiskLevel::High, 2, "high")).unwrap();

        let review = store.review(RiskLevel::Medium, 10);
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].summary, "high");
    }

    #[test]
    fn a_torn_final_line_costs_one_entry_and_not_the_whole_trail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuditStore::open(dir.path(), "p").expect("open");
        store.append(entry(RiskLevel::High, 1, "first")).unwrap();
        store.append(entry(RiskLevel::High, 2, "second")).unwrap();
        let path = store.path().to_path_buf();
        drop(store);

        // What a crash mid-append leaves behind.
        let mut raw = std::fs::read_to_string(&path).expect("read");
        raw.push_str("{\"risk\":\"high\",\"summ");
        std::fs::write(&path, raw).expect("write");

        let reopened = AuditStore::open(dir.path(), "p").expect("a torn tail must still open");
        assert_eq!(reopened.len(), 2);
    }

    #[test]
    fn the_memory_footprint_stays_bounded_however_long_the_log_gets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuditStore::open(dir.path(), "p").expect("open");
        for i in 0..(RECENT_CAPACITY as i64 + 200) {
            store.append(entry(RiskLevel::Info, i, "e")).unwrap();
        }

        assert_eq!(store.recent().count(), RECENT_CAPACITY);
        // But nothing was lost from the record itself.
        assert_eq!(store.read_all().expect("read").len(), RECENT_CAPACITY + 200);
    }
}
