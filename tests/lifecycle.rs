// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use common::{TempRepo, TestWorld};
use oat_agents::error::{codes, to_failure};
use oat_agents::role::memory::InMemoryCatalogBuilder;
use oat_agents::role::{CoreRole, CoreRoleDefinition, RoleDefinition, StartLocation};
use oat_agents::{execute, Cli};
use clap::Parser;
use std::collections::BTreeMap;

fn fixture_catalog() -> oat_agents::role::memory::InMemoryCatalog {
    InMemoryCatalogBuilder::new()
        .with_role(RoleDefinition {
            name: "worker".to_string(),
            instructions: "Write the pottery this task asks for.".to_string(),
            models: BTreeMap::new(),
            skills: Vec::new(),
            start: StartLocation::Fresh,
            exec_environment: false,
            prior_verification: false,
            max_concurrent: None,
        })
        .with_role(RoleDefinition {
            name: "reviewer".to_string(),
            instructions: "Review the pottery someone else made.".to_string(),
            models: BTreeMap::new(),
            skills: Vec::new(),
            start: StartLocation::Existing,
            exec_environment: false,
            prior_verification: false,
            max_concurrent: None,
        })
        .with_core_role(
            CoreRole::Meta,
            CoreRoleDefinition {
                instructions: "Coordinate the pottery workshop.".to_string(),
                models: BTreeMap::new(),
                skills: Vec::new(),
                backend: None,
            },
        )
        .with_core_role(
            CoreRole::Console,
            CoreRoleDefinition {
                instructions: "Observe the pottery workshop.".to_string(),
                models: BTreeMap::new(),
                skills: Vec::new(),
                backend: None,
            },
        )
        .build()
}

fn parse(args: &[&str]) -> Cli {
    let mut full = vec!["oat-agents"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap()
}

fn run_json(cli: Cli, env: &dyn oat_agents::environment::Environment, catalog: &dyn oat_agents::role::RoleCatalog) -> serde_json::Value {
    execute(cli, env, catalog).unwrap()
}

#[test]
fn meta_fire_creates_a_run_a_worktree_and_a_session() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let cli = parse(&[
        "meta",
        "fire",
        "--prompt",
        "Ship the glaze feature",
        "--repo",
        &repo.path().to_string_lossy(),
        "--name",
        "glaze",
        "--agent",
        "claude",
    ]);

    let result = run_json(cli, &env, &catalog);
    let run_id = result["run_id"].as_str().unwrap().to_string();
    assert_eq!(run_id, "glaze");

    let worktree = std::path::PathBuf::from(result["worktree"].as_str().unwrap());
    assert!(worktree.exists(), "the meta-agent's worktree was created");
    assert_eq!(result["branch"], "oat/glaze/meta");

    let calls = world.tmux_calls();
    let new_session_call = calls
        .iter()
        .find(|c| c.contains("new-session"))
        .expect("a new-session call was recorded");
    assert!(
        new_session_call.contains("oat_meta-"),
        "session named after the meta hash_id: {new_session_call}"
    );
    let session = result["session"].as_str().unwrap();
    assert!(session.starts_with("oat_meta-") && new_session_call.contains(session), "{result}");
    assert!(
        result["attach"].as_str().unwrap().ends_with(&format!(" -L oat-test attach -t {session}")),
        "the output says how to attach on the private server: {result}"
    );

    // The goal is read back as the meta-agent was given it, under its old name too.
    for command in ["goal", "big-plan"] {
        let shown = run_json(parse(&[command, "show", "--run", "glaze"]), &env, &catalog);
        assert_eq!(shown["goal"], "Ship the glaze feature", "{command} show");
    }
}

