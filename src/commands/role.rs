// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use super::generate_id;
use crate::env::integration::ExecEnvironments;
use crate::env::state::EnvStore;
use crate::environment::Environment;
use crate::concurrency::{Queue, QueuedFire};
use crate::error::{codes, err};
use crate::event_log::{events, now_iso, EventLog, LogEntry};
use crate::launch::{self, LaunchSpec};
use crate::plugins::snapshot::catalog_from_snapshot;
use crate::session::tmux::Tmux;
use crate::role::{Backend, RoleCatalog, RoleDefinition, StartLocation};
use crate::store::{MessageKind, RunRecord, Store};
use crate::worktree;
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum RoleCommand {
    /// Launch a member agent of a role in this Run, or queue it while the role is at its
    /// limit. Runs only inside a Run.
    Fire(FireArgs),
}

#[derive(Args, Debug)]
pub struct FireArgs {
    pub role: String,
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// For a role that starts in an existing worktree: give it a new worktree at this commit
    /// instead of an existing Dispatch's, for work nobody's worktree holds, such as a review
    /// of a whole epic branch.
    #[arg(long, conflicts_with = "from")]
    pub at: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub trust_workspace: bool,
    #[arg(long)]
    pub prompt: Option<String>,
    #[arg(long)]
    pub input_file: Option<PathBuf>,
}

pub fn run(command: RoleCommand, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    match command {
        RoleCommand::Fire(args) => fire(args, env, exec),
    }
}

fn fire(args: FireArgs, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    let run_id = env
        .var("OAT_RUN_ID")
        .ok_or_else(|| err(codes::RUN_NOT_BOUND, "role fire runs inside a Run; OAT_RUN_ID is not set"))?;
    let task = crate::resolve_text_input(&args.prompt, &args.input_file)?;

    let store = Store::open(env)?;
    let run_record: RunRecord = store.load_run(&run_id)?;

    // The Run's plugins were resolved and trust-checked once, at `meta fire` (F5's Architecture:
    // "Snapshot"); every role launch inside it rebuilds its catalog from that copy rather than
    // re-reading `.oat/plugins.toml` or re-checking trust.
    let catalog = catalog_from_snapshot(&store.run_plugin_snapshot_dir(&run_id))?;

    let at = args.at.as_deref().map(|reference| resolve_at(reference, &run_record)).transpose()?;
    let request = QueuedFire {
        seq: 0,
        dispatch_id: generate_id("dispatch"),
        role: args.role.clone(),
        from: args.from.clone(),
        at,
        name: args.name.clone(),
        agent: args.agent.clone(),
        trust_workspace: args.trust_workspace,
        task,
        queued_at: now_iso(),
        claimed_by: None,
    };
    // Whatever would refuse the launch refuses it now, not when a place comes free.
    check_request(&request, &catalog, &run_record)?;

    let Some(&limit) = run_record.role_limits.get(&args.role) else {
        return launch(env, exec, &store, &run_record, &catalog, &request);
    };

    // A limited role always goes through the queue, so a launch never overtakes one that was
    // waiting before it.
    let queue = Queue::for_run(&store, &run_id);
    let queued = queue.push(request)?;
    let drained = drain_with(env, exec, &store, &run_record, &catalog, Some(&queued.dispatch_id))?;
    if let Some(own) = drained.own {
        return own.map(|mut value| {
            value["started_from_queue"] = json!(drained.started);
            value
        });
    }

    let active = queue.active(&args.role)?;
    let position = queue
        .waiting()?
        .iter()
        .position(|entry| entry.dispatch_id == queued.dispatch_id)
        .map(|index| index + 1);
    EventLog::open(env).record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: Some(queued.dispatch_id.clone()),
        agent: Some(args.role.clone()),
        event: events::DISPATCH_QUEUED.to_string(),
        details: Some(json!({"role": args.role, "name": args.name, "max_concurrent": limit, "active": active})),
    })?;
    Ok(json!({
        "run_id": run_id,
        "dispatch_id": queued.dispatch_id,
        "role": args.role,
        "queued": true,
        "position": position,
        "max_concurrent": limit,
        "active": active,
        "started_from_queue": drained.started,
    }))
}

