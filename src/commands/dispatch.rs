// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::env::integration::ExecEnvironments;
use crate::environment::Environment;
use crate::error::{codes, err};
use crate::event_log::{events, now_iso, EventLog, LogEntry};
use crate::session::tmux::Tmux;
use crate::store::{MessageKind, Settlement, Store};
use crate::worktree;
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum DispatchCommand {
    /// Settle this Dispatch once, with an outcome and a report; the meta-agent receives it as
    /// `member_done`.
    Done(DoneArgs),
    /// Ask the meta-agent through the Run inbox and wait for its reply.
    Ask(AskArgs),
    /// A Dispatch's record and whether its session is still alive.
    Show(ShowArgs),
    /// Observe structured liveness for every Dispatch, without consuming the Run inbox.
    Pulse(PulseArgs),
    /// A Dispatch's transcript or terminal screen.
    Read(ReadArgs),
    /// End a Dispatch's session; with `--remove-worktree`, also remove its worktree.
    Release(ReleaseArgs),
}

#[derive(Args, Debug)]
pub struct DoneArgs {
    #[arg(long)]
    pub dispatch: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
    #[arg(long, default_value = "succeeded")]
    pub outcome: String,
    #[arg(long)]
    pub report: Option<String>,
    #[arg(long)]
    pub report_file: Option<std::path::PathBuf>,
}

#[derive(Args, Debug)]
pub struct AskArgs {
    #[arg(long)]
    pub dispatch: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub input_file: Option<std::path::PathBuf>,
    #[arg(long, default_value_t = 900_000)]
    pub timeout_ms: u64,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    #[arg(long)]
    pub dispatch: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
}

#[derive(Args, Debug)]
pub struct PulseArgs {
    #[arg(long)]
    pub run: String,
    /// Limit the array to this Dispatch. Omission always means the whole Run.
    #[arg(long)]
    pub dispatch: Option<String>,
}

#[derive(Args, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadSource;

#[derive(Args, Debug)]
pub struct ReadArgs {
    #[arg(long)]
    pub dispatch: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
    #[arg(long, default_value = "auto")]
    pub source: String,
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct ReleaseArgs {
    #[arg(long)]
    pub dispatch: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
    #[arg(long)]
    pub remove_worktree: bool,
    #[arg(long)]
    pub force: bool,
}

fn bound_run(run: &Option<String>, env: &dyn Environment) -> Result<String> {
    run.clone()
        .or_else(|| env.var("OAT_RUN_ID"))
        .ok_or_else(|| err(codes::RUN_NOT_BOUND, "no run bound; pass --run or set OAT_RUN_ID"))
}

fn bound_dispatch(dispatch: &Option<String>, env: &dyn Environment) -> Result<String> {
    dispatch
        .clone()
        .or_else(|| env.var("OAT_DISPATCH_ID"))
        .ok_or_else(|| {
            err(
                codes::DISPATCH_NOT_BOUND,
                "no dispatch bound; pass --dispatch or set OAT_DISPATCH_ID",
            )
        })
}

pub fn run(command: DispatchCommand, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    match command {
        DispatchCommand::Done(args) => done(args, env),
        DispatchCommand::Ask(args) => ask(args, env),
        DispatchCommand::Show(args) => show(args, env),
        DispatchCommand::Pulse(args) => pulse(args, env),
        DispatchCommand::Read(args) => read(args, env),
        DispatchCommand::Release(args) => release(args, env, exec),
    }
}

fn done(args: DoneArgs, env: &dyn Environment) -> Result<Value> {
    let run_id = bound_run(&args.run, env)?;
    let dispatch_id = bound_dispatch(&args.dispatch, env)?;
    let store = Store::open(env)?;
    let log = EventLog::open(env);
    let mut dispatch = store.load_dispatch(&run_id, &dispatch_id)?;
    if dispatch.is_settled() {
        return Err(err(
            codes::ALREADY_SETTLED,
            format!("dispatch '{dispatch_id}' is already settled"),
        ));
    }

    let report = match (&args.report, &args.report_file) {
        (Some(_), Some(_)) => {
            return Err(err(
                codes::INVALID_CLI_ARGUMENTS,
                "pass at most one of --report or --report-file",
            ))
        }
        (Some(text), None) => text.clone(),
        (None, Some(path)) if path.as_os_str() == "-" => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| err(codes::INVALID_INPUT, format!("could not read stdin: {e}")))?;
            buf
        }
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| {
            err(codes::INVALID_INPUT, format!("could not read {}: {e}", path.display()))
        })?,
        (None, None) => String::new(),
    };

    let settlement = match args.outcome.as_str() {
        "succeeded" => Settlement::Succeeded,
        "failed" => Settlement::Failed,
        other => {
            return Err(err(
                codes::INVALID_CLI_ARGUMENTS,
                format!("--outcome must be 'succeeded' or 'failed', got '{other}'"),
            ))
        }
    };

    dispatch.settled = Some(settlement);
    dispatch.report = Some(report.clone());
    store.save_dispatch(&dispatch)?;

    store.append_inbox(&run_id, &dispatch_id, MessageKind::MemberDone, &report)?;
    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: Some(dispatch_id.clone()),
        agent: Some(dispatch.role.clone()),
        event: events::AGENT_EXIT.to_string(),
        details: Some(json!({"outcome": args.outcome})),
    })?;

    Ok(json!({"run_id": run_id, "dispatch_id": dispatch_id, "settled": args.outcome}))
}