#[test]
fn trust_failures_warn_in_fire_output_and_dispatch_records_without_stopping_launch() {
    use oat_agents::store::Store;
    use std::time::{Duration, Instant};

    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();
    let lock = world.home.join(".claude.json.lock");
    std::fs::create_dir(&lock).unwrap();
    let started = Instant::now();
    let meta = run_json(parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "trust-warning", "--agent", "claude", "--trust-workspace",
    ]), &env, &catalog);
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert!(meta["trust_warning"].as_str().unwrap().contains("timed out"));
    assert!(std::fs::symlink_metadata(&lock).unwrap().is_dir());
    assert!(!world.home.join(".claude.json").exists());
    std::fs::remove_dir(&lock).unwrap();

    // A read error in a member launch also remains visible and releases the lock.
    std::fs::create_dir(world.home.join(".claude.json")).unwrap();
    let role_env = world.env().with_var("OAT_RUN_ID", "trust-warning");
    let role = run_json(parse(&[
        "role", "fire", "worker", "--name", "writer", "--prompt", "do work", "--trust-workspace",
    ]), &role_env, &catalog);
    assert!(role["trust_warning"].as_str().unwrap().contains("could not trust Claude workspace"));
    assert!(!lock.exists());
    for result in [&meta, &role] {
        let recorded = Store::open(&env).unwrap().load_dispatch(
            "trust-warning", result["dispatch_id"].as_str().unwrap()).unwrap();
        assert_eq!(recorded.trust_warning.as_deref(), result["trust_warning"].as_str());
        assert!(result["session"].is_string());
    }
    assert_eq!(world.tmux_calls().iter().filter(|call| call.contains("new-session")).count(), 2);
}

#[test]
fn fresh_role_gets_its_own_child_worktree() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-a", "--agent", "claude",
    ]);
    run_json(fire, &env, &catalog);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-a");
    let role_fire = parse(&["role", "fire", "worker", "--name", "w1", "--prompt", "do work"]);
    let result = run_json(role_fire, &env_with_run, &catalog);

    let worktree = std::path::PathBuf::from(result["worktree"].as_str().unwrap());
    assert!(worktree.exists());
    assert!(worktree.to_string_lossy().contains(".oat-run-a-w1"));
    assert_eq!(result["branch"], "oat/run-a/w1");
    let session = result["session"].as_str().unwrap();
    assert!(session.starts_with("oat_worker-") && session.ends_with("-w1"), "{result}");
    assert!(result["attach"].as_str().unwrap().ends_with(&format!("attach -t {session}")), "{result}");
}

#[test]
fn role_fire_defaults_to_the_run_s_recorded_backend() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = InMemoryCatalogBuilder::new()
        .with_role(RoleDefinition {
            name: "worker".to_string(),
            instructions: "Write the pottery this task asks for.".to_string(),
            models: BTreeMap::new(),
            skills: vec![oat_agents::role::SkillRef {
                name: "glazing".to_string(),
                files: vec![oat_agents::role::SkillFile {
                    relative_path: std::path::PathBuf::from("SKILL.md"),
                    contents: b"# Glazing".to_vec(),
                    executable: false,
                }],
            }],
            start: StartLocation::Fresh,
            exec_environment: false,
            prior_verification: false,
            max_concurrent: None,
        })
        .with_core_role(
            CoreRole::Meta,
            CoreRoleDefinition {
                instructions: "Coordinate the pottery workshop.".to_string(),
                models: BTreeMap::new(),
                skills: Vec::new(),
                backend: None,
            },
        )
        .build();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-codex", "--agent", "codex",
    ]);
    run_json(fire, &env, &catalog);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-codex");
    let role_fire = parse(&["role", "fire", "worker", "--name", "w1", "--prompt", "do work"]);
    let result = run_json(role_fire, &env_with_run, &catalog);

    let worktree = std::path::PathBuf::from(result["worktree"].as_str().unwrap());
    let dispatch_dir = std::path::PathBuf::from(result["dispatch_dir"].as_str().unwrap());

    let script = std::fs::read_to_string(dispatch_dir.join("launch.sh")).unwrap();
    assert!(script.contains("codex"), "launch script targets Codex: {script}");
    assert!(!script.contains("claude"), "launch script does not target Claude: {script}");

    let prompt = std::fs::read_to_string(dispatch_dir.join("prompt.md")).unwrap();
    assert!(prompt.contains("Codex"), "prompt names Codex as the backend: {prompt}");

    assert!(
        worktree.join(".agents/skills/glazing/SKILL.md").exists(),
        "the role's skill is materialized under .agents/skills/, Codex's convention"
    );
    assert!(
        !worktree.join(".claude/skills").exists(),
        "no Claude-only skill directory is created for a Codex-backed role"
    );
}

