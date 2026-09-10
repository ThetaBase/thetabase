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

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    pub fn open(dir: &Path, project_id: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("audit.jsonl");

        let (recent, total) = match path.exists() {
            true => Self::read_existing(&path, project_id)?,
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
            recent,
            total,
        })
    }

    /// Read an existing log, checking it belongs to this project.
    fn read_existing(path: &Path, project_id: &str) -> Result<(VecDeque<AuditEntry>, u64)> {
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
            // A half-written final line is what a crash mid-append leaves. Stop
            // there and keep everything before it: a truncated tail loses the
            // last event, where refusing to open loses the whole trail.
            let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) else {
                break;
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