/// Refuses a `role fire` that could never launch: an unknown role, a start location the
/// arguments contradict, a backend or a worktree that does not exist.
fn check_request(request: &QueuedFire, catalog: &dyn RoleCatalog, run_record: &RunRecord) -> Result<()> {
    let role_def = catalog.role(&request.role)?;
    if let Some(agent) = &request.agent {
        Backend::from_str(agent)?;
    }
    match (role_def.start, &request.from, &request.at) {
        (StartLocation::Fresh, Some(_), _) => Err(err(
            codes::INVALID_ROLE_OPTION,
            format!("role '{}' starts fresh; --from is not accepted", request.role),
        )),
        (StartLocation::Fresh, None, Some(_)) => Err(err(
            codes::INVALID_ROLE_OPTION,
            format!("role '{}' starts fresh; --at is not accepted", request.role),
        )),
        (StartLocation::Existing, Some(_), Some(_)) => Err(err(
            codes::INVALID_ROLE_OPTION,
            format!("role '{}' takes --from or --at, not both", request.role),
        )),
        (StartLocation::Existing, None, None) => Err(err(
            codes::ROLE_SOURCE_REQUIRED,
            format!("role '{}' starts in an existing worktree; --from or --at is required", request.role),
        )),
        (StartLocation::Existing, Some(from), None) => existing_worktree(from, run_record).map(|_| ()),
        (StartLocation::Existing, None, Some(at)) => resolve_at(at, run_record).map(|_| ()),
        (StartLocation::Fresh, None, None) => Ok(()),
    }
}

/// The full commit id `--at` names in the Run's repository.
fn resolve_at(reference: &str, run_record: &RunRecord) -> Result<String> {
    worktree::resolve_commit(Path::new(&run_record.repo), reference)?.ok_or_else(|| {
        err(
            codes::INVALID_ROLE_OPTION,
            format!("--at '{reference}' names no commit in this Run's repository"),
        )
    })
}

fn existing_worktree(from: &Path, run_record: &RunRecord) -> Result<PathBuf> {
    let canonical = from
        .canonicalize()
        .map_err(|_| err(codes::INVALID_WORKTREE_ID, format!("no such worktree: {}", from.display())))?;
    if !worktree::is_worktree_of(Path::new(&run_record.repo), &canonical)? {
        return Err(err(
            codes::INVALID_WORKTREE_ID,
            format!("'{}' is not a worktree of this Run's repository", canonical.display()),
        ));
    }
    Ok(canonical)
}

/// What one pass over the queue did. `own` is the outcome for the `role fire` that ran the
/// pass, when its own entry was launched in it.
struct Drained {
    own: Option<Result<Value>>,
    started: Vec<String>,
    failed: Vec<String>,
}

/// Launches every queued `role fire` whose role has a free place, oldest first. Run from the
/// meta-agent's own commands — `role fire`, `run wait`, `dispatch release` — so a launch is
/// never cut short by a session being released under it.
pub fn drain_queue(env: &dyn Environment, exec: &dyn ExecEnvironments, run_id: &str) -> Result<Value> {
    let store = Store::open(env)?;
    if Queue::for_run(&store, run_id).waiting()?.is_empty() {
        return Ok(json!({"started": [], "failed": []}));
    }
    let run_record = store.load_run(run_id)?;
    let catalog = catalog_from_snapshot(&store.run_plugin_snapshot_dir(run_id))?;
    let drained = drain_with(env, exec, &store, &run_record, &catalog, None)?;
    Ok(json!({"started": drained.started, "failed": drained.failed}))
}