#[test]
fn existing_role_runs_in_the_named_worktree() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-b", "--agent", "claude",
    ]);
    let meta_result = run_json(fire, &env, &catalog);
    let meta_worktree = meta_result["worktree"].as_str().unwrap().to_string();

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-b");
    let role_fire = parse(&[
        "role", "fire", "reviewer", "--from", &meta_worktree, "--prompt", "review this",
    ]);
    let result = run_json(role_fire, &env_with_run, &catalog);

    assert_eq!(result["worktree"].as_str().unwrap(), meta_worktree);
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn existing_role_gets_a_new_worktree_at_the_named_commit() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-at", "--agent", "claude",
    ]);
    let meta_result = run_json(fire, &env, &catalog);
    let meta_worktree = meta_result["worktree"].as_str().unwrap().to_string();

    // An epic branch that moved on after the Run began, and that no Dispatch's worktree holds.
    git(&repo.path(), &["branch", "epic/S1-kiln", "main"]);
    let scratch = tempfile::tempdir().unwrap();
    let epic_tree = scratch.path().join("epic");
    git(&repo.path(), &["worktree", "add", "-q", &epic_tree.to_string_lossy(), "epic/S1-kiln"]);
    std::fs::write(epic_tree.join("KILN.md"), "fired\n").unwrap();
    git(&epic_tree, &["add", "."]);
    git(&epic_tree, &["commit", "-q", "-m", "kiln"]);
    let epic_head = git(&repo.path(), &["rev-parse", "epic/S1-kiln"]);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-at");
    let role_fire = parse(&[
        "role", "fire", "reviewer", "--at", "epic/S1-kiln", "--name", "s1-bigreview",
        "--prompt", "review the whole epic",
    ]);
    let result = run_json(role_fire, &env_with_run, &catalog);

    let worktree = result["worktree"].as_str().unwrap();
    assert_ne!(worktree, meta_worktree, "it gets a worktree of its own");
    assert_eq!(result["at"].as_str().unwrap(), epic_head, "the reference is resolved to a commit");
    assert_eq!(git(std::path::Path::new(worktree), &["rev-parse", "HEAD"]), epic_head);
    assert!(std::path::Path::new(worktree).join("KILN.md").exists());
}

#[test]
fn at_is_refused_for_a_fresh_role_a_missing_commit_and_beside_from() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-at-refused", "--agent", "claude",
    ]);
    let meta_result = run_json(fire, &env, &catalog);
    let meta_worktree = meta_result["worktree"].as_str().unwrap().to_string();
    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-at-refused");

    let fresh = parse(&["role", "fire", "worker", "--at", "main", "--prompt", "do work"]);
    let err = execute(fresh, &env_with_run, &catalog).unwrap_err();
    assert_eq!(to_failure(&err).code, codes::INVALID_ROLE_OPTION);

    let missing = parse(&["role", "fire", "reviewer", "--at", "no-such-branch", "--prompt", "review"]);
    let err = execute(missing, &env_with_run, &catalog).unwrap_err();
    assert_eq!(to_failure(&err).code, codes::INVALID_ROLE_OPTION);

    let both = Cli::try_parse_from([
        "oat-agents", "role", "fire", "reviewer", "--from", &meta_worktree, "--at", "main",
        "--prompt", "review",
    ]);
    assert!(both.is_err(), "--from and --at name two different places to start");
}

