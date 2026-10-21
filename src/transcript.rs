// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

//! Reads a Dispatch's own transcript, for `dispatch read` and for the liveness report a timed
//! out `run wait` carries.
//!
//! **Verified, not assumed** (ticket §4.4): this locates Claude Code's own session files under
//! its `projects` directory; Codex keeps no equivalent per-worktree transcript on this
//! backend's own disk layout, so a Codex-backed Dispatch has no transcript to read here and
//! its liveness degrades to what the workflow log alone says; its spend is read from Codex's
//! own session files by `codex_session`. This is stated once, not
//! invented as backend-independent anywhere in this crate.

use crate::environment::Environment;
use crate::event_log::epoch_millis;
use crate::store::DispatchRecord;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn slug(worktree_path: &str) -> String {
    worktree_path
        .chars()
        .map(|c| if matches!(c, '/' | '.') { '-' } else { c })
        .collect()
}

pub fn claude_projects_dir(env: &dyn Environment) -> Option<PathBuf> {
    env.var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env.home_dir().map(|home| home.join(".claude")))
        .map(|config| config.join("projects"))
}

fn project_dir(projects_root: &Path, worktree_path: &str) -> Option<PathBuf> {
    let exact = projects_root.join(slug(worktree_path));
    if exact.is_dir() {
        return Some(exact);
    }
    None
}

/// The newest `.jsonl` session file under a worktree's Claude Code project directory, for a
/// session the Claude backend runs. Any other backend has none: its worktree may be shared
/// with a Claude-backed Dispatch — a reviewer reading a worker's branch — whose transcript
/// would otherwise be passed off as its own.
pub fn find_transcript(projects_root: &Path, backend: &str, worktree_path: &str) -> Option<PathBuf> {
    if !matches!(backend.parse(), Ok(crate::role::Backend::Claude)) {
        return None;
    }
    let project = project_dir(projects_root, worktree_path)?;
    fs::read_dir(&project)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
}

