// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use super::generate_id;
use super::run::clean_run;
use crate::env::integration::ExecEnvironments;
use crate::environment::Environment;
use crate::error::{codes, err};
use crate::event_log::{events, now_iso, EventLog, LogEntry};
use crate::launch::{self, LaunchSpec};
use crate::plugins;
use crate::session::tmux::Tmux;
use crate::role::{Backend, CoreRole, RoleCatalog};
use crate::store::{RunRecord, Store};
use crate::worktree;
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum MetaCommand {
    /// Start a Run from a goal: create the meta-agent's worktree and launch it in its own tmux
    /// session, unless `--headless` is set.
    Fire(FireArgs),
    /// Close a Run whose Dispatches are settled and released, and print its receipt.
    Finish(FinishArgs),
}

#[derive(Args, Debug)]
pub struct FireArgs {
    /// The goal, as text.
    #[arg(long)]
    pub prompt: Option<String>,
    /// A file holding the goal.
    #[arg(long)]
    pub input_file: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub base_branch: Option<String>,
    /// The Run's backend. Defaults to `backend` under `[oat-meta]` in `.oat/roles.toml`, then
    /// Claude Code.
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub trust_workspace: bool,
    /// The execution profile this Run's roles should use. Defaults to the repository's
    /// `default_profile`; `--no-exec` overrides either and gives the Run none at all.
    #[arg(long, value_name = "NAME")]
    pub exec_profile: Option<String>,
    #[arg(long)]
    pub no_exec: bool,
    /// Prepare the Run for an external driver without starting a meta-agent tmux session.
    #[arg(long)]
    pub headless: bool,
}

#[derive(Args, Debug)]
pub struct FinishArgs {
    #[arg(long)]
    pub run: Option<String>,
}

pub fn run(command: MetaCommand, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    match command {
        MetaCommand::Fire(args) => fire(args, env),
        MetaCommand::Finish(args) => finish(args, env, exec),
    }
}