#[test]
fn from_is_refused_for_a_fresh_role() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-c", "--agent", "claude",
    ]);
    let meta_result = run_json(fire, &env, &catalog);
    let meta_worktree = meta_result["worktree"].as_str().unwrap().to_string();

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-c");
    let role_fire = parse(&[
        "role", "fire", "worker", "--from", &meta_worktree, "--prompt", "do work",
    ]);
    let err = execute(role_fire, &env_with_run, &catalog).unwrap_err();
    assert_eq!(to_failure(&err).code, codes::INVALID_ROLE_OPTION);
}

#[test]
fn from_is_required_for_an_existing_role() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-d", "--agent", "claude",
    ]);
    run_json(fire, &env, &catalog);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-d");
    let role_fire = parse(&["role", "fire", "reviewer", "--prompt", "review this"]);
    let err = execute(role_fire, &env_with_run, &catalog).unwrap_err();
    assert_eq!(to_failure(&err).code, codes::ROLE_SOURCE_REQUIRED);
}

#[test]
fn unknown_role_names_the_role_and_the_known_ones() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-e", "--agent", "claude",
    ]);
    run_json(fire, &env, &catalog);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-e");
    let role_fire = parse(&["role", "fire", "sculptor", "--prompt", "sculpt"]);
    let err = execute(role_fire, &env_with_run, &catalog).unwrap_err();
    let failure = to_failure(&err);
    assert_eq!(failure.code, codes::UNKNOWN_ROLE);
    assert!(failure.message.contains("sculptor"));
    assert!(failure.message.contains("worker"));
    assert!(failure.message.contains("reviewer"));
}

#[test]
fn a_silent_role_still_gets_the_preamble_and_the_protocol_before_its_instructions() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-f", "--agent", "claude",
    ]);
    run_json(fire, &env, &catalog);

    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-f");
    let role_fire = parse(&["role", "fire", "worker", "--name", "w1", "--prompt", "THE-TASK-TEXT"]);
    let result = run_json(role_fire, &env_with_run, &catalog);

    let dispatch_dir = std::path::PathBuf::from(result["dispatch_dir"].as_str().unwrap());
    let prompt = std::fs::read_to_string(dispatch_dir.join("prompt.md")).unwrap();

    let preamble_at = prompt.find("Launch").expect("preamble present");
    let protocol_at = prompt.find("Role protocol").expect("role protocol present");
    let instructions_at = prompt.find("Write the pottery this task asks for.").expect("instructions present");
    let task_at = prompt.find("THE-TASK-TEXT").expect("task present");

    assert!(preamble_at < protocol_at, "preamble before protocol");
    assert!(protocol_at < instructions_at, "protocol before instructions");
    assert!(instructions_at < task_at, "instructions before task");
}

#[test]
fn a_run_goes_from_delegation_to_finish() {
    run_goes_from_delegation_to_finish(false);
}

#[test]
fn a_headless_run_can_be_driven_externally_from_delegation_to_finish() {
    run_goes_from_delegation_to_finish(true);
}

