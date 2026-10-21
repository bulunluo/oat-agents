// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

mod common;

use clap::Parser;
use common::{TempRepo, TestWorld};
use oat_agents::environment::TestEnvironment;
use oat_agents::event_log::{EventLog, events};
use oat_agents::role::memory::InMemoryCatalogBuilder;
use oat_agents::session::tmux::Tmux;
use oat_agents::store::{DispatchRecord, RunRecord, Store};
use oat_agents::{Cli, execute, worktree};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

// The real server uses a unique socket, an empty config and a temporary HOME. The guard
// also tears down sessions if an assertion fails, without contacting any other server.
struct Server {
    socket: String,
    home: std::path::PathBuf,
}

impl Server {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.env("HOME", &self.home).env_remove("TMUX");
        command.args(["-L", &self.socket, "-f", "/dev/null"]);
        command
    }

    fn start(&self, session: &str, cwd: &Path) {
        let output = self
            .command()
            .args(["new-session", "-d", "-s", session, "-c"])
            .arg(cwd)
            .args(["--", "/bin/sh -c 'printf member-output; exec sleep 300'"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.command().arg("kill-server").output();
    }
}

struct Fixture {
    // Drop the server before removing its HOME or worktrees.
    server: Server,
    _world: TestWorld,
    repo: TempRepo,
    env: TestEnvironment,
    store: Store,
    run: RunRecord,
}

impl Fixture {
    fn new() -> Self {
        let world = TestWorld::new();
        let repo = TempRepo::new();
        let server = Server {
            socket: format!(
                "oat-finish-{}-{}",
                std::process::id(),
                world.scratch.path().file_name().unwrap().to_string_lossy()
            ),
            home: world.home.clone(),
        };
        let env = world
            .env()
            .with_var("OAT_TMUX_COMMAND", "tmux")
            .with_var("OAT_TMUX_SOCKET", &server.socket);
        let store = Store::open(&env).unwrap();
        let run: RunRecord = serde_json::from_value(json!({
            "id": "finish", "name": "finish", "repo": repo.path(),
            "base_branch": "main", "backend": "claude",
            "created_at": "2026-01-01T00:00:00Z", "meta_dispatch_id": "meta"
        }))
        .unwrap();
        store.create_run(&run).unwrap();
        let fixture = Self {
            server,
            _world: world,
            repo,
            env,
            store,
            run,
        };
        let mut meta = fixture.member("meta", "oat-meta");
        meta.headless = true;
        fixture.store.save_dispatch(&meta).unwrap();
        fixture
    }

    fn member(&self, id: &str, role: &str) -> DispatchRecord {
        let path = self.repo.dir.path().join(id);
        let branch = format!("oat/finish/{id}");
        worktree::create_worktree(&self.repo.path(), &path, &branch, "main").unwrap();
        let dispatch: DispatchRecord = serde_json::from_value(json!({
            "id": id, "run_id": self.run.id, "role": role, "backend": "claude",
            "name": id, "worktree": path, "branch": branch,
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        self.store.create_dispatch(&dispatch).unwrap();
        dispatch
    }

    fn session(&self, dispatch: &DispatchRecord) -> String {
        Tmux::session_name(&dispatch.hash_id(&self.run))
    }

    fn start(&self, dispatch: &DispatchRecord) {
        let session = self.session(dispatch);
        self.server.start(&session, Path::new(&dispatch.worktree));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Tmux::from_env(&self.env)
            .capture_pane(&session)
            .unwrap()
            .contains("member-output")
        {
            assert!(Instant::now() < deadline, "member did not print its screen");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn screen(&self, dispatch: &DispatchRecord) -> std::path::PathBuf {
        self.store
            .dispatch_dir_path(&self.run.id, &dispatch.id)
            .join("screen.txt")
    }

    fn finish(&self) -> Value {
        let cli =
            Cli::try_parse_from(["oat-agents", "meta", "finish", "--run", &self.run.id]).unwrap();
        execute(cli, &self.env, &InMemoryCatalogBuilder::new().build()).unwrap()
    }
}

#[test]
fn finish_closes_live_and_dead_members_before_cleanup_and_skips_released_and_meta() {
    let f = Fixture::new();
    // A settled meta Dispatch is still excluded even if it has no release timestamp.
    let mut meta = f.store.load_dispatch(&f.run.id, "meta").unwrap();
    meta.settled = Some(oat_agents::store::Settlement::Succeeded);
    f.store.save_dispatch(&meta).unwrap();
    let planner = f.member("planner", "planner");
    let reviewer = f.member("reviewer", "reviewer");
    let dead = f.member("dead", "worker");
    let mut released = f.member("released", "worker");
    released.released_at = Some("2026-01-02T00:00:00Z".into());
    f.store.save_dispatch(&released).unwrap();
    std::fs::write(f.screen(&released), "previous screen").unwrap();
    let original = serde_json::to_value(&released).unwrap();
    for member in [&planner, &reviewer, &released] {
        f.start(member);
    }

    let receipt = f.finish();
    let closed = receipt["closed_members"].as_array().unwrap();
    assert_eq!(closed.len(), 3, "{receipt}");
    let tmux = Tmux::from_env(&f.env);
    for member in [&planner, &reviewer, &dead] {
        assert!(!tmux.session_alive(&f.session(member)).unwrap());
        let saved = f.store.load_dispatch(&f.run.id, &member.id).unwrap();
        assert!(saved.released_at.is_some());
        assert!(
            saved.settled.is_none(),
            "releasing must not invent a settlement"
        );
        assert!(
            !Path::new(&member.worktree).exists(),
            "worktree should be cleaned"
        );
        let result = closed
            .iter()
            .find(|r| r["dispatch_id"] == member.id)
            .unwrap();
        assert_eq!(result["released"], true);
        assert_eq!(result["errors"], json!([]));
    }
    for member in [&planner, &reviewer] {
        assert!(
            std::fs::read_to_string(f.screen(member))
                .unwrap()
                .contains("member-output")
        );
    }
    assert!(!f.screen(&dead).exists());
    assert!(
        tmux.session_alive(&f.session(&released)).unwrap(),
        "already released session is untouched"
    );
    assert_eq!(
        std::fs::read_to_string(f.screen(&released)).unwrap(),
        "previous screen"
    );
    assert_eq!(
        serde_json::to_value(f.store.load_dispatch(&f.run.id, &released.id).unwrap()).unwrap(),
        original
    );
    assert!(
        !f.store
            .dispatch_dir_path(&f.run.id, "meta")
            .join("screen.txt")
            .exists()
    );
    assert!(
        f.store
            .load_dispatch(&f.run.id, "meta")
            .unwrap()
            .released_at
            .is_none()
    );

    let entries = EventLog::open(&f.env).read_run(&f.run.id).unwrap();
    let first_cleanup = entries
        .iter()
        .position(|e| e.event == events::WORKTREE_REMOVED)
        .unwrap();
    let releases: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.event == events::DISPATCH_RELEASED)
        .collect();
    assert_eq!(releases.len(), 3);
    for (index, entry) in releases {
        assert!(index < first_cleanup);
        assert!(
            closed
                .iter()
                .any(|r| Some(r["dispatch_id"].as_str().unwrap()) == entry.dispatch_id.as_deref())
        );
    }
    assert!(!f.store.load_run(&f.run.id).unwrap().is_open());
}

#[test]
fn finish_reports_capture_write_failure_and_still_closes_every_member() {
    let f = Fixture::new();
    let broken = f.member("a-broken", "planner");
    let next = f.member("b-next", "reviewer");
    f.start(&broken);
    f.start(&next);
    std::fs::create_dir(f.screen(&broken)).unwrap();

    let receipt = f.finish();
    let closed = receipt["closed_members"].as_array().unwrap();
    assert_eq!(closed.len(), 2);
    assert_eq!(closed[0]["dispatch_id"], broken.id);
    assert!(
        closed[0]["errors"][0]
            .as_str()
            .unwrap()
            .contains("save screen:")
    );
    assert_eq!(closed[1]["errors"], json!([]));
    for member in [&broken, &next] {
        assert!(
            !Tmux::from_env(&f.env)
                .session_alive(&f.session(member))
                .unwrap()
        );
        assert!(
            f.store
                .load_dispatch(&f.run.id, &member.id)
                .unwrap()
                .released_at
                .is_some()
        );
    }
    assert!(
        std::fs::read_to_string(f.screen(&next))
            .unwrap()
            .contains("member-output")
    );
    assert!(!f.store.load_run(&f.run.id).unwrap().is_open());
}