fn drain_with(
    env: &dyn Environment,
    exec: &dyn ExecEnvironments,
    store: &Store,
    run_record: &RunRecord,
    catalog: &dyn RoleCatalog,
    own: Option<&str>,
) -> Result<Drained> {
    let mut drained = Drained { own: None, started: Vec::new(), failed: Vec::new() };
    if !run_record.is_open() {
        return Ok(drained);
    }
    let queue = Queue::for_run(store, &run_record.id);
    let log = EventLog::open(env);
    while let Some(entry) = queue.claim_next(&run_record.role_limits)? {
        let result = launch(env, exec, store, run_record, catalog, &entry);
        queue.release_claim(&entry)?;
        if own == Some(entry.dispatch_id.as_str()) {
            drained.own = Some(result);
            continue;
        }
        match result {
            Ok(_) => {
                log.record(&LogEntry {
                    timestamp: now_iso(),
                    run_id: run_record.id.clone(),
                    dispatch_id: Some(entry.dispatch_id.clone()),
                    agent: Some(entry.role.clone()),
                    event: events::DISPATCH_DEQUEUED.to_string(),
                    details: Some(json!({"role": entry.role, "queued_at": entry.queued_at})),
                })?;
                drained.started.push(entry.dispatch_id);
            }
            Err(error) => {
                // Nobody is waiting on this launch's output any more; the meta-agent hears of
                // the failure the way it hears of everything else, through the Run inbox.
                let message = format!(
                    "the queued launch of role '{}' ({}) failed when its place came free: {error:#}",
                    entry.role,
                    entry.name.as_deref().unwrap_or("unnamed"),
                );
                store.append_inbox(&run_record.id, &entry.dispatch_id, MessageKind::LaunchFailed, &message)?;
                log.record(&LogEntry {
                    timestamp: now_iso(),
                    run_id: run_record.id.clone(),
                    dispatch_id: Some(entry.dispatch_id.clone()),
                    agent: Some(entry.role.clone()),
                    event: events::QUEUED_LAUNCH_FAILED.to_string(),
                    details: Some(json!({"role": entry.role, "error": format!("{error:#}")})),
                })?;
                drained.failed.push(entry.dispatch_id);
            }
        }
    }
    Ok(drained)
}