pub fn read_transcript(
    env: &dyn Environment,
    dispatch: &DispatchRecord,
    limit: usize,
) -> Result<Option<String>> {
    let Some(projects_root) = claude_projects_dir(env) else {
        return Ok(None);
    };
    let Some(path) = find_transcript(&projects_root, &dispatch.backend, &dispatch.worktree) else {
        return Ok(None);
    };
    let rows = render(&path, limit);
    Ok(Some(
        rows.into_iter()
            .map(|r| format!("{} {:?} {}", r.time, r.kind, r.text))
            .collect::<Vec<_>>()
            .join("\n"),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowKind {
    Prompt,
    Thinking,
    Text,
    Tool,
    Result,
    /// Typed at the session after its task — by an operator on the live tab.
    You,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub time: String,
    pub kind: RowKind,
    pub text: String,
}

pub fn render(path: &Path, limit: usize) -> Vec<Row> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let mut saw_prompt = false;
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Claude Code's own injected notes, which nobody said.
        if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let time = entry.get("timestamp").and_then(Value::as_str).unwrap_or_default().to_string();
        let role = entry.pointer("/message/role").and_then(Value::as_str).unwrap_or_default();
        match entry.pointer("/message/content") {
            // The first thing typed at a session is its task; anything typed later is the
            // operator talking to it.
            Some(Value::String(text)) if role == "user" => {
                let kind = if saw_prompt { RowKind::You } else { RowKind::Prompt };
                saw_prompt = true;
                push(&mut rows, &time, kind, text);
            }
            Some(Value::String(text)) => push(&mut rows, &time, RowKind::Text, text),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => push(
                            &mut rows,
                            &time,
                            if role == "user" { RowKind::Prompt } else { RowKind::Text },
                            block.get("text").and_then(Value::as_str).unwrap_or_default(),
                        ),
                        Some("thinking") => push(
                            &mut rows,
                            &time,
                            RowKind::Thinking,
                            block.get("thinking").and_then(Value::as_str).unwrap_or_default(),
                        ),
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = block
                                .get("input")
                                .map(|value| condense(&value.to_string(), 160))
                                .unwrap_or_default();
                            push(&mut rows, &time, RowKind::Tool, &format!("{name} {input}"));
                        }
                        Some("tool_result") => {
                            let text = match block.get("content") {
                                Some(Value::String(t)) => t.clone(),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            push(&mut rows, &time, RowKind::Result, &condense(&text, 240));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if rows.len() > limit {
        rows.split_off(rows.len() - limit)
    } else {
        rows
    }
}

fn push(rows: &mut Vec<Row>, time: &str, kind: RowKind, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    rows.push(Row {
        time: time.to_string(),
        kind,
        text: trimmed.to_string(),
    });
}

/// A tool's input or output on one line, cut to `limit` characters: the log reads it as the
/// context behind a turn, not as the turn itself.
fn condense(text: &str, limit: usize) -> String {
    let single: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let trimmed = single.trim();
    if trimmed.chars().count() > limit {
        let head: String = trimmed.chars().take(limit - 3).collect();
        format!("{head}...")
    } else {
        trimmed.to_string()
    }
}

/// What the newest entry says the agent is doing right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Doing {
    Tool(String),
    Turn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Liveness {
    Working,
    Waiting,
    Stalled,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Working => "working",
            Liveness::Waiting => "waiting",
            Liveness::Stalled => "stalled",
        }
    }
}

pub const QUIET_AFTER_MS: i64 = 60_000;
pub const OVERRUN_AFTER_MS: i64 = 1_200_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pulse {
    pub at: i64,
    pub doing: Doing,
}

impl Pulse {
    pub fn silent_ms(&self, now_ms: i64) -> i64 {
        (now_ms - self.at).max(0)
    }

    pub fn liveness(&self, now_ms: i64, quiet_after_ms: i64) -> Liveness {
        if matches!(self.doing, Doing::Tool(_)) {
            return Liveness::Working;
        }
        if self.silent_ms(now_ms) <= quiet_after_ms {
            Liveness::Working
        } else {
            Liveness::Stalled
        }
    }

    pub fn overrunning(&self, now_ms: i64, quiet_after_ms: i64, overrun_after_ms: i64) -> bool {
        matches!(self.doing, Doing::Tool(_))
            && self.liveness(now_ms, quiet_after_ms) != Liveness::Stalled
            && self.silent_ms(now_ms) > overrun_after_ms
    }

    pub fn outstanding(&self) -> Option<String> {
        match &self.doing {
            Doing::Tool(name) => Some(name.clone()),
            Doing::Turn => None,
        }
    }
}

/// Reads the newest timestamp and whether a tool call is still outstanding: a `tool_use`
/// block with no matching `tool_result` after it means the file's silence is that tool
/// running, not the agent stopping.
pub fn pulse_from(contents: &str) -> Option<Pulse> {
    let mut at: Option<i64> = None;
    let mut pending: Vec<(String, String)> = Vec::new();
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(stamp) = entry.get("timestamp").and_then(Value::as_str).and_then(epoch_millis) {
            at = Some(at.map_or(stamp, |known: i64| known.max(stamp)));
        }
        let Some(Value::Array(blocks)) = entry.pointer("/message/content") else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    pending.push((id.to_string(), name.to_string()));
                }
                Some("tool_result") => {
                    let id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                    pending.retain(|(known, _)| known != id);
                }
                _ => {}
            }
        }
    }
    Some(Pulse {
        at: at?,
        doing: match pending.first() {
            Some((_, name)) => Doing::Tool(name.clone()),
            None => Doing::Turn,
        },
    })
}

pub fn pulse_for(env: &dyn Environment, dispatch: &DispatchRecord) -> Option<Pulse> {
    let projects_root = claude_projects_dir(env)?;
    let path = find_transcript(&projects_root, &dispatch.backend, &dispatch.worktree)?;
    let contents = fs::read_to_string(path).ok()?;
    pulse_from(&contents)
}

