// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::environment::Environment;
use crate::error::{codes, err};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// A plugin's identity as `run.json` records it. Always `[]` in this ticket; F5 fills it in
/// (ticket Contracts).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRecord {
    pub name: String,
    pub source: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub base_branch: String,
    pub backend: String,
    pub created_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub plugins: Vec<PluginRecord>,
    #[serde(default)]
    pub meta_worktree: Option<String>,
    #[serde(default)]
    pub meta_dispatch_id: Option<String>,
    /// What the Run was fired to do. Records written before the rename carry it as `big_plan`.
    #[serde(default, alias = "big_plan")]
    pub goal: Option<String>,
    /// How many Dispatches of each role may run at once, settled at `meta fire` from the
    /// plugins' defaults and `.oat/roles.toml`. A role not listed has no limit.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub role_limits: std::collections::BTreeMap<String, u32>,
    /// The backend and model each role is launched with where `.oat/roles.toml` sets them,
    /// settled at `meta fire` like `role_limits`. A role not listed takes its plugin's.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub role_settings: std::collections::BTreeMap<String, crate::role::repository::LaunchSettings>,
}

impl RunRecord {
    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Settlement {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchRecord {
    pub id: String,
    pub run_id: String,
    pub role: String,
    pub backend: String,
    pub worktree: String,
    pub branch: String,
    pub created_at: String,
    #[serde(default)]
    pub env_id: Option<String>,
    #[serde(default)]
    pub image_id: Option<String>,
    /// Why a role that asked for an execution environment was launched without one, so its
    /// commands ran on the host. `None` when it got one, or never asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_skipped: Option<String>,
    #[serde(default)]
    pub settled: Option<Settlement>,
    #[serde(default)]
    pub report: Option<String>,
    #[serde(default)]
    pub released_at: Option<String>,
    /// What this Dispatch is for, as `role fire --name` gave it: the last part of its
    /// hash_id. `None` for the meta-agent, and for a launch that named nothing, which are
    /// then known by the Run's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The model and reasoning effort it was launched with, as its role set them for its
    /// backend. `None` when the role left them to the backend's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Prepared for an external driver, without a meta-agent tmux session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub headless: bool,
}

impl DispatchRecord {
    pub fn is_settled(&self) -> bool {
        self.settled.is_some()
    }

    /// Its hash_id: the meta-agent's under `meta`, every other role's under its own name,
    /// then what it was launched for. Every place that finds a Dispatch's session derives it
    /// here, so they cannot disagree.
    pub fn hash_id(&self, run: &RunRecord) -> String {
        let label = if self.role == crate::role::CoreRole::Meta.name() {
            "meta"
        } else {
            self.role.as_str()
        };
        crate::event_log::hash_id(label, self.name.as_deref().unwrap_or(&run.name), &self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A member agent settled its Dispatch; the message is its report. Inboxes written before
    /// the rename carry it as `worker_done`.
    #[serde(alias = "worker_done")]
    MemberDone,
    Question,
    Escalation,
    /// A queued `role fire` that could not be launched when its place came free. Nobody is
    /// waiting for a reply to it.
    LaunchFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub seq: u64,
    pub dispatch_id: String,
    pub kind: MessageKind,
    pub message: String,
    pub created_at: String,
    #[serde(default)]
    pub delivery_id: Option<String>,
    #[serde(default)]
    pub acked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxReply {
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub delivery_id: String,
    pub messages: Vec<InboxMessage>,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(env: &dyn Environment) -> Result<Self> {
        let root = crate::environment::store_root(env)
            .ok_or_else(|| err(codes::INTERNAL_ERROR, "no state directory available"))?;
        Ok(Self { root })
    }

    pub fn for_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    fn run_json_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("run.json")
    }

    fn dispatch_dir(&self, run_id: &str, dispatch_id: &str) -> PathBuf {
        self.run_dir(run_id).join("dispatches").join(dispatch_id)
    }

    fn dispatch_json_path(&self, run_id: &str, dispatch_id: &str) -> PathBuf {
        self.dispatch_dir(run_id, dispatch_id).join("dispatch.json")
    }

    fn inbox_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("inbox")
    }

    /// Where a Run's plugin snapshot lives (F5's Architecture: "the plugin trees are copied
    /// into the Run's ... state directory"). Every later catalog for this Run is built from
    /// here, never by re-resolving `.oat/plugins.toml`.
    pub fn run_plugin_snapshot_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("plugins")
    }

    pub fn create_run(&self, run: &RunRecord) -> Result<()> {
        let dir = self.run_dir(&run.id);
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(self.inbox_dir(&run.id))?;
        self.save_run(run)
    }

    pub fn save_run(&self, run: &RunRecord) -> Result<()> {
        let path = self.run_json_path(&run.id);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, serde_json::to_string_pretty(run)?)?;
        Ok(())
    }

    pub fn load_run(&self, run_id: &str) -> Result<RunRecord> {
        let path = self.run_json_path(run_id);
        let content = fs::read_to_string(&path)
            .map_err(|_| err(codes::INVALID_INPUT, format!("no such run: {run_id}")))?;
        Ok(serde_json::from_str(&content)?)
    }

    pub fn list_run_ids(&self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().join("run.json").exists() {
                ids.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// The most recently updated Run — by `run.json`'s modified time — for a command whose
    /// `--run` was left implicit.
    pub fn most_recently_updated_run(&self) -> Result<Option<String>> {
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for id in self.list_run_ids()? {
            let meta = fs::metadata(self.run_json_path(&id))?;
            let modified = meta.modified()?;
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, id));
            }
        }
        Ok(best.map(|(_, id)| id))
    }