fn ask(args: AskArgs, env: &dyn Environment) -> Result<Value> {
    let run_id = bound_run(&args.run, env)?;
    let dispatch_id = bound_dispatch(&args.dispatch, env)?;
    let text = crate::resolve_text_input(&args.body, &args.input_file)?;
    let store = Store::open(env)?;
    let log = EventLog::open(env);

    let seq = store.append_inbox(&run_id, &dispatch_id, MessageKind::Question, &text)?;
    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: Some(dispatch_id.clone()),
        agent: None,
        event: events::INBOX_MESSAGE.to_string(),
        details: Some(json!({"seq": seq, "kind": "question"})),
    })?;

    let timeout = Duration::from_millis(args.timeout_ms);
    let poll = Duration::from_millis(500);
    let reply = store.wait_reply(&run_id, seq, timeout, poll)?;

    Ok(json!({
        "run_id": run_id,
        "dispatch_id": dispatch_id,
        "message": seq,
        "timed_out": reply.is_none(),
        "reply": reply.map(|r| r.message),
    }))
}

fn show(args: ShowArgs, env: &dyn Environment) -> Result<Value> {
    let run_id = bound_run(&args.run, env)?;
    let dispatch_id = bound_dispatch(&args.dispatch, env)?;
    let store = Store::open(env)?;
    let dispatch = store.load_dispatch(&run_id, &dispatch_id)?;
    let run_record = store.load_run(&run_id)?;

    let alive = if dispatch.released_at.is_some() {
        false
    } else {
        let tmux = Tmux::from_env(env);
        let hid = dispatch.hash_id(&run_record);
        let session = Tmux::session_name(&hid);
        tmux.session_alive(&session).unwrap_or(false)
    };

    Ok(json!({
        "dispatch": dispatch,
        "alive": alive,
    }))
}

fn pulse(args: PulseArgs, env: &dyn Environment) -> Result<Value> {
    let store = Store::open(env)?;
    let run = store.load_run(&args.run)?;
    let dispatches = match args.dispatch {
        Some(id) => vec![store.load_dispatch(&args.run, &id)?],
        None => store.list_dispatches(&args.run),
    };
    let waiting = store.waiting_on_meta(&args.run)?;
    let tmux = Tmux::from_env(env);
    let projects = crate::transcript::claude_projects_dir(env);
    let mut result = Vec::new();
    for dispatch in dispatches {
        let session = Tmux::session_name(&dispatch.hash_id(&run));
        let session_alive = !dispatch.headless && dispatch.released_at.is_none()
            && tmux.session_alive(&session).unwrap_or(false);
        let (pane_dead, activity) = if session_alive {
            tmux.activity(&session)
        } else {
            (None, None)
        };
        let transcript = if dispatch.headless {
            None
        } else {
            projects.as_ref().and_then(|root| {
                crate::transcript::find_transcript(root, &dispatch.backend, &dispatch.worktree)
            })
        };
        let modified = transcript.as_ref()
            .and_then(|path| std::fs::metadata(path).ok()?.modified().ok());
        result.push(json!({
            "dispatch_id": dispatch.id,
            "role": dispatch.role,
            "settled": dispatch.is_settled(),
            "session_alive": session_alive,
            "pane_dead": pane_dead,
            "last_output_at": last_output_at(activity, modified),
            "transcript_path": transcript,
            "waiting_on_meta": waiting.contains(&dispatch.id),
            "pid": dispatch.pid,
            "pgid": dispatch.pgid,
            "started_at": dispatch.started_at,
            "identity_warning": dispatch.identity_warning,
            "headless": dispatch.headless,
        }));
    }
    Ok(Value::Array(result))
}

