// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::concurrency::{Queue, QueuedFire};
use crate::environment::Environment;
use crate::error::{codes, err};
use crate::store::{DispatchRecord, Settlement, Store};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Checklist {
    pub items: Vec<String>,
    pub checked: Vec<bool>,
    /// Per item, the `role fire --name` of the work it tracks. Every Dispatch named that, or
    /// named that plus a `-` suffix (`s3-f8`, `s3-f8-fix1`, `s3-f8-review`), counts as that
    /// item's, so its progress is read from the Run store rather than reported by hand.
    #[serde(default)]
    pub links: Vec<Option<String>>,
}

impl Checklist {
    pub fn link(&self, index: usize) -> Option<&str> {
        self.links.get(index).and_then(|link| link.as_deref())
    }

    fn index(&self, n: usize) -> Result<usize> {
        if self.items.is_empty() {
            return Err(err(codes::INVALID_INPUT, "no checklist is set for this run"));
        }
        n.checked_sub(1)
            .filter(|i| *i < self.items.len())
            .ok_or_else(|| err(codes::INVALID_INPUT, format!("no checklist item {n}")))
    }
}

/// Whether a Dispatch named `name` belongs to the item linked to `key`. A whole `-`-separated
/// prefix, so `s3-f1` does not claim `s3-f10-rebase`.
pub fn link_matches(key: &str, name: &str) -> bool {
    name.strip_prefix(key)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// Where the work behind a linked item stands, derived from its Dispatches and queued fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ItemProgress {
    /// Nothing has been fired under the link yet.
    NoDispatch,
    /// At least one Dispatch is still running; `role` and `name` are the newest one's.
    Active {
        role: String,
        name: String,
        running: usize,
        dispatches: usize,
    },
    /// Nothing is running, but a fire is waiting for a place.
    Queued { name: String, dispatches: usize },
    /// Every Dispatch has ended; these are the newest one's.
    Ended {
        role: String,
        name: String,
        outcome: Option<String>,
        /// Every one of them has been released: the meta-agent has no further use for the
        /// work, so an unchecked item is likely one it forgot to check.
        all_released: bool,
        dispatches: usize,
    },
}

impl ItemProgress {
    /// Derives the progress of the item linked to `key`. `dispatches` is oldest first, as
    /// `Store::list_dispatches` returns them.
    pub fn of(key: &str, dispatches: &[DispatchRecord], queued: &[QueuedFire]) -> Self {
        let own: Vec<&DispatchRecord> = dispatches
            .iter()
            .filter(|dispatch| dispatch.name.as_deref().is_some_and(|name| link_matches(key, name)))
            .collect();
        let waiting: Vec<&QueuedFire> = queued
            .iter()
            .filter(|fire| fire.name.as_deref().is_some_and(|name| link_matches(key, name)))
            .collect();
        let name_of = |dispatch: &DispatchRecord| dispatch.name.clone().unwrap_or_default();
        let running: Vec<&&DispatchRecord> = own
            .iter()
            .filter(|dispatch| !dispatch.is_settled() && dispatch.released_at.is_none())
            .collect();
        if let Some(latest) = running.last() {
            return ItemProgress::Active {
                role: latest.role.clone(),
                name: name_of(latest),
                running: running.len(),
                dispatches: own.len(),
            };
        }
        if let Some(fire) = waiting.first() {
            return ItemProgress::Queued {
                name: fire.name.clone().unwrap_or_default(),
                dispatches: own.len(),
            };
        }
        match own.last() {
            None => ItemProgress::NoDispatch,
            Some(latest) => ItemProgress::Ended {
                role: latest.role.clone(),
                name: name_of(latest),
                outcome: latest.settled.as_ref().map(|settled| {
                    match settled {
                        Settlement::Succeeded => "succeeded",
                        Settlement::Failed => "failed",
                    }
                    .to_owned()
                }),
                all_released: own.iter().all(|dispatch| dispatch.released_at.is_some()),
                dispatches: own.len(),
            },
        }
    }

    /// One line for the checklist panel.
    pub fn summary(&self) -> String {
        let rounds = |n: usize| {
            if n > 1 {
                format!(" · {n} dispatches")
            } else {
                String::new()
            }
        };
        match self {
            ItemProgress::NoDispatch => "no dispatch yet".to_owned(),
            ItemProgress::Active {
                role,
                name,
                running,
                dispatches,
            } => {
                let more = if *running > 1 {
                    format!(" +{}", running - 1)
                } else {
                    String::new()
                };
                format!("{role} {name} active{more}{}", rounds(*dispatches))
            }
            ItemProgress::Queued { name, dispatches } => {
                format!("{name} queued{}", rounds(*dispatches))
            }
            ItemProgress::Ended {
                role,
                name,
                outcome,
                all_released,
                dispatches,
            } => {
                let outcome = outcome.as_deref().unwrap_or("ended");
                let released = if *all_released { ", released" } else { "" };
                format!("{role} {name} {outcome}{released}{}", rounds(*dispatches))
            }
        }
    }

    /// Whether the work looks finished: every Dispatch ended well and was released.
    pub fn looks_done(&self) -> bool {
        matches!(
            self,
            ItemProgress::Ended {
                outcome: Some(outcome),
                all_released: true,
                ..
            } if outcome == "succeeded"
        )
    }
}

/// Each item's progress, `None` for an item with no link. Reads the Run store once.
pub fn progress(store: &Store, run_id: &str, checklist: &Checklist) -> Vec<Option<ItemProgress>> {
    if checklist.links.iter().all(Option::is_none) {
        return vec![None; checklist.items.len()];
    }
    let dispatches = store.list_dispatches(run_id);
    let queued = Queue::for_run(store, run_id).waiting().unwrap_or_default();
    (0..checklist.items.len())
        .map(|index| {
            checklist
                .link(index)
                .map(|key| ItemProgress::of(key, &dispatches, &queued))
        })
        .collect()
}

