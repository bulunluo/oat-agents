// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use clap::Parser;
use common::{PluginBuilder, TempRepo, TestWorld};
use oat_agents::error::{codes, to_failure};
use oat_agents::role::memory::InMemoryCatalogBuilder;
use oat_agents::{execute, Cli};
use serde_json::Value;
use std::path::Path;

fn parse(args: &[&str]) -> Cli {
    let mut full = vec!["oat-agents"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap()
}

/// Every command here reads its roles from the Run's plugin snapshot; the catalog `execute`
/// takes is not consulted.
fn run(cli: Cli, env: &dyn oat_agents::environment::Environment) -> anyhow::Result<Value> {
    execute(cli, env, &InMemoryCatalogBuilder::new().build())
}

/// A plugin whose `worker` defaults to one Dispatch at a time and whose `reviewer` has no
/// limit.
fn install_limited_plugin(world: &TestWorld, repo: &Path) {
    PluginBuilder::new(repo, "fixture-plugin", "fixture-plugin")
        .role("worker", "fresh", "Throw the pots.")
        .max_concurrent("worker", 1)
        .role("reviewer", "existing", "Inspect the pots.")
        .core("oat-meta", "Coordinate the pottery workshop.")
        .core("oat-console", "Observe the pottery workshop.")
        .finish(world, repo, "fixture-plugin", "fixture-plugin");
}

fn meta_fire(world: &TestWorld, repo: &Path, name: &str) -> anyhow::Result<Value> {
    run(
        parse(&[
            "meta", "fire", "--prompt", "plan", "--repo", &repo.to_string_lossy(), "--name", name,
            "--agent", "claude",
        ]),
        &world.env(),
    )
}

fn fire_worker(world: &TestWorld, run_id: &str, name: &str) -> Value {
    let env = world.env().with_var("OAT_RUN_ID", run_id);
    run(parse(&["role", "fire", "worker", "--name", name, "--prompt", "throw a pot"]), &env).unwrap()
}

fn settle(world: &TestWorld, run_id: &str, dispatch_id: &str) {
    let env = world.env().with_var("OAT_RUN_ID", run_id).with_var("OAT_DISPATCH_ID", dispatch_id);
    run(parse(&["dispatch", "done", "--report", "thrown"]), &env).unwrap();
}

fn log_events(world: &TestWorld, run_id: &str) -> Vec<String> {
    let log = run(parse(&["log", "show", "--run", run_id, "--format", "json"]), &world.env()).unwrap();
    log["entries"].as_array().unwrap().iter().map(|e| e["event"].as_str().unwrap().to_string()).collect()
}

#[test]
#[cfg(unix)]
fn a_failed_tmux_start_leaves_no_dispatch_log_entry_or_held_slot() {
    use oat_agents::store::Store;
    use std::os::unix::fs::PermissionsExt;

    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    run(
        parse(&[
            "meta", "fire", "--headless", "--prompt", "plan", "--repo",
            &repo.path().to_string_lossy(), "--name", "kiln", "--agent", "claude",
        ]),
        &world.env(),
    ).unwrap();
    let before = log_events(&world, "kiln");
    let failing_tmux = world.scratch.path().join("failing-tmux.sh");
    std::fs::write(
        &failing_tmux,
        "#!/bin/sh\nif [ \"$3\" = new-session ]; then\n  echo 'start refused' >&2\n  exit 1\nfi\nexit 0\n",
    ).unwrap();
    std::fs::set_permissions(&failing_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = world.env()
        .with_var("OAT_RUN_ID", "kiln")
        .with_var("OAT_TMUX_COMMAND", failing_tmux.to_string_lossy().to_string());
    let error = run(
        parse(&["role", "fire", "worker", "--name", "bowl", "--prompt", "throw a pot"]),
        &env,
    ).unwrap_err();
    let failure = to_failure(&error);
    assert_eq!(failure.code, codes::INTERNAL_ERROR);
    assert!(failure.message.contains("tmux new-session failed: start refused"), "{failure:?}");

    let store = Store::open(&env).unwrap();
    let dispatches = store.list_dispatches("kiln");
    assert_eq!(dispatches.len(), 1, "only the headless meta Dispatch remains: {dispatches:?}");
    assert_eq!(dispatches[0].role, "oat-meta");
    assert_eq!(log_events(&world, "kiln"), before, "a failed start must not log agent_enter");
    let worktree = std::path::PathBuf::from(format!("{}.oat-kiln-bowl", repo.path().display()));
    assert!(!worktree.exists(), "a failed start must remove its fresh worktree");

    let launched = fire_worker(&world, "kiln", "bowl");
    assert!(launched["queued"].is_null(), "the failed start must free its slot: {launched}");
    assert!(launched["dispatch_id"].is_string(), "{launched}");
    assert!(worktree.exists(), "the same worktree name can be launched again");
    assert_eq!(store.list_dispatches("kiln").len(), 2);
    assert!(world.tmux_calls().iter().any(|call| call.contains("new-session")));
}

#[test]
fn a_role_at_its_plugin_default_queues_and_starts_once_a_dispatch_settles() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    let fired = meta_fire(&world, &repo.path(), "kiln").unwrap();
    assert_eq!(fired["role_limits"]["worker"], 1);

    let first = fire_worker(&world, "kiln", "bowl");
    assert!(first["queued"].is_null(), "the first worker launches at once: {first}");
    let first_id = first["dispatch_id"].as_str().unwrap().to_string();

    let second = fire_worker(&world, "kiln", "vase");
    assert_eq!(second["queued"], true, "{second}");
    assert_eq!(second["position"], 1);
    assert_eq!(second["active"], 1);
    let second_id = second["dispatch_id"].as_str().unwrap().to_string();
    let vase_worktree = std::path::PathBuf::from(format!("{}.oat-kiln-vase", repo.path().display()));
    assert!(!vase_worktree.exists(), "a queued Dispatch has no worktree yet");

    settle(&world, "kiln", &first_id);
    let meta_env = world.env().with_var("OAT_RUN_ID", "kiln");
    let delivery = run(parse(&["run", "wait", "--ack", "--timeout-ms", "2000"]), &meta_env).unwrap();
    assert_eq!(delivery["messages"][0]["kind"], "member_done");
    assert_eq!(delivery["started_from_queue"], serde_json::json!([second_id]));

    let shown = run(parse(&["dispatch", "show", "--dispatch", &second_id]), &meta_env).unwrap();
    assert_eq!(shown["dispatch"]["role"], "worker", "{shown}");
    assert!(vase_worktree.exists(), "the Dispatch got its worktree when it started");

    let events = log_events(&world, "kiln");
    for expected in ["dispatch_queued", "dispatch_dequeued"] {
        assert!(events.contains(&expected.to_string()), "log is missing '{expected}': {events:?}");
    }
}

#[test]
fn the_repository_overrides_the_plugin_default() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    std::fs::write(repo.path().join(".oat/roles.toml"), "[worker]\nmax_concurrent = 2\n").unwrap();
    let fired = meta_fire(&world, &repo.path(), "kiln").unwrap();
    assert_eq!(fired["role_limits"]["worker"], 2);

    assert!(fire_worker(&world, "kiln", "bowl")["queued"].is_null());
    assert!(fire_worker(&world, "kiln", "vase")["queued"].is_null());
    assert_eq!(fire_worker(&world, "kiln", "urn")["queued"], true);
}