fn last_output_at(activity: Option<i64>, modified: Option<std::time::SystemTime>) -> Option<String> {
    let activity = activity.and_then(|seconds| chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0));
    let modified = modified.map(chrono::DateTime::<chrono::Utc>::from);
    activity.max(modified).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
}

fn read(args: ReadArgs, env: &dyn Environment) -> Result<Value> {
    let run_id = bound_run(&args.run, env)?;
    let dispatch_id = bound_dispatch(&args.dispatch, env)?;
    let store = Store::open(env)?;
    let run_record = store.load_run(&run_id)?;
    let dispatch = store.load_dispatch(&run_id, &dispatch_id)?;

    let hid = dispatch.hash_id(&run_record);
    let session = Tmux::session_name(&hid);
    let tmux = Tmux::from_env(env);

    let source = args.source.as_str();
    let use_transcript = source == "transcript" || source == "auto";
    if use_transcript {
        if let Ok(Some(text)) = crate::transcript::read_transcript(env, &dispatch, args.limit) {
            return Ok(json!({"source": "transcript", "text": text}));
        }
        if source == "transcript" {
            return Err(err(codes::TRANSCRIPT_NOT_FOUND, "no transcript found for this dispatch"));
        }
    }

    if dispatch.released_at.is_none() && tmux.session_alive(&session).unwrap_or(false) {
        let text = tmux.capture_pane(&session)?;
        return Ok(json!({"source": "terminal", "text": text}));
    }

    let saved_path = store
        .dispatch_dir_path(&run_id, &dispatch_id)
        .join("screen.txt");
    if saved_path.exists() {
        let text = std::fs::read_to_string(saved_path)?;
        return Ok(json!({"source": "terminal", "text": text}));
    }

    Err(err(codes::TRANSCRIPT_NOT_FOUND, "no transcript or terminal screen available"))
}

fn release(args: ReleaseArgs, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    let run_id = bound_run(&args.run, env)?;
    let dispatch_id = bound_dispatch(&args.dispatch, env)?;
    let store = Store::open(env)?;
    let log = EventLog::open(env);
    let run_record = store.load_run(&run_id)?;
    let mut dispatch = store.load_dispatch(&run_id, &dispatch_id)?;

    let tmux = Tmux::from_env(env);
    let hid = dispatch.hash_id(&run_record);
    let session = Tmux::session_name(&hid);

    if tmux.session_alive(&session).unwrap_or(false) {
        if let Ok(text) = tmux.capture_pane(&session) {
            let _ = std::fs::write(
                store.dispatch_dir_path(&run_id, &dispatch_id).join("screen.txt"),
                text,
            );
        }
        let _ = tmux.kill_session(&session);
    }

    let mut removal = None;
    if args.remove_worktree {
        let repo = std::path::PathBuf::from(&run_record.repo);
        let path = std::path::PathBuf::from(&dispatch.worktree);
        let outcome = worktree::remove_worktree(
            &repo,
            &path,
            &dispatch.branch,
            &run_record.base_branch,
            args.force,
        )?;
        match outcome {
            worktree::RemovalOutcome::Removed => {
                log.record(&LogEntry {
                    timestamp: now_iso(),
                    run_id: run_id.clone(),
                    dispatch_id: Some(dispatch_id.clone()),
                    agent: None,
                    event: events::WORKTREE_REMOVED.to_string(),
                    details: None,
                })?;
                removal = Some(json!({"removed": true}));
            }
            worktree::RemovalOutcome::Kept { reason } => {
                removal = Some(json!({"removed": false, "kept_reason": reason}));
            }
        }
    }

    dispatch.released_at = Some(now_iso());
    store.save_dispatch(&dispatch)?;

    // The release is done whatever the queue does; a failure to start what was waiting is
    // reported beside it, not in place of it.
    let queue = super::role::drain_queue(env, exec, &run_id)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));

    Ok(json!({
        "run_id": run_id,
        "dispatch_id": dispatch_id,
        "released": true,
        "worktree": removal,
        "queue": queue,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn last_output_uses_the_newer_source_and_preserves_missing_sources() {
        let mtime: SystemTime = UNIX_EPOCH + Duration::from_secs(20);
        assert_eq!(last_output_at(Some(10), Some(mtime)).as_deref(), Some("1970-01-01T00:00:20Z"));
        assert_eq!(last_output_at(Some(30), Some(mtime)).as_deref(), Some("1970-01-01T00:00:30Z"));
        assert_eq!(last_output_at(None, Some(mtime)), last_output_at(Some(20), None));
        assert_eq!(last_output_at(None, None), None);
        assert_eq!(last_output_at(Some(i64::MAX), None), None);
    }
}