fn run_goes_from_delegation_to_finish(headless: bool) {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let repo_path = repo.path().to_string_lossy().to_string();
    let mut fire_args = vec![
        "meta", "fire", "--prompt", "plan", "--repo", &repo_path,
        "--name", "run-g", "--agent", "claude",
    ];
    if headless {
        fire_args.push("--headless");
    }
    let meta_result = run_json(parse(&fire_args), &env, &catalog);
    let run_id = meta_result["run_id"].as_str().unwrap().to_string();
    let store = oat_agents::store::Store::open(&env).unwrap();
    let meta_dispatch_id = meta_result["dispatch_id"].as_str().unwrap();
    let meta_dispatch = store.load_dispatch(&run_id, meta_dispatch_id).unwrap();
    assert_eq!(meta_dispatch.headless, headless);
    if headless {
        assert_eq!(meta_result["headless"], true);
        assert!(meta_result.get("session").is_none());
        assert!(meta_result.get("attach").is_none());
        assert!(world.tmux_calls().is_empty(), "headless fire never invokes tmux");
        assert_eq!(meta_dispatch.role, "oat-meta");
        assert_eq!(serde_json::to_value(&meta_dispatch).unwrap()["headless"], true);
        let worktree = std::path::Path::new(&meta_dispatch.worktree);
        assert!(worktree.exists());
        assert_eq!(git(worktree, &["branch", "--show-current"]), meta_result["branch"]);
        let dispatch_dir = std::path::Path::new(meta_result["dispatch_dir"].as_str().unwrap());
        assert!(std::fs::read_to_string(dispatch_dir.join("prompt.md")).unwrap().contains("plan"));
        assert!(dispatch_dir.join("launch.sh").is_file());
    }

    let meta_env = world.env().with_var("OAT_RUN_ID", &run_id);
    let role_fire = parse(&["role", "fire", "worker", "--name", "w1", "--prompt", "do work"]);
    let role_result = run_json(role_fire, &meta_env, &catalog);
    let dispatch_id = role_result["dispatch_id"].as_str().unwrap().to_string();
    if headless {
        let member = store.load_dispatch(&run_id, &dispatch_id).unwrap();
        assert!(!member.headless);
        let calls = world.tmux_calls();
        let launches: Vec<_> = calls.iter().filter(|call| call.contains("new-session")).collect();
        assert_eq!(launches.len(), 1, "{calls:?}");
        assert!(launches[0].contains(role_result["session"].as_str().unwrap()));
        assert_eq!(
            git(std::path::Path::new(&member.worktree), &["rev-parse", "HEAD"]),
            git(std::path::Path::new(&meta_dispatch.worktree), &["rev-parse", "HEAD"]),
        );
    }

    // The worker asks a question; this blocks, so it runs on its own thread while the
    // meta-agent answers it with `run reply`.
    let worker_env_for_ask = world.env()
        .with_var("OAT_RUN_ID", &run_id)
        .with_var("OAT_DISPATCH_ID", &dispatch_id);
    let ask_cli = parse(&["dispatch", "ask", "--body", "which glaze?", "--timeout-ms", "5000"]);
    let catalog_for_thread = fixture_catalog();
    let ask_handle = std::thread::spawn(move || {
        execute(ask_cli, &worker_env_for_ask, &catalog_for_thread).unwrap()
    });

    // The meta-agent receives the question through its own `run wait`, acknowledges the
    // delivery, and only then replies to it — the Run inbox is how it learns the message id.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let wait_for_question = parse(&["run", "wait", "--ack", "--timeout-ms", "5000"]);
    let question_delivery = run_json(wait_for_question, &meta_env, &catalog);
    assert_eq!(question_delivery["timed_out"], false);
    assert_eq!(question_delivery["messages"][0]["kind"], "question");
    let question_seq = question_delivery["messages"][0]["seq"].as_u64().unwrap();

    let reply_cli = parse(&[
        "run", "reply", "--message", &question_seq.to_string(), "--prompt", "cobalt",
    ]);
    run_json(reply_cli, &meta_env, &catalog);

    let ask_result = ask_handle.join().unwrap();
    assert_eq!(ask_result["timed_out"], false);
    assert_eq!(ask_result["reply"], "cobalt");

    // The worker settles.
    let worker_env = world.env()
        .with_var("OAT_RUN_ID", &run_id)
        .with_var("OAT_DISPATCH_ID", &dispatch_id);
    let done_cli = parse(&["dispatch", "done", "--report", "shipped the glaze"]);
    run_json(done_cli, &worker_env, &catalog);

    // The meta-agent waits again, receives the member_done delivery, and acknowledges it.
    let wait_cli = parse(&["run", "wait", "--ack", "--timeout-ms", "5000"]);
    let wait_result = run_json(wait_cli, &meta_env, &catalog);
    assert_eq!(wait_result["timed_out"], false);
    assert_eq!(wait_result["messages"][0]["kind"], "member_done");

    // The meta-agent releases the Dispatch and its worktree, then finishes the Run.
    let release_cli = parse(&["dispatch", "release", "--dispatch", &dispatch_id, "--remove-worktree", "--force"]);
    run_json(release_cli, &meta_env, &catalog);

    let calls_before_finish = world.tmux_calls();
    let finish_cli = parse(&["meta", "finish"]);
    let finish_result = run_json(finish_cli, &meta_env, &catalog);
    assert!(finish_result["closed_at"].is_string());
    if headless {
        assert_eq!(world.tmux_calls(), calls_before_finish, "no meta session to kill");
        assert!(!store.load_run(&run_id).unwrap().is_open());
        let meta = store.load_dispatch(&run_id, meta_dispatch_id).unwrap();
        assert!(meta.is_settled());
        assert!(meta.released_at.is_some());
    }

    let log_cli = parse(&["log", "show", "--run", &run_id, "--format", "json"]);
    let log_result = run_json(log_cli, &meta_env, &catalog);
    let events: Vec<String> = log_result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_string())
        .collect();
    for expected in ["run_created", "agent_enter", "inbox_message", "inbox_reply", "agent_exit", "run_closed"] {
        assert!(events.contains(&expected.to_string()), "log is missing '{expected}': {events:?}");
    }
    let created_at = events.iter().position(|e| e == "run_created").unwrap();
    let closed_at = events.iter().position(|e| e == "run_closed").unwrap();
    assert!(created_at < closed_at, "run_created precedes run_closed");
}

