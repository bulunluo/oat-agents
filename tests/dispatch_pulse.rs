// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use clap::Parser;
use common::{TempRepo, TestWorld};
use oat_agents::environment::Environment;
use oat_agents::role::memory::InMemoryCatalogBuilder;
use oat_agents::store::{DispatchRecord, MessageKind, RunRecord, Store};
use oat_agents::{Cli, execute};
use serde_json::{Value, json};
use std::fs;
use std::time::{Duration, UNIX_EPOCH};

fn run(args: &[&str], env: &dyn Environment) -> Value {
    let cli =
        Cli::try_parse_from(std::iter::once("oat-agents").chain(args.iter().copied())).unwrap();
    execute(cli, env, &InMemoryCatalogBuilder::new().build()).unwrap()
}

#[test]
fn pulse_reports_questions_settlement_activity_and_identity_without_consuming_inbox() {
    let world = TestWorld::new();
    let config = world.scratch.path().join("custom-claude");
    let env = world
        .env()
        .with_var("CLAUDE_CONFIG_DIR", config.to_string_lossy())
        .with_var("OAT_DISPATCH_ID", "d1");
    let store = Store::open(&env).unwrap();
    let record: RunRecord = serde_json::from_value(json!({
        "id": "r1", "name": "glaze", "repo": "/test/repo", "base_branch": "main",
        "backend": "claude", "created_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    store.create_run(&record).unwrap();
    let dispatch: DispatchRecord = serde_json::from_value(json!({
        "id": "d1", "run_id": "r1", "role": "worker", "backend": "claude",
        "worktree": "/test/worktree", "branch": "oat/glaze/worker",
        "created_at": "2026-01-01T00:00:00Z", "pid": 123, "pgid": 120,
        "started_at": "Wed Oct  7 09:10:11 2026"
    }))
    .unwrap();
    store.create_dispatch(&dispatch).unwrap();
    store
        .create_dispatch(&DispatchRecord {
            id: "d2".into(),
            backend: "codex".into(),
            settled: Some(oat_agents::store::Settlement::Succeeded),
            ..dispatch.clone()
        })
        .unwrap();
    store
        .create_dispatch(&DispatchRecord {
            id: "meta".into(),
            role: "oat-meta".into(),
            headless: true,
            pid: None,
            pgid: None,
            started_at: None,
            ..dispatch.clone()
        })
        .unwrap();
    let question = store
        .append_inbox("r1", "d1", MessageKind::Question, "which glaze?")
        .unwrap();
    store
        .append_inbox("r1", "d2", MessageKind::MemberDone, "done")
        .unwrap();
    let transcript = config.join("projects/-test-worktree/session.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "{}\n").unwrap();
    fs::File::open(&transcript)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_100)),
        )
        .unwrap();
    fs::write(world.scratch.path().join("tmux-activity"), "1 1700000000\n").unwrap();

    let pulses = run(&["dispatch", "pulse", "--run", "r1"], &env);
    let pulses = pulses.as_array().unwrap();
    assert_eq!(
        pulses.len(),
        3,
        "OAT_DISPATCH_ID must not implicitly filter pulse"
    );
    let worker = pulses.iter().find(|p| p["dispatch_id"] == "d1").unwrap();
    assert_eq!(worker["role"], "worker");
    assert_eq!(worker["settled"], false);
    assert_eq!(worker["headless"], false);
    assert_eq!(worker["session_alive"], true);
    assert_eq!(worker["pane_dead"], true);
    assert_eq!(worker["waiting_on_meta"], true);
    assert_eq!(worker["last_output_at"], "2023-11-14T22:15:00Z");
    assert_eq!(
        worker["transcript_path"],
        transcript.to_string_lossy().as_ref()
    );
    assert_eq!(worker["pid"], 123);
    assert_eq!(worker["pgid"], 120);
    assert_eq!(worker["started_at"], "Wed Oct  7 09:10:11 2026");
    let codex = pulses.iter().find(|p| p["dispatch_id"] == "d2").unwrap();
    assert_eq!(codex["settled"], true);
    assert_eq!(codex["waiting_on_meta"], false);
    assert_eq!(
        codex["transcript_path"],
        Value::Null,
        "Codex never borrows a Claude transcript"
    );
    assert_eq!(codex["last_output_at"], "2023-11-14T22:13:20Z");
    let meta = pulses.iter().find(|p| p["dispatch_id"] == "meta").unwrap();
    assert_eq!(meta["headless"], true);
    assert_eq!(meta["session_alive"], false);
    assert!(meta["pane_dead"].is_null() && meta["last_output_at"].is_null());
    assert!(meta["pid"].is_null() && meta["transcript_path"].is_null());
    assert!(
        !world
            .tmux_calls()
            .iter()
            .any(|call| call.contains("capture-pane"))
    );

    let delivery = store
        .wait_inbox("r1", Duration::ZERO, Duration::ZERO)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.messages.len(), 2);
    store.ack_inbox("r1", &delivery.delivery_id).unwrap();
    let filtered = run(
        &["dispatch", "pulse", "--run", "r1", "--dispatch", "d1"],
        &env,
    );
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(
        filtered[0]["waiting_on_meta"], true,
        "acknowledgement is not a reply"
    );
    store.reply_to_message("r1", question, "cobalt").unwrap();
    fs::write(world.scratch.path().join("tmux-activity"), "0 1700000200\n").unwrap();
    let filtered = run(
        &["dispatch", "pulse", "--run", "r1", "--dispatch", "d1"],
        &env,
    );
    assert_eq!(filtered[0]["waiting_on_meta"], false);
    assert_eq!(filtered[0]["pane_dead"], false);
    assert_eq!(filtered[0]["last_output_at"], "2023-11-14T22:16:40Z");

    fs::write(world.scratch.path().join("tmux-session-dead"), "").unwrap();
    let dead = run(
        &["dispatch", "pulse", "--run", "r1", "--dispatch", "d2"],
        &env,
    );
    assert_eq!(dead[0]["session_alive"], false);
    assert!(dead[0]["pane_dead"].is_null() && dead[0]["last_output_at"].is_null());
}