/// What one transcript says about its session: what it spent, what it is doing, what it runs
/// as, and how full its context is — the live view's roster reads all four at once.
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub usage: crate::pricing::Usage,
    pub pulse: Option<Pulse>,
    /// The model of the most recent turn, which is what the agent runs on now.
    pub model: Option<String>,
    /// Reasoning effort of the most recent turn.
    pub effort: Option<String>,
    /// Everything the most recent turn was given to read — fresh, written to cache, or read
    /// back from it: how full the context is now, which cumulative usage does not say.
    pub context: Option<u64>,
}

impl Session {
    /// The share of its context window the newest turn occupies, as a percentage. `None` for a
    /// model the pricing table does not know: a share of a guessed window would mislead.
    pub fn context_share(&self) -> Option<f64> {
        let window = crate::pricing::context_window(self.model.as_deref()?)?;
        Some(self.context? as f64 / window as f64 * 100.0)
    }
}

/// Reads a session's spend, pulse, model and effort from its transcript in one pass.
pub fn session(path: &Path) -> Session {
    let Ok(contents) = fs::read_to_string(path) else {
        return Session::default();
    };
    let mut session = Session {
        usage: crate::usage::usage_from_transcript(path),
        pulse: pulse_from(&contents),
        ..Session::default()
    };
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(model) = entry.pointer("/message/model").and_then(Value::as_str) {
            session.model = Some(model.to_string());
        }
        if let Some(usage) = entry.pointer("/message/usage") {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            session.context = Some(
                count("input_tokens") + count("cache_read_input_tokens") + count("cache_creation_input_tokens"),
            );
        }
        // Both keys are usually present with one of them null, so each is read as a string
        // before falling through to the other.
        if let Some(effort) = entry
            .get("perTurnEffort")
            .and_then(Value::as_str)
            .or_else(|| entry.get("effort").and_then(Value::as_str))
        {
            session.effort = Some(effort.to_string());
        }
    }
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_honor_claude_config_dir() {
        let home = PathBuf::from("/test/home");
        let env = crate::environment::TestEnvironment::new(home.clone());
        assert_eq!(claude_projects_dir(&env), Some(home.join(".claude/projects")));
        let env = env.with_var("CLAUDE_CONFIG_DIR", "/test/custom-claude");
        assert_eq!(claude_projects_dir(&env), Some(PathBuf::from("/test/custom-claude/projects")));
    }

    #[test]
    fn a_codex_dispatch_does_not_take_the_transcript_of_a_claude_one_in_its_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = "/work/repo.oat-s3-worker";
        let project = root.path().join(slug(worktree));
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("session.jsonl"), "").unwrap();

        assert!(find_transcript(root.path(), "claude", worktree).is_some());
        assert_eq!(find_transcript(root.path(), "codex", worktree), None);
    }

    #[test]
    fn an_outstanding_tool_call_is_working_however_long_it_has_been_silent() {
        let contents = format!(
            "{}",
            json!({"type": "assistant", "timestamp": "2026-09-22T01:41:00.000Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]}})
        );
        let pulse = pulse_from(&contents).unwrap();
        assert_eq!(pulse.doing, Doing::Tool("Bash".to_string()));
        let now = pulse.at + 10_800_000;
        assert_eq!(pulse.liveness(now, QUIET_AFTER_MS), Liveness::Working);
        assert!(pulse.overrunning(now, QUIET_AFTER_MS, OVERRUN_AFTER_MS));
    }

    #[test]
    fn a_finished_turn_with_no_outstanding_tool_stalls_after_the_quiet_window() {
        let contents = format!(
            "{}\n{}",
            json!({"type": "assistant", "timestamp": "2026-09-22T01:41:00.000Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]}}),
            json!({"type": "user", "timestamp": "2026-09-22T01:41:05.000Z",
                   "message": {"role": "user", "content": [
                       {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}),
        );
        let pulse = pulse_from(&contents).unwrap();
        assert_eq!(pulse.doing, Doing::Turn);
        assert_eq!(pulse.liveness(pulse.at + 1_000, QUIET_AFTER_MS), Liveness::Working);
        assert_eq!(pulse.liveness(pulse.at + 120_000, QUIET_AFTER_MS), Liveness::Stalled);
    }
}
