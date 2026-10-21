// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

//! How many Dispatches of one role a Run may run at once, and the queue a `role fire` over
//! that limit waits in.
//!
//! A plugin's `role.toml` sets a role's default `max_concurrent`; the repository overrides it
//! in `.oat/roles.toml`. `meta fire` settles the result into the Run's record, so a Run keeps
//! the limits it started with, the same way it keeps the plugins it started with.
//!
//! A Dispatch holds one of its role's places from launch until it is settled or released. A
//! `role fire` that finds no free place is written to `<run>/queue/<seq>.json` and launched,
//! in order, by the next meta-agent command that finds one free.

use crate::error::{codes, err};
use crate::role::repository::RepoRoles;
use crate::role::RoleCatalog;
use crate::store::Store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Every role's effective limit for a Run: the plugin's default, replaced by the repository's
/// `.oat/roles.toml` where it names the role. A role missing from the result has no limit.
pub fn resolve_limits(repo: &RepoRoles, catalog: &dyn RoleCatalog) -> Result<BTreeMap<String, u32>> {
    let mut limits = BTreeMap::new();
    for name in catalog.role_names() {
        if let Some(limit) = catalog.role(&name)?.max_concurrent {
            limits.insert(name, limit);
        }
    }
    limits.extend(repo.limits.clone());
    Ok(limits)
}

/// Everything `role fire` was given, kept until a place is free. The Dispatch's id is chosen
/// when it is queued, so the meta-agent can name it before it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedFire {
    pub seq: u64,
    pub dispatch_id: String,
    pub role: String,
    pub from: Option<PathBuf>,
    /// The commit a role that starts in an existing worktree gets a new one at, resolved when
    /// it was fired, so a branch that moves while the launch waits does not move the review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub trust_workspace: bool,
    pub task: String,
    pub queued_at: String,
    /// The process launching it, once one has claimed it; a claim whose process is gone is
    /// returned to the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<u32>,
}

const WAITING: &str = "json";
const LAUNCHING: &str = "launching";
/// Longer than any claim holds the lock for: it covers reading a few files, never a launch.
const STALE_LOCK_AFTER: Duration = Duration::from_secs(30);
const LOCK_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Queue<'a> {
    store: &'a Store,
    run_id: String,
    dir: PathBuf,
}

impl<'a> Queue<'a> {
    pub fn for_run(store: &'a Store, run_id: &str) -> Self {
        Self {
            store,
            run_id: run_id.to_string(),
            dir: store.root().join(run_id).join("queue"),
        }
    }

    /// The entries still waiting for a place, oldest first.
    pub fn waiting(&self) -> Result<Vec<QueuedFire>> {
        self.read(WAITING)
    }

    /// Queues `fire` behind everything already waiting and returns it with its place.
    pub fn push(&self, mut fire: QueuedFire) -> Result<QueuedFire> {
        let _lock = self.lock()?;
        let taken = self.read(WAITING)?.into_iter().chain(self.read(LAUNCHING)?);
        fire.seq = taken.map(|entry| entry.seq).max().unwrap_or(0) + 1;
        fire.claimed_by = None;
        self.write(&fire, WAITING)?;
        Ok(fire)
    }

    /// Claims the oldest waiting entry whose role has a free place, marking it as being
    /// launched by this process. Entries of a role at its limit are passed over, not blocking
    /// those behind them.
    pub fn claim_next(&self, limits: &BTreeMap<String, u32>) -> Result<Option<QueuedFire>> {
        let _lock = self.lock()?;
        self.recover_abandoned_claims()?;
        let launching = self.read(LAUNCHING)?;
        for mut entry in self.read(WAITING)? {
            let free = match limits.get(&entry.role) {
                None => true,
                Some(&limit) => {
                    let claimed = launching.iter().filter(|e| e.role == entry.role).count();
                    self.active(&entry.role)? + claimed < limit as usize
                }
            };
            if free {
                entry.claimed_by = Some(std::process::id());
                self.write(&entry, LAUNCHING)?;
                fs::remove_file(self.path(entry.seq, WAITING))?;
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Ends a claim, whether the launch succeeded or failed.
    pub fn release_claim(&self, entry: &QueuedFire) -> Result<()> {
        match fs::remove_file(self.path(entry.seq, LAUNCHING)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Empties the queue, returning what was still waiting.
    pub fn drop_waiting(&self) -> Result<Vec<QueuedFire>> {
        let _lock = self.lock()?;
        let waiting = self.read(WAITING)?;
        for entry in &waiting {
            fs::remove_file(self.path(entry.seq, WAITING))?;
        }
        Ok(waiting)
    }

    /// Dispatches of `role` that hold a place: launched, and neither settled nor released.
    pub fn active(&self, role: &str) -> Result<usize> {
        let dir = self.store.root().join(&self.run_id).join("dispatches");
        if !dir.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in fs::read_dir(&dir)? {
            let id = entry?.file_name().to_string_lossy().to_string();
            let Ok(dispatch) = self.store.load_dispatch(&self.run_id, &id) else {
                continue;
            };
            if dispatch.role == role && !dispatch.is_settled() && dispatch.released_at.is_none() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn recover_abandoned_claims(&self) -> Result<()> {
        for mut entry in self.read(LAUNCHING)? {
            if entry.claimed_by.is_some_and(process_alive) {
                continue;
            }
            // The launch got as far as recording its Dispatch: it holds its place from there.
            if self.store.load_dispatch(&self.run_id, &entry.dispatch_id).is_err() {
                entry.claimed_by = None;
                self.write(&entry, WAITING)?;
            }
            fs::remove_file(self.path(entry.seq, LAUNCHING))?;
        }
        Ok(())
    }

    fn path(&self, seq: u64, extension: &str) -> PathBuf {
        self.dir.join(format!("{seq}.{extension}"))
    }

    fn read(&self, extension: &str) -> Result<Vec<QueuedFire>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some(extension) {
                continue;
            }
            entries.push(serde_json::from_str::<QueuedFire>(&fs::read_to_string(&path)?)?);
        }
        entries.sort_by_key(|entry| entry.seq);
        Ok(entries)
    }

    fn write(&self, entry: &QueuedFire, extension: &str) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(entry.seq, extension);
        let partial = path.with_extension(format!("{extension}.partial"));
        fs::write(&partial, serde_json::to_string_pretty(entry)?)?;
        fs::rename(partial, path)?;
        Ok(())
    }

    /// Serializes every decision about who gets a place. Held only while files are read and
    /// renamed, never across a launch.
    fn lock(&self) -> Result<QueueLock> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("lock");
        let started = Instant::now();
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(QueueLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > STALE_LOCK_AFTER);
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() > LOCK_TIMEOUT {
                        return Err(err(
                            codes::INTERNAL_ERROR,
                            format!("the Run's queue is locked: {}", path.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

struct QueueLock {
    path: PathBuf,
}

impl Drop for QueueLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Probe existence without delivering a signal. Permission denial still proves existence.
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false; // kill(0, ...) probes a process group, not a process.
    }
    // SAFETY: signal zero only probes the positive pid; no pointers or signal delivery.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // ESRCH is the only definitive absence; EPERM (and unexpected errors) retain the claim.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive_and_reaped_child_is_dead() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }
}