fn fire(args: FireArgs, env: &dyn Environment) -> Result<Value> {
    let goal = crate::resolve_text_input(&args.prompt, &args.input_file)?;
    let repo = args
        .repo
        .canonicalize()
        .map_err(|e| err(codes::INVALID_INPUT, format!("invalid --repo: {e}")))?;

    // The gate (ticket Scope): an uninitialised repository or an untrusted plugin stops the Run
    // before a worktree, a branch or a run record exists.
    let (resolved, catalog) = plugins::gate(&repo, env)?;
    let catalog = &catalog;
    let core_role = catalog.core_role(CoreRole::Meta)?;
    // Settled once, with the plugins: a Run keeps the limits and the launch settings it
    // started with.
    let repo_roles = crate::role::repository::load(&repo, catalog)?;
    let role_limits = crate::concurrency::resolve_limits(&repo_roles, catalog)?;
    let meta_settings = repo_roles.launch_for(CoreRole::Meta.name());
    let backend = match &args.agent {
        Some(agent) => Backend::from_str(agent)?,
        None => meta_settings.backend.unwrap_or(Backend::Claude),
    };

    let run_name = args
        .name
        .clone()
        .unwrap_or_else(|| generate_id("run"));
    let run_id = worktree::sanitize_segment(&run_name);

    let base_branch = match args.base_branch.clone() {
        Some(b) => b,
        None => worktree::default_branch(&repo)?,
    };
    let base_commit_output = worktree::run_git(&repo, &["rev-parse", &base_branch])?;
    if !base_commit_output.status.success() {
        return Err(err(
            codes::INVALID_INPUT,
            format!("unknown base branch '{base_branch}'"),
        ));
    }
    let base_commit = String::from_utf8_lossy(&base_commit_output.stdout)
        .trim()
        .to_string();

    let branch = worktree::branch_name(&run_id, "meta");
    let path = worktree::worktree_path(&repo, &run_id, "meta");
    worktree::create_worktree(&repo, &path, &branch, &base_commit)?;

    if backend == Backend::Claude {
        let _ = launch::write_claude_local_settings(&path);
        if args.trust_workspace {
            let _ = launch::trust_claude_workspace(env, &path);
        }
    }

    let store = Store::open(env)?;
    let log = EventLog::open(env);
    let dispatch_id = generate_id("dispatch");

    // Every later catalog for this Run is built from this copy, not by re-reading
    // `.oat/plugins.toml` (ticket Architecture: "Snapshot").
    let plugin_records = plugins::snapshot::snapshot_plugins(&resolved, &store.run_plugin_snapshot_dir(&run_id))?;

    let run_record = RunRecord {
        id: run_id.clone(),
        name: run_id.clone(),
        repo: repo.to_string_lossy().to_string(),
        base_branch: base_branch.clone(),
        backend: format!("{backend:?}").to_lowercase(),
        created_at: now_iso(),
        closed_at: None,
        plugins: plugin_records.clone(),
        meta_worktree: Some(path.to_string_lossy().to_string()),
        meta_dispatch_id: Some(dispatch_id.clone()),
        goal: Some(goal.clone()),
        role_limits,
        role_settings: repo_roles.launch,
    };
    store.create_run(&run_record)?;

    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: None,
        agent: None,
        event: events::RUN_CREATED.to_string(),
        details: Some(json!({"repo": run_record.repo, "base_branch": base_branch, "goal": goal})),
    })?;
    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: None,
        agent: None,
        event: events::PLUGINS_RESOLVED.to_string(),
        details: Some(json!({"plugins": plugin_records})),
    })?;

    let exec = select_exec_profile(env, &repo, &run_id, &args, catalog)?;
    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: None,
        agent: None,
        event: match exec.profile {
            Some(_) => events::EXEC_PROFILE_SELECTED,
            None => events::EXEC_PROFILE_NONE,
        }
        .to_string(),
        details: Some(exec.to_json()),
    })?;

    let model = meta_settings.model_for(backend, &core_role.models);

    let spec = LaunchSpec {
        run: run_record.clone(),
        role_label: CoreRole::Meta.name().to_string(),
        name: None,
        is_core: true,
        worktree: path.clone(),
        branch: branch.clone(),
        backend,
        baseline: crate::launch::prompt::OAT_META_BASELINE.to_string(),
        instructions: vec![core_role.instructions.clone()],
        task: goal,
        skills: core_role.skills.clone(),
        model: model.clone(),
        role_names_for_preamble: catalog.role_names(),
        created_fresh_worktree: true,
        repo: repo.clone(),
    };

    let model_env = (model.model.clone(), model.reasoning_effort.clone());
    let (dispatch, dispatch_dir) = if args.headless {
        launch::prepare_dispatch(&store, &log, &spec, dispatch_id, model_env, true)?
    } else {
        launch::launch_dispatch(env, &store, &log, spec, dispatch_id, model_env)?
    };

    let mut result = json!({
        "run_id": run_id,
        "dispatch_id": dispatch.id,
        "worktree": dispatch.worktree,
        "branch": dispatch.branch,
        "dispatch_dir": dispatch_dir.to_string_lossy(),
        "exec": exec.to_json(),
        "backend": run_record.backend,
        "role_limits": run_record.role_limits,
        "role_settings": run_record.role_settings,
    });
    if args.headless {
        result["headless"] = json!(true);
    } else {
        let session = Tmux::session_name(&dispatch.hash_id(&run_record));
        result["attach"] = json!(Tmux::from_env(env).attach_command(&session));
        result["session"] = json!(session);
    }
    Ok(result)
}

/// Which execution profile a Run got, and why — reported in `meta fire`'s output and the
/// workflow log, so a Run whose roles fall back to the host is never a silent one.
struct ExecSelection {
    profile: Option<String>,
    /// `--exec-profile`, `default_profile`, `--no-exec`, or `none configured`.
    source: &'static str,
    /// Roles of this Run that declare `exec_environment = true`.
    roles_wanting: Vec<String>,
}

impl ExecSelection {
    fn warning(&self) -> Option<String> {
        if self.profile.is_some() || self.roles_wanting.is_empty() || self.source == "--no-exec" {
            return None;
        }
        Some(format!(
            "this Run has no execution profile, so {} will run commands on the host; \
             pass --exec-profile <name> or set default_profile in .oat/exec.toml",
            self.roles_wanting.join(", ")
        ))
    }

    fn to_json(&self) -> Value {
        json!({
            "profile": self.profile,
            "source": self.source,
            "roles_wanting_environment": self.roles_wanting,
            "warning": self.warning(),
        })
    }
}