fn launch(
    env: &dyn Environment,
    exec: &dyn ExecEnvironments,
    store: &Store,
    run_record: &RunRecord,
    catalog: &dyn RoleCatalog,
    request: &QueuedFire,
) -> Result<Value> {
    check_request(request, catalog, run_record)?;
    let run_id = run_record.id.clone();
    let repo = PathBuf::from(&run_record.repo);
    let role_def = catalog.role(&request.role)?;
    let settings = run_record.role_settings.get(&request.role).cloned().unwrap_or_default();
    // `--agent` for this one launch, then the repository's choice for the role, then the Run's.
    let backend = match (&request.agent, settings.backend) {
        (Some(agent), _) => Backend::from_str(agent)?,
        (None, Some(backend)) => backend,
        (None, None) => Backend::from_str(&run_record.backend)?,
    };
    let task = request.task.clone();

    let launch_name = request.name.clone().unwrap_or_else(|| generate_id(&request.role));
    let dispatch_id = request.dispatch_id.clone();

    let (worktree_path, branch, created_fresh) = match (role_def.start, &request.at) {
        (StartLocation::Fresh, _) => {
            let branch = worktree::branch_name(&run_record.name, &launch_name);
            let path = worktree::worktree_path(&repo, &run_record.name, &launch_name);
            let meta_worktree = run_record
                .meta_worktree
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| repo.clone());
            let base_commit = worktree::current_commit(&meta_worktree)?;
            worktree::create_worktree(&repo, &path, &branch, &base_commit)?;
            (path, branch, true)
        }
        (StartLocation::Existing, Some(at)) => {
            let branch = worktree::branch_name(&run_record.name, &launch_name);
            let path = worktree::worktree_path(&repo, &run_record.name, &launch_name);
            worktree::create_worktree(&repo, &path, &branch, at)?;
            (path, branch, true)
        }
        (StartLocation::Existing, None) => {
            let from = request.from.as_deref().unwrap_or(Path::new(""));
            let canonical = existing_worktree(from, run_record)?;
            let branch_output = worktree::run_git(&canonical, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            let branch = String::from_utf8_lossy(&branch_output.stdout).trim().to_string();
            (canonical, branch, false)
        }
    };

    let mut trust_warning = None;
    if backend == Backend::Claude {
        let _ = launch::write_claude_local_settings(&worktree_path);
        if request.trust_workspace {
            trust_warning = launch::trust_claude_workspace(env, &worktree_path)
                .err()
                .map(|error| format!("could not trust Claude workspace: {error:#}"));
        }
    }

    let model = settings.model_for(backend, &role_def.models);

    let log = EventLog::open(env);
    let mut skills = role_def.skills.clone();
    let mut instructions = vec![role_def.instructions.clone()];
    let env_binding = bring_up_environment(env, &log, &run_id, role_def, &request.role, &worktree_path, exec)?;
    match &env_binding {
        EnvBinding::Bound(record) => {
            skills.push(crate::env::exec_environment_skill());
            instructions.push(render_execution_environment_block(record));
            if role_def.prior_verification {
                let reusable = exec.reusable(env, &worktree_path, record);
                instructions.push(render_prior_verification_block(record, &reusable));
            }
        }
        EnvBinding::Skipped(reason) => instructions.push(render_no_environment_block(reason)),
        EnvBinding::NotWanted => {}
    }

    let spec = LaunchSpec {
        run: run_record.clone(),
        role_label: request.role.clone(),
        name: dispatch_name(&request.role, request.name.as_deref(), (!created_fresh).then_some(branch.as_str())),
        is_core: false,
        worktree: worktree_path.clone(),
        branch: branch.clone(),
        backend,
        baseline: String::new(),
        instructions,
        task,
        skills,
        model: model.clone(),
        role_names_for_preamble: catalog.role_names(),
        created_fresh_worktree: created_fresh,
        repo: repo.clone(),
        trust_warning,
    };

    let (dispatch, dispatch_dir) = launch::launch_dispatch(
        env,
        store,
        &log,
        spec,
        dispatch_id.clone(),
        (model.model.clone(), model.reasoning_effort.clone()),
    )?;

    match &env_binding {
        EnvBinding::Bound(record) => {
            let mut dispatch_record = store.load_dispatch(&run_id, &dispatch.id)?;
            dispatch_record.env_id = Some(record.env_id.clone());
            dispatch_record.image_id = Some(record.image_id.clone());
            store.save_dispatch(&dispatch_record)?;
        }
        EnvBinding::Skipped(reason) => {
            let mut dispatch_record = store.load_dispatch(&run_id, &dispatch.id)?;
            dispatch_record.env_skipped = Some(reason.clone());
            store.save_dispatch(&dispatch_record)?;
            log.record(&LogEntry {
                timestamp: now_iso(),
                run_id: run_id.clone(),
                dispatch_id: Some(dispatch.id.clone()),
                agent: Some(request.role.clone()),
                event: events::ENV_SKIPPED.to_string(),
                details: Some(json!({"role": request.role, "reason": reason})),
            })?;
        }
        EnvBinding::NotWanted => {}
    }

    let session = Tmux::session_name(&dispatch.hash_id(run_record));
    Ok(json!({
        "run_id": run_id,
        "dispatch_id": dispatch.id,
        "role": request.role,
        "backend": dispatch.backend,
        "worktree": dispatch.worktree,
        "branch": dispatch.branch,
        "session": session,
        "attach": Tmux::from_env(env).attach_command(&session),
        "at": request.at,
        "dispatch_dir": dispatch_dir.to_string_lossy(),
        "trust_warning": dispatch.trust_warning,
        "exec": env_binding.to_json(),
    }))
}

/// What a role launch got in the way of an execution environment — always reported, so a role
/// that asked for one and ran on the host is visible in the launch result, the log and the view.
enum EnvBinding {
    Bound(Box<crate::env::EnvRecord>),
    Skipped(String),
    NotWanted,
}

impl EnvBinding {
    fn to_json(&self) -> Value {
        match self {
            EnvBinding::Bound(record) => json!({
                "environment": "pod",
                "profile": record.profile,
                "env_id": record.env_id,
                "pod": record.pod,
                "namespace": record.namespace,
                "image_id": record.image_id,
            }),
            EnvBinding::Skipped(reason) => json!({"environment": "host", "warning": reason}),
            EnvBinding::NotWanted => json!({"environment": "host"}),
        }
    }
}