#[test]
fn releasing_a_dispatch_starts_the_next_one_waiting() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    meta_fire(&world, &repo.path(), "kiln").unwrap();

    let first_id = fire_worker(&world, "kiln", "bowl")["dispatch_id"].as_str().unwrap().to_string();
    let second_id = fire_worker(&world, "kiln", "vase")["dispatch_id"].as_str().unwrap().to_string();

    let meta_env = world.env().with_var("OAT_RUN_ID", "kiln");
    let released = run(parse(&["dispatch", "release", "--dispatch", &first_id]), &meta_env).unwrap();
    assert_eq!(released["queue"]["started"], serde_json::json!([second_id]));
}

#[test]
fn finishing_a_run_drops_what_is_still_queued() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    meta_fire(&world, &repo.path(), "kiln").unwrap();

    fire_worker(&world, "kiln", "bowl");
    let queued_id = fire_worker(&world, "kiln", "vase")["dispatch_id"].as_str().unwrap().to_string();

    let receipt = run(parse(&["meta", "finish", "--run", "kiln"]), &world.env()).unwrap();
    assert_eq!(receipt["dropped_from_queue"], serde_json::json!([queued_id]));
    assert!(log_events(&world, "kiln").contains(&"dispatch_dropped".to_string()));
}

#[test]
fn a_repository_limit_for_an_unknown_role_refuses_the_run() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    std::fs::write(repo.path().join(".oat/roles.toml"), "[potter]\nmax_concurrent = 2\n").unwrap();

    let error = meta_fire(&world, &repo.path(), "kiln").unwrap_err();
    let failure = to_failure(&error);
    assert_eq!(failure.code, codes::ROLE_LIMITS_INVALID);
    assert!(failure.message.contains("potter"), "{}", failure.message);
    let meta_worktree = std::path::PathBuf::from(format!("{}.oat-kiln-meta", repo.path().display()));
    assert!(!meta_worktree.exists(), "nothing was created for the refused Run");
}

#[test]
fn a_zero_limit_is_refused_wherever_it_is_written() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    install_limited_plugin(&world, &repo.path());
    std::fs::write(repo.path().join(".oat/roles.toml"), "[worker]\nmax_concurrent = 0\n").unwrap();
    let error = meta_fire(&world, &repo.path(), "kiln").unwrap_err();
    assert_eq!(to_failure(&error).code, codes::ROLE_LIMITS_INVALID);

    let repo = TempRepo::new();
    let world = TestWorld::new();
    PluginBuilder::new(&repo.path(), "fixture-plugin", "fixture-plugin")
        .role("worker", "fresh", "Throw the pots.")
        .max_concurrent("worker", 0)
        .with_default_core()
        .finish(&world, &repo.path(), "fixture-plugin", "fixture-plugin");
    let error = meta_fire(&world, &repo.path(), "kiln").unwrap_err();
    let failure = to_failure(&error);
    assert_eq!(failure.code, codes::PLUGIN_LOAD_FAILED);
    assert!(failure.message.contains("max_concurrent"), "{}", failure.message);
}
