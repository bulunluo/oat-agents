// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use clap::Parser;
use common::{TempRepo, TestWorld};
use oat_agents::environment::{Environment, TestEnvironment};
use oat_agents::launch::trust_claude_workspace;
use oat_agents::role::memory::InMemoryCatalogBuilder;
use oat_agents::{Cli, execute};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

fn run(args: &[&str], env: &dyn Environment) -> Value {
    let cli =
        Cli::try_parse_from(std::iter::once("oat-agents").chain(args.iter().copied())).unwrap();
    execute(cli, env, &InMemoryCatalogBuilder::new().build()).unwrap()
}

fn assert_trusted(config_dir: &Path, worktree: &str) {
    let root: Value =
        serde_json::from_slice(&fs::read(config_dir.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(root["projects"][worktree]["hasTrustDialogAccepted"], true);
    assert!(!config_dir.join(".claude.json.lock").exists());
}

#[test]
fn fire_trust_uses_each_launch_environment_and_passes_it_to_the_session() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let meta_config = tempfile::tempdir().unwrap();
    let member_config = tempfile::tempdir().unwrap();
    let home_config = world.home.join(".claude.json");
    let original = r#"{"keep":"home settings"}"#;
    fs::write(&home_config, original).unwrap();
    let meta_env = world
        .env()
        .with_var("OAT_CLAUDE_TOKEN_FILE", meta_config.path().join("token").to_string_lossy())
        .with_var("CODEX_HOME", meta_config.path().join("codex").to_string_lossy())
        .with_var("CLAUDE_CONFIG_DIR", meta_config.path().to_string_lossy());
    let meta = run(
        &[
            "meta",
            "fire",
            "--prompt",
            "plan",
            "--repo",
            &repo.path().to_string_lossy(),
            "--name",
            "trust-config",
            "--agent",
            "claude",
            "--trust-workspace",
        ],
        &meta_env,
    );
    assert!(meta["trust_warning"].is_null());
    assert_trusted(meta_config.path(), meta["worktree"].as_str().unwrap());
    let meta_contents = fs::read(meta_config.path().join(".claude.json")).unwrap();

    // The member has a different account from the meta-agent. TestEnvironment also
    // deliberately differs from the process environment; no global env mutation is needed.
    let member_env = world
        .env()
        .with_var("OAT_RUN_ID", "trust-config")
        .with_var("OAT_CLAUDE_TOKEN_FILE", member_config.path().join("token").to_string_lossy())
        .with_var("CODEX_HOME", member_config.path().join("codex").to_string_lossy())
        .with_var("CLAUDE_CONFIG_DIR", member_config.path().to_string_lossy());
    let member = run(
        &[
            "role",
            "fire",
            "worker",
            "--name",
            "custom",
            "--prompt",
            "work",
            "--trust-workspace",
        ],
        &member_env,
    );
    assert!(member["trust_warning"].is_null());
    assert_trusted(member_config.path(), member["worktree"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&home_config).unwrap(), original);
    assert!(!world.home.join(".claude.json.lock").exists());
    assert_eq!(
        fs::read(meta_config.path().join(".claude.json")).unwrap(),
        meta_contents
    );
    let member_contents = fs::read(member_config.path().join(".claude.json")).unwrap();

    let empty_env = member_env
        .with_var("CLAUDE_CONFIG_DIR", "")
        .with_var("OAT_CLAUDE_TOKEN_FILE", "")
        .with_var("CODEX_HOME", "");
    let fallback = run(
        &[
            "role",
            "fire",
            "worker",
            "--name",
            "fallback",
            "--prompt",
            "work",
            "--trust-workspace",
        ],
        &empty_env,
    );
    assert!(fallback["trust_warning"].is_null());
    assert_trusted(&world.home, fallback["worktree"].as_str().unwrap());
    assert_eq!(
        fs::read(member_config.path().join(".claude.json")).unwrap(),
        member_contents
    );

    let unset_env = world.env().with_var("OAT_RUN_ID", "trust-config");
    let unset = run(
        &[
            "role", "fire", "worker", "--name", "unset", "--prompt", "work",
            "--trust-workspace",
        ],
        &unset_env,
    );
    assert!(unset["trust_warning"].is_null());
    assert_trusted(&world.home, unset["worktree"].as_str().unwrap());

    let calls = world.tmux_calls();
    for (result, config_dir) in [
        (&meta, meta_config.path().to_string_lossy().into_owned()),
        (&member, member_config.path().to_string_lossy().into_owned()),
        (&fallback, String::new()),
        (&unset, String::new()),
    ] {
        let call = calls
            .iter()
            .find(|call| {
                call.contains("new-session") && call.contains(result["session"].as_str().unwrap())
            })
            .unwrap();
        assert!(call.contains(&format!("-e CLAUDE_CONFIG_DIR={config_dir} ")));
        for (name, suffix) in [("OAT_CLAUDE_TOKEN_FILE", "token"), ("CODEX_HOME", "codex")] {
            let value = if config_dir.is_empty() {
                String::new()
            } else {
                format!("{config_dir}/{suffix}")
            };
            // Explicit empty overrides prevent the shared server's earlier account from
            // leaking into a member whose oat invocation has no account configuration.
            assert!(call.contains(&format!("-e {name}={value} ")));
        }
    }
}

#[test]
fn custom_config_uses_the_same_directory_lock_and_preserves_symlinks() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let env = TestEnvironment::new(home.path().to_path_buf())
        .with_var("CLAUDE_CONFIG_DIR", config_dir.path().to_string_lossy());
    let lock = config_dir.path().join(".claude.json.lock");
    let config = config_dir.path().join(".claude.json");
    fs::create_dir(&lock).unwrap();
    thread::scope(|scope| {
        let writer = scope.spawn(|| trust_claude_workspace(&env, Path::new("workspace")));
        thread::sleep(Duration::from_millis(250));
        assert!(!writer.is_finished());
        assert!(fs::symlink_metadata(&lock).unwrap().is_dir());
        assert!(!config.exists());
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
        fs::write(&config, r#"{"keep":42}"#).unwrap();
        fs::remove_dir(&lock).unwrap();
        writer.join().unwrap().unwrap();
    });
    assert_trusted(config_dir.path(), "workspace");

    let target = config_dir.path().join("settings.json");
    fs::rename(&config, &target).unwrap();
    std::os::unix::fs::symlink("settings.json", &config).unwrap();
    fs::create_dir(&lock).unwrap();
    fs::File::open(&lock)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(11))
        .unwrap();
    trust_claude_workspace(&env, Path::new("another-workspace")).unwrap();
    assert!(fs::symlink_metadata(&config).unwrap().is_symlink());
    assert_trusted(config_dir.path(), "another-workspace");
    let root: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert_eq!(root["keep"], 42);
    assert_eq!(
        root["projects"]["workspace"]["hasTrustDialogAccepted"],
        true
    );
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(config_dir.path()).unwrap().count(), 2);
}