#[test]
fn launch_captures_identity_after_start_and_show_exposes_it() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    // Keep this process alive while the stub returns its pid for ps to inspect.
    fs::write(
        world.scratch.path().join("tmux-pane-pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let fired = run(
        &[
            "meta",
            "fire",
            "--prompt",
            "glaze",
            "--repo",
            &repo.path().to_string_lossy(),
            "--name",
            "glaze",
        ],
        &env,
    );
    let shown = run(
        &[
            "dispatch",
            "show",
            "--run",
            "glaze",
            "--dispatch",
            fired["dispatch_id"].as_str().unwrap(),
        ],
        &env,
    );
    assert_eq!(shown["dispatch"]["pid"], std::process::id());
    assert!(shown["dispatch"]["pgid"].as_u64().unwrap() > 0);
    assert!(!shown["dispatch"]["started_at"].as_str().unwrap().is_empty());
    assert!(shown["dispatch"]["identity_warning"].is_null());
    let calls = world.tmux_calls();
    let start = calls
        .iter()
        .position(|c| c.contains("new-session"))
        .unwrap();
    assert!(calls[start + 1].contains("display-message -p -t =oat_"));
    assert!(calls[start + 1].ends_with("#{pane_pid}"));
}

#[test]
fn missing_pane_pid_or_failed_ps_is_a_recorded_warning_not_a_failed_launch() {
    let repo = TempRepo::new();
    let world = TestWorld::new();
    common::install_pottery_plugin(&world, &repo.path());
    let env = world.env();
    for (name, fake_pid) in [("missing", None), ("gone", Some("2147483647"))] {
        if let Some(pid) = fake_pid {
            fs::write(world.scratch.path().join("tmux-pane-pid"), pid).unwrap();
        }
        let fired = run(
            &[
                "meta",
                "fire",
                "--prompt",
                "glaze",
                "--repo",
                &repo.path().to_string_lossy(),
                "--name",
                name,
            ],
            &env,
        );
        let store = Store::open(&env).unwrap();
        let dispatch = store
            .load_dispatch(name, fired["dispatch_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(dispatch.pid, fake_pid.map(|pid| pid.parse().unwrap()));
        assert!(dispatch.pgid.is_none() && dispatch.started_at.is_none());
        assert!(dispatch.identity_warning.is_some());
    }
}
