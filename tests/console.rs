// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use common::{PluginBuilder, TestWorld};
use oat_agents::role::Backend;

#[test]
fn console_always_overrides_the_tmux_servers_account_scope() {
    for backend in [Backend::Claude, Backend::Codex] {
        for value in [None, Some(""), Some("configured-account")] {
            let world = TestWorld::new();
            let repo = tempfile::tempdir().unwrap();
            common::install_console_plugin(&world, repo.path(), "Observe.");
            let mut env = world.env();
            let names = ["CLAUDE_CONFIG_DIR", "OAT_CLAUDE_TOKEN_FILE", "CODEX_HOME"];
            let account = world.scratch.path().join("configured-account");
            std::fs::create_dir(&account).unwrap();
            let expected = match value {
                Some("configured-account") => account.to_string_lossy().into_owned(),
                _ => String::new(),
            };
            if value.is_some() {
                for name in names {
                    env = env.with_var(name, &expected);
                }
            }
            oat_agents::console::open(&env, repo.path(), Some(backend)).unwrap();
            let calls = world.tmux_calls();
            let call = calls.iter().find(|call| call.contains("new-session")).unwrap();
            for name in names {
                assert!(call.contains(&format!("-e {name}={expected} ")));
                assert_eq!(call.matches(&format!("-e {name}=")).count(), 1);
            }
        }
    }
}

#[test]
fn the_consoles_prompt_carries_the_catalogs_instructions_after_its_baseline() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "Watch spend and flag anything over budget.");

    let record = oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap();
    assert_eq!(record.backend, "claude");

    let dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap();

    let baseline_at = prompt.find("Never consume a Run inbox").expect("baseline present");
    let instructions_at = prompt
        .find("Watch spend and flag anything over budget.")
        .expect("catalog instructions present");
    assert!(baseline_at < instructions_at, "instructions must follow the baseline");
}

#[test]
fn a_skill_with_a_script_lands_in_the_consoles_directory_with_the_script_executable() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    PluginBuilder::new(repo.path(), "fixture-plugin", "fixture-plugin")
        .core("oat-meta", "Coordinate.")
        .core_with("oat-console", "team instructions", &["team-skill"])
        .skill(
            "team-skill",
            &[
                ("SKILL.md", b"# Team skill", false),
                ("check.sh", b"#!/bin/sh\necho ok\n", true),
            ],
        )
        .finish(&world, repo.path(), "fixture-plugin", "fixture-plugin");

    oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap();

    let dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let script = dir.join(".claude/skills/team-skill/check.sh");
    assert!(script.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the skill's script keeps its executable bit");
    }
}

#[test]
fn the_console_baseline_prohibits_the_inbox_release_teardown_and_interactive_panels() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "team instructions");

    oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap();
    let dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap().to_lowercase();

    assert!(prompt.contains("never consume a run inbox"));
    assert!(prompt.contains("never release a dispatch"));
    assert!(prompt.contains("never fire a role"));
    assert!(prompt.contains("interactive panel"));
    for plugin_role in ["planner", "worker", "reviewer"] {
        assert!(!prompt.contains(plugin_role), "baseline must name no plugin role: {plugin_role}");
    }
}

#[test]
fn opening_a_console_delivers_the_core_system_view_skill_even_when_the_catalog_declares_none() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "team instructions");

    oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap();

    let dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let skill_file = dir.join(".claude/skills/oat-system-view/SKILL.md");
    assert!(skill_file.exists(), "the core skill must be materialized even with no catalog skills");
    let contents = std::fs::read_to_string(&skill_file).unwrap();
    assert!(contents.contains("name: oat-system-view"));
}

#[test]
fn the_consoles_own_system_view_cross_reference_is_rewritten_per_backend() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "team instructions");

    oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap();
    let claude_dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let claude_prompt = std::fs::read_to_string(claude_dir.join("prompt.md")).unwrap();
    assert!(
        claude_prompt.contains("oat-system-view"),
        "the skill cross-reference is rewritten to a plain name for Claude Code: {claude_prompt}"
    );
    assert!(!claude_prompt.contains("$oat-system-view"));

    let world_codex = TestWorld::new();
    let env_codex = world_codex.env();
    let repo_codex = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world_codex, repo_codex.path(), "team instructions");
    oat_agents::console::open(&env_codex, repo_codex.path(), Some(Backend::Codex)).unwrap();
    let codex_dir =
        oat_agents::console::console_dir(&env_codex, &repo_codex.path().canonicalize().unwrap()).unwrap();
    let codex_prompt = std::fs::read_to_string(codex_dir.join("prompt.md")).unwrap();
    assert!(
        codex_prompt.contains("$oat-system-view"),
        "the skill cross-reference is left as written for Codex: {codex_prompt}"
    );
}

#[test]
fn two_consoles_opened_from_two_repositories_get_two_directories() {
    let world = TestWorld::new();
    let env = world.env();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo_a.path(), "team instructions");
    common::install_console_plugin(&world, repo_b.path(), "team instructions");

    oat_agents::console::open(&env, repo_a.path(), Some(Backend::Claude)).unwrap();
    oat_agents::console::open(&env, repo_b.path(), Some(Backend::Claude)).unwrap();

    let dir_a = oat_agents::console::console_dir(&env, &repo_a.path().canonicalize().unwrap()).unwrap();
    let dir_b = oat_agents::console::console_dir(&env, &repo_b.path().canonicalize().unwrap()).unwrap();

    assert_ne!(dir_a, dir_b);
    assert!(dir_a.exists());
    assert!(dir_b.exists());
}