pub struct ChecklistStore {
    dir: Option<PathBuf>,
}

impl ChecklistStore {
    pub fn open(env: &dyn Environment) -> Self {
        Self {
            dir: crate::environment::checklist_dir(env),
        }
    }

    pub fn for_dir(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    fn path(&self, run_id: &str) -> Result<PathBuf> {
        let dir = self
            .dir
            .clone()
            .ok_or_else(|| err(codes::INTERNAL_ERROR, "no checklist directory available"))?;
        Ok(dir.join(format!("{run_id}.json")))
    }

    pub fn load(&self, run_id: &str) -> Result<Checklist> {
        let path = self.path(run_id)?;
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Ok(Checklist::default()),
        };
        Ok(serde_json::from_str(&content)?)
    }

    pub fn save(&self, run_id: &str, checklist: &Checklist) -> Result<()> {
        let path = self.path(run_id)?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, serde_json::to_string_pretty(checklist)?)?;
        Ok(())
    }

    /// Replaces the whole checklist and clears checked state.
    pub fn replace_items(&self, run_id: &str, items: Vec<String>) -> Result<Checklist> {
        let checked = vec![false; items.len()];
        let checklist = Checklist {
            items,
            checked,
            links: Vec::new(),
        };
        self.save(run_id, &checklist)?;
        Ok(checklist)
    }

    /// `n` is 1-based, per the CLI contract.
    pub fn set_checked(&self, run_id: &str, n: usize, value: bool) -> Result<Checklist> {
        let mut checklist = self.load(run_id)?;
        let index = checklist.index(n)?;
        checklist.checked.resize(checklist.items.len(), false);
        checklist.checked[index] = value;
        self.save(run_id, &checklist)?;
        Ok(checklist)
    }

    /// Links item `n` (1-based) to the work named `link`, or unlinks it with `None`.
    pub fn set_link(&self, run_id: &str, n: usize, link: Option<String>) -> Result<Checklist> {
        let mut checklist = self.load(run_id)?;
        let index = checklist.index(n)?;
        checklist.links.resize(checklist.items.len(), None);
        checklist.links[index] = link.filter(|link| !link.is_empty());
        self.save(run_id, &checklist)?;
        Ok(checklist)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(id: &str, name: Option<&str>, settled: Option<Settlement>, released: bool) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            run_id: "run".to_string(),
            role: "worker".to_string(),
            backend: "claude".to_string(),
            worktree: String::new(),
            branch: String::new(),
            created_at: format!("2026-09-26T01:00:0{}Z", id.len() % 10),
            env_id: None,
            image_id: None,
            env_skipped: None,
            settled,
            report: None,
            released_at: released.then(|| "2026-09-26T02:00:00Z".to_string()),
            name: name.map(str::to_string),
            model: None,
            effort: None,
            headless: false,
        }
    }

    #[test]
    fn a_link_claims_its_name_and_its_suffixed_rounds_but_not_a_longer_name() {
        assert!(link_matches("s3-f1", "s3-f1"));
        assert!(link_matches("s3-f1", "s3-f1-fix3"));
        assert!(!link_matches("s3-f1", "s3-f10-rebase"));
        assert!(!link_matches("s3-f1", "s3-f"));
    }

    #[test]
    fn progress_prefers_running_work_then_the_queue_then_the_newest_ended_dispatch() {
        let ended = dispatch("d1", Some("kiln"), Some(Settlement::Succeeded), true);
        let running = dispatch("d22", Some("kiln-fix1"), None, false);
        let other = dispatch("d333", Some("kilns"), None, false);

        let all = [ended.clone(), running.clone(), other.clone()];
        let ItemProgress::Active { name, dispatches, running, .. } = ItemProgress::of("kiln", &all, &[]) else {
            panic!("running work wins");
        };
        assert_eq!((name.as_str(), dispatches, running), ("kiln-fix1", 2, 1));

        let queued = QueuedFire {
            seq: 1,
            dispatch_id: "d4444".to_string(),
            role: "reviewer".to_string(),
            from: None,
            at: None,
            name: Some("kiln-review".to_string()),
            agent: None,
            trust_workspace: false,
            task: String::new(),
            queued_at: String::new(),
            claimed_by: None,
        };
        let waiting = ItemProgress::of("kiln", std::slice::from_ref(&ended), std::slice::from_ref(&queued));
        assert_eq!(waiting.summary(), "kiln-review queued");

        let finished = ItemProgress::of("kiln", std::slice::from_ref(&ended), &[]);
        assert_eq!(finished.summary(), "worker kiln succeeded, released");
        assert!(finished.looks_done());

        let kept = dispatch("d1", Some("kiln"), Some(Settlement::Succeeded), false);
        assert!(!ItemProgress::of("kiln", &[kept], &[]).looks_done(), "not released yet");
        assert_eq!(ItemProgress::of("glaze", &all, &[]), ItemProgress::NoDispatch);
    }

    #[test]
    fn a_checklist_saved_before_links_existed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("run.json"), r#"{"items": ["a", "b"], "checked": [true]}"#).unwrap();
        let store = ChecklistStore::for_dir(Some(dir.path().to_path_buf()));
        let checklist = store.load("run").unwrap();
        assert_eq!(checklist.link(0), None);

        let checklist = store.set_checked("run", 2, true).unwrap();
        assert_eq!(checklist.checked, vec![true, true]);
        let checklist = store.set_link("run", 2, Some("b".to_string())).unwrap();
        assert_eq!(checklist.links, vec![None, Some("b".to_string())]);
        assert!(store.set_link("run", 3, None).is_err());
    }
}