#[test]
fn a_linked_checklist_item_shows_the_work_fired_under_its_name() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    let catalog = fixture_catalog();

    let fire = parse(&[
        "meta", "fire", "--prompt", "plan", "--repo", &repo.path().to_string_lossy(),
        "--name", "run-list", "--agent", "claude",
    ]);
    run_json(fire, &env, &catalog);
    let env_with_run = world.env().with_var("OAT_RUN_ID", "run-list");

    let update = parse(&[
        "checklist", "update", "--items", "glaze pots", "--items", "fire kiln", "--items", "sweep",
        "--link", "1=glaze", "--link", "2=kiln",
    ]);
    let result = run_json(update, &env_with_run, &catalog);
    assert_eq!(result["checklist"]["links"], serde_json::json!(["glaze", "kiln", null]));

    // `glaze-fix1` belongs to `glaze`; `glazer` does not.
    for name in ["glaze-fix1", "glazer"] {
        let role_fire = parse(&["role", "fire", "worker", "--name", name, "--prompt", "do work"]);
        run_json(role_fire, &env_with_run, &catalog);
    }

    let show = run_json(parse(&["checklist", "show"]), &env_with_run, &catalog);
    let progress = &show["progress"];
    assert_eq!(progress[0]["state"], "active");
    assert_eq!(progress[0]["name"], "glaze-fix1");
    assert_eq!(progress[0]["dispatches"], 1);
    assert_eq!(progress[1]["state"], "no_dispatch");
    assert!(progress[2].is_null(), "an unlinked item has no progress: {show}");
}

#[test]
fn a_malformed_link_changes_nothing() {
    let world = TestWorld::new();
    let env = world.env().with_var("OAT_RUN_ID", "run-list");
    let catalog = fixture_catalog();

    run_json(parse(&["checklist", "update", "--items", "glaze pots"]), &env, &catalog);
    let bad = parse(&["checklist", "update", "--items", "other", "--link", "glaze"]);
    let failure = to_failure(&execute(bad, &env, &catalog).unwrap_err());
    assert_eq!(failure.code, codes::INVALID_INPUT);

    let show = run_json(parse(&["checklist", "show"]), &env, &catalog);
    assert_eq!(show["checklist"]["items"], serde_json::json!(["glaze pots"]));
}