    pub fn create_dispatch(&self, dispatch: &DispatchRecord) -> Result<()> {
        fs::create_dir_all(self.dispatch_dir(&dispatch.run_id, &dispatch.id))?;
        self.save_dispatch(dispatch)
    }

    pub fn save_dispatch(&self, dispatch: &DispatchRecord) -> Result<()> {
        let path = self.dispatch_json_path(&dispatch.run_id, &dispatch.id);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, serde_json::to_string_pretty(dispatch)?)?;
        Ok(())
    }

    pub fn load_dispatch(&self, run_id: &str, dispatch_id: &str) -> Result<DispatchRecord> {
        let path = self.dispatch_json_path(run_id, dispatch_id);
        let content = fs::read_to_string(&path).map_err(|_| {
            err(
                codes::DISPATCH_NOT_BOUND,
                format!("no such dispatch: {dispatch_id}"),
            )
        })?;
        Ok(serde_json::from_str(&content)?)
    }

    /// Every Dispatch recorded for a Run, oldest first. A record that cannot be read is
    /// skipped: readers of a live Run see what is there, not an error.
    pub fn list_dispatches(&self, run_id: &str) -> Vec<DispatchRecord> {
        let dir = self.root.join(run_id).join("dispatches");
        let Ok(read) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut dispatches: Vec<DispatchRecord> = read
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                self.load_dispatch(run_id, &entry.file_name().to_string_lossy())
                    .ok()
            })
            .collect();
        dispatches.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        dispatches
    }

    pub fn dispatch_dir_path(&self, run_id: &str, dispatch_id: &str) -> PathBuf {
        self.dispatch_dir(run_id, dispatch_id)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- Run inbox (`.dev_docs/CONTEXT.md`, *Run inbox*: exactly one consumer, that Run's
    // `oat-meta`) ---

    fn next_seq(&self, run_id: &str) -> Result<u64> {
        let dir = self.inbox_dir(run_id);
        fs::create_dir_all(&dir)?;
        let mut max = 0u64;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name.strip_suffix(".json").and_then(|s| s.parse::<u64>().ok()) {
                max = max.max(num);
            }
        }
        Ok(max + 1)
    }

    pub fn append_inbox(
        &self,
        run_id: &str,
        dispatch_id: &str,
        kind: MessageKind,
        message: &str,
    ) -> Result<u64> {
        let seq = self.next_seq(run_id)?;
        let entry = InboxMessage {
            seq,
            dispatch_id: dispatch_id.to_string(),
            kind,
            message: message.to_string(),
            created_at: crate::event_log::now_iso(),
            delivery_id: None,
            acked: false,
        };
        let path = self.inbox_dir(run_id).join(format!("{seq}.json"));
        fs::write(path, serde_json::to_string_pretty(&entry)?)?;
        Ok(seq)
    }

    fn read_message(&self, run_id: &str, seq: u64) -> Result<InboxMessage> {
        let path = self.inbox_dir(run_id).join(format!("{seq}.json"));
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    fn write_message(&self, run_id: &str, message: &InboxMessage) -> Result<()> {
        let path = self.inbox_dir(run_id).join(format!("{}.json", message.seq));
        fs::write(path, serde_json::to_string_pretty(message)?)?;
        Ok(())
    }

    fn pending_seqs(&self, run_id: &str) -> Result<Vec<u64>> {
        let dir = self.inbox_dir(run_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut seqs = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name.strip_suffix(".json").and_then(|s| s.parse::<u64>().ok()) {
                seqs.push(num);
            }
        }
        seqs.sort();
        Ok(seqs)
    }

    /// Blocks, polling at `poll` intervals, until at least one undelivered message exists or
    /// `timeout` elapses. An unacknowledged prior delivery is redelivered under the same
    /// delivery id with the same message ids (ticket §4.2).
    pub fn wait_inbox(
        &self,
        run_id: &str,
        timeout: Duration,
        poll: Duration,
    ) -> Result<Option<Delivery>> {
        let started = std::time::Instant::now();
        loop {
            let seqs = self.pending_seqs(run_id)?;
            let mut messages = Vec::new();
            let mut existing_delivery: Option<String> = None;
            for seq in &seqs {
                let msg = self.read_message(run_id, *seq)?;
                if msg.acked {
                    continue;
                }
                if let Some(delivery_id) = &msg.delivery_id {
                    existing_delivery = Some(delivery_id.clone());
                    messages.push(msg);
                } else if existing_delivery.is_none() {
                    messages.push(msg);
                }
            }
            // Redeliver an outstanding, unacknowledged delivery unchanged.
            let delivered_pending: Vec<InboxMessage> = messages
                .iter()
                .filter(|m| m.delivery_id.is_some())
                .cloned()
                .collect();
            if !delivered_pending.is_empty() {
                let delivery_id = delivered_pending[0].delivery_id.clone().unwrap();
                return Ok(Some(Delivery {
                    delivery_id,
                    messages: delivered_pending,
                }));
            }
            let undelivered: Vec<InboxMessage> = messages
                .into_iter()
                .filter(|m| m.delivery_id.is_none())
                .collect();
            if !undelivered.is_empty() {
                let delivery_id = format!("delivery-{}", crate::event_log::now_iso());
                let mut delivered = Vec::new();
                for mut msg in undelivered {
                    msg.delivery_id = Some(delivery_id.clone());
                    self.write_message(run_id, &msg)?;
                    delivered.push(msg);
                }
                return Ok(Some(Delivery {
                    delivery_id,
                    messages: delivered,
                }));
            }
            if started.elapsed() >= timeout {
                return Ok(None);
            }
            thread::sleep(poll.min(timeout.saturating_sub(started.elapsed()).max(Duration::from_millis(1))));
        }
    }

    /// Retires a delivery: every message in it is marked acknowledged and will not be
    /// redelivered.
    pub fn ack_inbox(&self, run_id: &str, delivery_id: &str) -> Result<usize> {
        let mut acked = 0;
        for seq in self.pending_seqs(run_id)? {
            let mut msg = self.read_message(run_id, seq)?;
            if msg.delivery_id.as_deref() == Some(delivery_id) && !msg.acked {
                msg.acked = true;
                self.write_message(run_id, &msg)?;
                acked += 1;
            }
        }
        Ok(acked)
    }

    /// Posts the meta-agent's reply to one message, so its sender's `wait_reply` sees it.
    pub fn reply_to_message(&self, run_id: &str, seq: u64, message: &str) -> Result<()> {
        // Confirms the message exists before writing a reply beside it.
        self.read_message(run_id, seq)?;
        let reply = InboxReply {
            message: message.to_string(),
            created_at: crate::event_log::now_iso(),
        };
        let path = self.inbox_dir(run_id).join(format!("{seq}.reply.json"));
        fs::write(path, serde_json::to_string_pretty(&reply)?)?;
        Ok(())
    }

    pub fn wait_reply(
        &self,
        run_id: &str,
        seq: u64,
        timeout: Duration,
        poll: Duration,
    ) -> Result<Option<InboxReply>> {
        let started = std::time::Instant::now();
        let path = self.inbox_dir(run_id).join(format!("{seq}.reply.json"));
        loop {
            if path.exists() {
                let content = fs::read_to_string(&path)?;
                return Ok(Some(serde_json::from_str(&content)?));
            }
            if started.elapsed() >= timeout {
                return Ok(None);
            }
            thread::sleep(poll.min(timeout.saturating_sub(started.elapsed()).max(Duration::from_millis(1))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::for_root(dir.path().join("runs"));
        (dir, store)
    }

    #[test]
    fn an_undelivered_message_is_delivered_once_and_not_redelivered_after_ack() {
        let (_dir, store) = store();
        store.append_inbox("run1", "dispatch1", MessageKind::MemberDone, "done").unwrap();

        let first = store
            .wait_inbox("run1", Duration::from_millis(50), Duration::from_millis(5))
            .unwrap()
            .expect("a delivery");
        assert_eq!(first.messages.len(), 1);

        // Redelivered under the same delivery id until acknowledged.
        let second = store
            .wait_inbox("run1", Duration::from_millis(50), Duration::from_millis(5))
            .unwrap()
            .expect("redelivered");
        assert_eq!(second.delivery_id, first.delivery_id);

        store.ack_inbox("run1", &first.delivery_id).unwrap();
        let third = store
            .wait_inbox("run1", Duration::from_millis(50), Duration::from_millis(5))
            .unwrap();
        assert!(third.is_none(), "an acknowledged delivery is not redelivered");
    }

    #[test]
    fn a_message_written_as_worker_done_is_delivered_as_member_done() {
        let (_dir, store) = store();
        let dir = store.inbox_dir("run1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.json"),
            r#"{"seq": 1, "dispatch_id": "dispatch1", "kind": "worker_done", "message": "done",
                "created_at": "2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let delivery = store
            .wait_inbox("run1", Duration::from_millis(50), Duration::from_millis(5))
            .unwrap()
            .expect("a delivery");
        assert!(matches!(delivery.messages[0].kind, MessageKind::MemberDone));
        assert_eq!(serde_json::to_value(&delivery.messages[0].kind).unwrap(), "member_done");
    }

    #[test]
    fn a_reply_is_visible_to_the_message_it_answers() {
        let (_dir, store) = store();
        let seq = store
            .append_inbox("run1", "dispatch1", MessageKind::Question, "which glaze?")
            .unwrap();
        assert!(store
            .wait_reply("run1", seq, Duration::from_millis(20), Duration::from_millis(5))
            .unwrap()
            .is_none());
        store.reply_to_message("run1", seq, "cobalt").unwrap();
        let reply = store
            .wait_reply("run1", seq, Duration::from_millis(20), Duration::from_millis(5))
            .unwrap()
            .unwrap();
        assert_eq!(reply.message, "cobalt");
    }

    #[test]
    fn run_json_round_trips_with_and_without_plugin_records() {
        let (_dir, store) = store();
        let empty = RunRecord {
            id: "r1".to_string(),
            name: "r1".to_string(),
            repo: "/tmp/repo".to_string(),
            base_branch: "main".to_string(),
            backend: "claude".to_string(),
            created_at: "2026-01-01T00:00:00.000000000Z".to_string(),
            closed_at: None,
            plugins: Vec::new(),
            meta_worktree: None,
            meta_dispatch_id: None,
            goal: None,
            role_limits: Default::default(),
            role_settings: Default::default(),
        };
        store.create_run(&empty).unwrap();
        assert_eq!(store.load_run("r1").unwrap().plugins, Vec::new());

        // A record written before the rename keeps its goal, and is saved under the new key.
        let old: RunRecord = serde_json::from_value(serde_json::json!({
            "id": "r0", "name": "r0", "repo": "/tmp/repo", "base_branch": "main",
            "backend": "claude", "created_at": "2026-01-01T00:00:00Z", "big_plan": "Glaze every pot",
        }))
        .unwrap();
        assert_eq!(old.goal.as_deref(), Some("Glaze every pot"));
        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!(saved["goal"], "Glaze every pot");
        assert!(saved.get("big_plan").is_none());

        let mut with_plugins = empty.clone();
        with_plugins.id = "r2".to_string();
        with_plugins.name = "r2".to_string();
        with_plugins.plugins = vec![
            PluginRecord { name: "example".to_string(), source: "embedded".to_string(), version: "0.1.0".to_string() },
            PluginRecord { name: "team".to_string(), source: "git:...".to_string(), version: "abc123".to_string() },
        ];
        store.create_run(&with_plugins).unwrap();
        assert_eq!(store.load_run("r2").unwrap().plugins.len(), 2);
    }

    #[test]
    fn a_dispatch_is_known_by_its_name_and_one_recorded_without_one_by_its_runs() {
        let run: RunRecord = serde_json::from_value(serde_json::json!({
            "id": "s3", "name": "s3-format-v6-parity", "repo": "/tmp/repo", "base_branch": "main",
            "backend": "claude", "created_at": "2026-01-01T00:00:00.000000000Z",
        }))
        .unwrap();
        // A record written before Dispatches had names: its session keeps the name it has.
        let old: DispatchRecord = serde_json::from_value(serde_json::json!({
            "id": "dispatch-1", "run_id": "s3", "role": "worker", "backend": "claude",
            "worktree": "/tmp/w", "branch": "oat/s3/w", "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        assert!(!old.headless);
        assert!(serde_json::to_value(&old).unwrap().get("headless").is_none());
        assert_eq!(old.hash_id(&run), crate::event_log::hash_id("worker", "s3-format-v6-parity", "dispatch-1"));

        let named = DispatchRecord { name: Some("s3-f8".to_string()), ..old.clone() };
        assert!(named.hash_id(&run).starts_with("worker-") && named.hash_id(&run).ends_with("-s3-f8"));
        let meta = DispatchRecord { role: "oat-meta".to_string(), ..old };
        assert!(meta.hash_id(&run).starts_with("meta-"));
    }
}