/// The Run's execution profile is chosen once, here, and every role launch inherits it:
/// `meta fire` selects it explicitly, from the configured default, or not at all with
/// `--no-exec`. Whether any given role actually gets an environment is still that role's own
/// `exec_environment` field (ADR-0001's consequence: no role name is consulted here).
fn select_exec_profile(
    env: &dyn Environment,
    repo: &std::path::Path,
    run_id: &str,
    args: &FireArgs,
    catalog: &dyn RoleCatalog,
) -> Result<ExecSelection> {
    let roles_wanting = catalog
        .role_names()
        .into_iter()
        .filter(|name| catalog.role(name).is_ok_and(|role| role.exec_environment))
        .collect();
    if args.no_exec {
        return Ok(ExecSelection { profile: None, source: "--no-exec", roles_wanting });
    }
    let exec_config = crate::env::config::load(env, Some(repo))?;
    match crate::env::config::resolve(&exec_config, args.exec_profile.as_deref()) {
        Ok((profile_name, _)) => {
            crate::env::state::EnvStore::open(env)?.save_run_profile(run_id, &profile_name)?;
            let source = if args.exec_profile.is_some() { "--exec-profile" } else { "default_profile" };
            Ok(ExecSelection { profile: Some(profile_name), source, roles_wanting })
        }
        Err(error) if args.exec_profile.is_some() => Err(error),
        Err(_) => Ok(ExecSelection { profile: None, source: "none configured", roles_wanting }),
    }
}

fn finish(args: FinishArgs, env: &dyn Environment, exec: &dyn ExecEnvironments) -> Result<Value> {
    let run_id = args
        .run
        .or_else(|| env.var("OAT_RUN_ID"))
        .ok_or_else(|| err(codes::RUN_NOT_BOUND, "no run bound; pass --run or set OAT_RUN_ID"))?;

    let store = Store::open(env)?;
    let log = EventLog::open(env);
    let mut run_record = store.load_run(&run_id)?;
    if !run_record.is_open() {
        return Err(err(codes::ALREADY_SETTLED, format!("run '{run_id}' is already closed")));
    }

    run_record.closed_at = Some(now_iso());
    store.save_run(&run_record)?;

    // A closed Run launches nothing more; what was still waiting for a place is dropped and
    // said so.
    let dropped = crate::concurrency::Queue::for_run(&store, &run_id).drop_waiting()?;
    for entry in &dropped {
        log.record(&LogEntry {
            timestamp: now_iso(),
            run_id: run_id.clone(),
            dispatch_id: Some(entry.dispatch_id.clone()),
            agent: Some(entry.role.clone()),
            event: events::DISPATCH_DROPPED.to_string(),
            details: Some(json!({"role": entry.role, "name": entry.name})),
        })?;
    }

    let meta_dispatch_id = run_record.meta_dispatch_id.clone();
    let mut headless = false;
    if let Some(dispatch_id) = &meta_dispatch_id {
        if let Ok(mut dispatch) = store.load_dispatch(&run_id, dispatch_id) {
            headless = dispatch.headless;
            if dispatch.settled.is_none() {
                dispatch.settled = Some(crate::store::Settlement::Succeeded);
                dispatch.released_at = Some(now_iso());
                store.save_dispatch(&dispatch)?;
            }
        }
    }

    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: meta_dispatch_id.clone(),
        agent: Some(CoreRole::Meta.name().to_string()),
        event: events::AGENT_EXIT.to_string(),
        details: None,
    })?;
    log.record(&LogEntry {
        timestamp: now_iso(),
        run_id: run_id.clone(),
        dispatch_id: None,
        agent: None,
        event: events::RUN_CLOSED.to_string(),
        details: None,
    })?;

    let cleanup = clean_run(env, &store, &run_id, false)?;
    // A closed Run has no Dispatch left to run a command in, so its pods are only cost.
    let environments = exec.remove_run(env, &log, &run_id);
    let receipt = json!({
        "run_id": run_id,
        "closed_at": run_record.closed_at,
        "cleanup": cleanup,
        "environments": environments,
        "dropped_from_queue": dropped.iter().map(|entry| &entry.dispatch_id).collect::<Vec<_>>(),
    });

    // The receipt above is what settles the Run; killing the meta-agent's own tmux session
    // is the last thing that happens, after cleanup is scheduled, never before.
    if let Some(dispatch_id) = meta_dispatch_id.as_ref().filter(|_| !headless) {
        let tmux = crate::session::tmux::Tmux::from_env(env);
        let hid = crate::event_log::hash_id("meta", &run_record.name, dispatch_id);
        let session = crate::session::tmux::Tmux::session_name(&hid);
        let _ = tmux.kill_session(&session);
    }

    Ok(receipt)
}