#[test]
fn each_repositorys_read_only_run_listing_only_shows_its_own_runs() {
    use oat_agents::role::memory::InMemoryCatalogBuilder;
    use oat_agents::store::{RunRecord, Store};

    let world = TestWorld::new();
    let env = world.env();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();

    let store = Store::open(&env).unwrap();
    let base = RunRecord {
        id: "run-a".to_string(),
        name: "run-a".to_string(),
        repo: repo_a.path().canonicalize().unwrap().to_string_lossy().to_string(),
        base_branch: "main".to_string(),
        backend: "claude".to_string(),
        created_at: oat_agents::event_log::now_iso(),
        closed_at: None,
        plugins: Vec::new(),
        meta_worktree: None,
        meta_dispatch_id: None,
        goal: None,
        role_limits: Default::default(),
        role_settings: Default::default(),
    };
    store.create_run(&base).unwrap();
    let mut run_b = base.clone();
    run_b.id = "run-b".to_string();
    run_b.name = "run-b".to_string();
    run_b.repo = repo_b.path().canonicalize().unwrap().to_string_lossy().to_string();
    store.create_run(&run_b).unwrap();

    let catalog = InMemoryCatalogBuilder::new().build();

    let cli_a = oat_agents::Cli {
        command: oat_agents::TopCommand::Log {
            command: oat_agents::commands::log::LogCommand::Runs(oat_agents::commands::log::RunsArgs {
                repo: Some(repo_a.path().to_path_buf()),
            }),
        },
    };
    let result_a = oat_agents::execute(cli_a, &env, &catalog).unwrap();
    let runs_a = result_a["runs"].as_array().unwrap();
    assert_eq!(runs_a.len(), 1);
    assert_eq!(runs_a[0]["run_id"], "run-a");

    let cli_b = oat_agents::Cli {
        command: oat_agents::TopCommand::Log {
            command: oat_agents::commands::log::LogCommand::Runs(oat_agents::commands::log::RunsArgs {
                repo: Some(repo_b.path().to_path_buf()),
            }),
        },
    };
    let result_b = oat_agents::execute(cli_b, &env, &catalog).unwrap();
    let runs_b = result_b["runs"].as_array().unwrap();
    assert_eq!(runs_b.len(), 1);
    assert_eq!(runs_b[0]["run_id"], "run-b");
}

fn console_on(world: &TestWorld, repo: &std::path::Path, console_toml: &str) {
    PluginBuilder::new(repo, "fixture-plugin", "fixture-plugin")
        .core("oat-meta", "Coordinate.")
        .core("oat-console", "team instructions")
        .core_toml("oat-console", console_toml)
        .finish(world, repo, "fixture-plugin", "fixture-plugin");
}

#[test]
fn the_console_runs_on_the_backend_its_plugin_chooses() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    console_on(&world, repo.path(), "backend = \"codex\"\n");

    let record = oat_agents::console::open(&env, repo.path(), None).unwrap();
    assert_eq!(record.backend, "codex");
    let dir = oat_agents::console::console_dir(&env, &repo.path().canonicalize().unwrap()).unwrap();
    let script = std::fs::read_to_string(dir.join("launch.sh")).unwrap();
    assert!(script.contains("OAT_CODEX_COMMAND"), "{script}");
}

#[test]
fn a_backend_that_contradicts_the_plugins_choice_is_refused() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    console_on(&world, repo.path(), "backend = \"codex\"\n");

    let error = oat_agents::console::open(&env, repo.path(), Some(Backend::Claude)).unwrap_err();
    assert!(error.to_string().contains("Codex"), "{error}");
    oat_agents::console::open(&env, repo.path(), Some(Backend::Codex)).unwrap();
}

#[test]
fn without_a_plugins_choice_the_console_takes_the_request_or_claude_code() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "team instructions");
    assert_eq!(oat_agents::console::open(&env, repo.path(), None).unwrap().backend, "claude");

    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    common::install_console_plugin(&world, repo.path(), "team instructions");
    assert_eq!(
        oat_agents::console::open(&env, repo.path(), Some(Backend::Codex)).unwrap().backend,
        "codex"
    );
}

#[test]
fn the_repositorys_roles_toml_chooses_the_consoles_backend_and_model_over_its_plugins() {
    let world = TestWorld::new();
    let env = world.env();
    let repo = tempfile::tempdir().unwrap();
    console_on(&world, repo.path(), "backend = \"codex\"\n[model.claude]\nmodel = \"plugin-model\"\n");
    std::fs::write(
        repo.path().join(".oat/roles.toml"),
        "[oat-console]\nbackend = \"claude\"\n[oat-console.model.claude]\nreasoning_effort = \"low\"\n",
    )
    .unwrap();

    let error = oat_agents::console::open(&env, repo.path(), Some(Backend::Codex)).unwrap_err();
    assert!(error.to_string().contains(".oat/roles.toml"), "{error}");

    assert_eq!(oat_agents::console::open(&env, repo.path(), None).unwrap().backend, "claude");
    let calls = world.tmux_calls();
    assert!(calls.iter().any(|c| c.contains("OAT_MODEL=plugin-model")), "{calls:?}");
    assert!(calls.iter().any(|c| c.contains("OAT_EFFORT=low")), "{calls:?}");
}
