// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::environment::Environment;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One line of the append-only workflow log (`.dev_docs/CONTEXT.md`, *Workflow log*): the only
/// place an observer reads a Run from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

pub struct EventLog {
    dir: Option<PathBuf>,
}

impl EventLog {
    pub fn open(env: &dyn Environment) -> Self {
        Self {
            dir: crate::environment::log_dir(env),
        }
    }

    pub fn for_dir(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    /// Where the log is kept, when it is kept at all.
    pub fn dir(&self) -> Option<&std::path::Path> {
        self.dir.as_deref()
    }

    /// Appends one entry. A missing log directory disables logging without failing the
    /// caller's command (`environment::log_dir`'s contract).
    pub fn record(&self, entry: &LogEntry) -> Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.jsonl", entry.run_id));
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn read_run(&self, run_id: &str) -> Result<Vec<LogEntry>> {
        let Some(dir) = &self.dir else {
            return Ok(Vec::new());
        };
        let path = dir.join(format!("{run_id}.jsonl"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(path)?;
        let mut entries = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str(line)?);
        }
        Ok(entries)
    }

    /// Every Run id this log has ever seen, from its filenames.
    pub fn known_runs(&self) -> Vec<String> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let Ok(read) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut runs: Vec<String> = read
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".jsonl").map(|s| s.to_string())
            })
            .collect();
        runs.sort();
        runs
    }
}

pub fn now_iso() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    humantime_epoch(now.as_secs(), now.subsec_nanos())
}

fn humantime_epoch(secs: u64, nanos: u32) -> String {
    // A dependency-free RFC 3339 (UTC) formatter, precise enough for log ordering.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z"
    )
}

/// Parses an RFC 3339 UTC timestamp (`now_iso`'s own format, and the timestamps a backend's
/// transcript carries) to epoch milliseconds. `None` for anything that does not parse.
pub fn epoch_millis(timestamp: &str) -> Option<i64> {
    let timestamp = timestamp.strip_suffix('Z')?;
    let (date, time) = timestamp.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;

    let (hms, frac) = match time.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time, ""),
    };
    let mut hms_parts = hms.split(':');
    let hour: i64 = hms_parts.next()?.parse().ok()?;
    let minute: i64 = hms_parts.next()?.parse().ok()?;
    let second: i64 = hms_parts.next()?.parse().ok()?;

    let millis: i64 = if frac.is_empty() {
        0
    } else {
        let padded = format!("{frac:0<3}");
        padded[..3].parse().ok()?
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(secs * 1000 + millis)
}

/// The inverse of `civil_from_days`: y/m/d -> days since 1970-01-01. Howard Hinnant's
/// days-from-civil algorithm, public domain.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = ((m as i64 + 9) % 12) as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01 -> y/m/d), public domain.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// `<role>-<4 hex>-<name>` (`.dev_docs/CONTEXT.md`, *hash_id*): the stable identifier of one
/// agent execution, quoted in every log line about it.
pub fn hash_id(role: &str, name: &str, seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{:02x}{:02x}", digest[0], digest[1]);
    format!("{role}-{hex}-{name}")
}

/// A Run's goal from its `run_created` details. Logs written before the rename carry it as
/// `big_plan`.
pub fn run_goal(details: &serde_json::Value) -> Option<&str> {
    details
        .get("goal")
        .or_else(|| details.get("big_plan"))
        .and_then(serde_json::Value::as_str)
        .filter(|goal| !goal.is_empty())
}

pub mod events {
    pub const RUN_CREATED: &str = "run_created";
    pub const AGENT_ENTER: &str = "agent_enter";
    pub const AGENT_EXIT: &str = "agent_exit";
    pub const INBOX_MESSAGE: &str = "inbox_message";
    pub const INBOX_REPLY: &str = "inbox_reply";
    pub const INBOX_ACK: &str = "inbox_ack";
    pub const INBOX_TIMEOUT: &str = "inbox_timeout";
    pub const NEEDS_HUMAN: &str = "needs_human";
    pub const HUMAN_ANSWER: &str = "human_answer";
    pub const WORKTREE_REMOVED: &str = "worktree_removed";
    pub const RUN_CLOSED: &str = "run_closed";
    pub const CHECKLIST_UPDATED: &str = "checklist_updated";
    pub const DECISION: &str = "decision";
    pub const DELEGATION: &str = "delegation";
    pub const REVIEW_RESULT: &str = "review-result";
    pub const RETRY: &str = "retry";
    pub const BLOCKER: &str = "blocker";
    pub const NOTE: &str = "note";
    pub const ENV_CREATED: &str = "env_created";
    pub const ENV_READY: &str = "env_ready";
    pub const ENV_DESTROYED: &str = "env_destroyed";
    pub const ENV_IMAGE_BUILT: &str = "env_image_built";
    pub const ENV_REAPED: &str = "env_reaped";
    pub const EXEC_PROFILE_SELECTED: &str = "exec_profile_selected";
    pub const EXEC_PROFILE_NONE: &str = "exec_profile_none";
    pub const ENV_SKIPPED: &str = "env_skipped";
    pub const PLUGINS_RESOLVED: &str = "plugins_resolved";
    pub const DISPATCH_QUEUED: &str = "dispatch_queued";
    pub const DISPATCH_DEQUEUED: &str = "dispatch_dequeued";
    pub const QUEUED_LAUNCH_FAILED: &str = "queued_launch_failed";
    pub const DISPATCH_DROPPED: &str = "dispatch_dropped";
    pub const DISPATCH_RELEASED: &str = "dispatch_released";
}

/// Milliseconds since the epoch, for the execution ledger's timestamps.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_id_has_the_role_hex_name_shape() {
        let id = hash_id("worker", "glaze", "seed-1");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "worker");
        assert_eq!(parts[1].len(), 4);
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parts[2], "glaze");

        // Deterministic for the same seed, distinct for a different one.
        assert_eq!(hash_id("worker", "glaze", "seed-1"), id);
        assert_ne!(hash_id("worker", "glaze", "seed-2"), id);
    }

    #[test]
    fn a_runs_goal_is_read_under_its_new_name_or_its_old_one() {
        use serde_json::json;
        assert_eq!(run_goal(&json!({"goal": "Glaze every pot"})), Some("Glaze every pot"));
        assert_eq!(run_goal(&json!({"big_plan": "Sweep the studio"})), Some("Sweep the studio"));
        assert_eq!(run_goal(&json!({"goal": "", "repo": "/tmp"})), None);
        assert_eq!(run_goal(&json!({"repo": "/tmp"})), None);
    }

    #[test]
    fn epoch_millis_round_trips_through_now_iso() {
        let text = "2026-09-22T01:41:00.500000000Z";
        let millis = epoch_millis(text).unwrap();
        assert_eq!(millis % 1000, 500);
        assert!(epoch_millis("not a timestamp").is_none());
    }
}