/// Whether this Dispatch gets an execution environment is two reads of the role definition
/// (ticket Architecture): `exec_environment` decides it, and nothing else does — not the role's
/// name, not what the Run happens to be doing. A role that says `false` never gets one, even
/// when the Run carries a profile; a role that says `true` gets one only when the Run actually
/// has a profile to give it.
/// The longest a Dispatch's name gets in its hash_id, so the roster keeps a column for it.
const NAME_MAX: usize = 32;

/// What a Dispatch is called in its hash_id: the `--name` it was launched with, or, for one
/// that starts in an existing worktree, that worktree's branch. The role is already the
/// hash_id's first part, so a name that repeats it at either end drops it — `s3-f8-worker`
/// launched as a worker is `worker-1a2b-s3-f8`. `None` when there is nothing to go on.
fn dispatch_name(role: &str, name: Option<&str>, branch: Option<&str>) -> Option<String> {
    let raw = name.or_else(|| branch.map(|branch| branch.rsplit('/').next().unwrap_or(branch)))?;
    let mut name = worktree::sanitize_segment(raw);
    let role = worktree::sanitize_segment(role);
    let trimmed = name
        .strip_suffix(&format!("-{role}"))
        .or_else(|| name.strip_prefix(&format!("{role}-")))
        .map(str::to_owned);
    if let Some(trimmed) = trimmed {
        name = trimmed;
    }
    if name.len() > NAME_MAX {
        name.truncate(NAME_MAX);
        name = name.trim_end_matches('-').to_owned();
    }
    Some(name).filter(|name| !name.is_empty() && name != "run")
}

fn bring_up_environment(
    env: &dyn Environment,
    log: &crate::event_log::EventLog,
    run_id: &str,
    role_def: &RoleDefinition,
    role: &str,
    worktree: &Path,
    exec: &dyn ExecEnvironments,
) -> Result<EnvBinding> {
    if !role_def.exec_environment {
        return Ok(EnvBinding::NotWanted);
    }
    let Some(profile) = EnvStore::open(env)?.run_profile(run_id) else {
        return Ok(EnvBinding::Skipped(format!(
            "role '{role}' asks for an execution environment, but this Run has no execution profile; \
             its commands run on the host"
        )));
    };
    let record = exec.ensure(env, log, run_id, &profile, role, worktree)?;
    Ok(EnvBinding::Bound(Box::new(record)))
}

fn render_execution_environment_block(record: &crate::env::EnvRecord) -> String {
    format!(
        "<execution-environment>\nenv_id: {}\nimage_id: {}\n</execution-environment>",
        record.env_id, record.image_id
    )
}

fn render_no_environment_block(reason: &str) -> String {
    format!(
        "<execution-environment>\nnone: {reason}. Say in your report that your verification ran \
         on the host.\n</execution-environment>"
    )
}

fn render_prior_verification_block(record: &crate::env::EnvRecord, reusable: &Value) -> String {
    format!(
        "## Prior verification\n\nThe execution ledger's reusable entries for this worktree, judged against \
         environment `{}`:\n\n```json\n{}\n```",
        record.env_id,
        serde_json::to_string_pretty(reusable).unwrap_or_else(|_| "null".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::dispatch_name;

    #[test]
    fn a_dispatch_is_named_for_its_work_without_repeating_its_role() {
        assert_eq!(dispatch_name("worker", Some("s3-f10-worker"), None).as_deref(), Some("s3-f10"));
        assert_eq!(dispatch_name("worker", Some("s3-f7-fix1"), None).as_deref(), Some("s3-f7-fix1"));
        assert_eq!(dispatch_name("reviewer", Some("reviewer-S3 F8"), None).as_deref(), Some("s3-f8"));
        assert_eq!(dispatch_name("worker", Some("worker"), None).as_deref(), Some("worker"));
    }

    #[test]
    fn an_unnamed_dispatch_in_an_existing_worktree_takes_its_branch() {
        assert_eq!(
            dispatch_name("reviewer", None, Some("feature/S3-97-mac-shell-windows-print-links"))
                .as_deref(),
            Some("s3-97-mac-shell-windows-print-li")
        );
        assert_eq!(dispatch_name("worker", None, None), None, "then it is known by the Run's name");
    }
}
