// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use super::*;
use crate::event_log::{LogEntry, now_iso};
use crate::store::{DispatchRecord, RunRecord};
use ratatui::style::Color;
use ratatui::backend::TestBackend;
use std::cell::RefCell;
use std::collections::HashSet;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// A stand-in for the machine: sessions it calls alive, screens and diffs it hands back, and
/// every call that would have changed something, recorded.
#[derive(Default)]
struct RecordingControl {
    alive: HashSet<String>,
    screens: HashMap<String, String>,
    diffs: HashMap<String, String>,
    consoles: HashMap<PathBuf, String>,
    calls: RefCell<Vec<String>>,
}

impl RecordingControl {
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn record(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

impl Control for RecordingControl {
    fn close_run(&self, run_id: &str) -> Result<String> {
        self.record(format!("close_run {run_id}"));
        Ok("closed".to_string())
    }

    fn clean_run(&self, run_id: &str) -> Result<String> {
        self.record(format!("clean_run {run_id}"));
        Ok("removed 2 worktrees".to_string())
    }

    fn type_input(&self, session: &str, input: &live::LiveInput) -> Result<()> {
        self.record(format!("type {session} {input:?}"));
        Ok(())
    }

    fn cursor(&self, _session: &str) -> Option<(u16, u16)> {
        None
    }

    fn screen_mode(&self, _session: &str) -> Option<live::ScreenMode> {
        None
    }

    fn capture(&self, session: &str) -> Result<String> {
        Ok(self.screens.get(session).cloned().unwrap_or_default())
    }

    fn capture_history(&self, session: &str) -> Result<String> {
        self.capture(session)
    }

    fn resize(&self, session: &str, cols: u16, rows: u16) -> Result<()> {
        self.record(format!("resize {session} {cols}x{rows}"));
        Ok(())
    }

    fn is_alive(&self, session: &str) -> bool {
        self.alive.contains(session)
    }

    fn diff(&self, worktree: &Path, _base: &str) -> Result<DiffReport> {
        let text = self
            .diffs
            .get(&worktree.display().to_string())
            .cloned()
            .unwrap_or_default();
        Ok(DiffReport {
            added: text.lines().filter(|line| line.starts_with('+')).count() as u32,
            removed: 0,
            text,
        })
    }

    fn console_live(&self, repo: &Path) -> bool {
        self.consoles.contains_key(repo)
    }

    fn console(&self, repo: &Path) -> Result<ConsoleHandle> {
        self.record(format!("console {}", repo.display()));
        Ok(ConsoleHandle {
            repo: repo.to_path_buf(),
            backend: "claude".to_string(),
            started_at: now_iso(),
            session: self
                .consoles
                .get(repo)
                .cloned()
                .unwrap_or_else(|| "oat_console-repo".to_string()),
            dir: None,
        })
    }

    fn stop_console(&self, repo: &Path) -> Result<String> {
        self.record(format!("stop_console {}", repo.display()));
        Ok("stopped".to_string())
    }

    fn copy(&self, text: &str) -> Result<()> {
        self.record(format!("copy {text:?}"));
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    log: EventLog,
    checklist: ChecklistStore,
}

const OPEN_RUN: &str = "run-a-open";
const CLOSED_RUN: &str = "run-b-closed";

fn entry(run_id: &str, dispatch_id: Option<&str>, agent: Option<&str>, event: &str, details: Value) -> LogEntry {
    LogEntry {
        timestamp: now_iso(),
        run_id: run_id.to_string(),
        dispatch_id: dispatch_id.map(str::to_string),
        agent: agent.map(str::to_string),
        event: event.to_string(),
        details: Some(details).filter(|details| !details.is_null()),
    }
}

/// Two Runs: an open one waiting on a person, whose meta-agent, worker (in a pod) and
/// reviewer (on the host though it asked for a pod, and already settled) are on record; and
/// a closed one that ran on the host.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::for_root(dir.path().join("runs"));
    let log = EventLog::for_dir(Some(dir.path().join("log")));
    let checklist = ChecklistStore::for_dir(Some(dir.path().join("checklists")));

    let open = RunRecord {
        id: OPEN_RUN.to_string(),
        name: "glaze".to_string(),
        repo: "/repos/pottery".to_string(),
        base_branch: "main".to_string(),
        backend: "claude".to_string(),
        created_at: now_iso(),
        closed_at: None,
        plugins: Vec::new(),
        meta_worktree: None,
        meta_dispatch_id: Some("dispatch-0".to_string()),
        goal: Some("# Glaze every pot\n\nThen fire the kiln.".to_string()),
        role_limits: Default::default(),
        role_settings: Default::default(),
    };
    store.create_run(&open).unwrap();
    store
        .create_run(&RunRecord {
            id: CLOSED_RUN.to_string(),
            closed_at: Some(now_iso()),
            goal: Some("Sweep the studio".to_string()),
            ..open.clone()
        })
        .unwrap();

    let meta = DispatchRecord {
        id: "dispatch-0".to_string(),
        run_id: OPEN_RUN.to_string(),
        role: "oat-meta".to_string(),
        backend: "claude".to_string(),
        worktree: "/repos/pottery.oat-glaze-meta".to_string(),
        branch: "oat/glaze/meta".to_string(),
        created_at: "2026-09-26T01:00:00.000000000Z".to_string(),
        env_id: None,
        image_id: None,
        env_skipped: None,
        settled: None,
        report: None,
        released_at: None,
        name: None,
        model: None,
        effort: None,
        headless: false,
        pid: None,
        pgid: None,
        started_at: None,
        identity_warning: None,
    };
    store.create_dispatch(&meta).unwrap();
    store
        .create_dispatch(&DispatchRecord {
            id: "dispatch-1".to_string(),
            role: "worker".to_string(),
            worktree: "/repos/pottery.oat-glaze-worker".to_string(),
            branch: "oat/glaze/worker".to_string(),
            created_at: "2026-09-26T01:01:00.000000000Z".to_string(),
            env_id: Some("oat-env-worker".to_string()),
            image_id: Some("sha256:0123456789abcdef".to_string()),
            ..meta.clone()
        })
        .unwrap();
    store
        .create_dispatch(&DispatchRecord {
            id: "dispatch-2".to_string(),
            role: "reviewer".to_string(),
            worktree: "/repos/pottery.oat-glaze-reviewer".to_string(),
            branch: "oat/glaze/reviewer".to_string(),
            created_at: "2026-09-26T01:02:00.000000000Z".to_string(),
            backend: "codex".to_string(),
            env_skipped: Some("this Run has no execution profile".to_string()),
            settled: Some(crate::store::Settlement::Succeeded),
            model: Some("gpt-5.5".to_string()),
            effort: Some("high".to_string()),
            ..meta.clone()
        })
        .unwrap();

    let details = |value: Value| value;
    for record in [
        entry(OPEN_RUN, None, None, events::RUN_CREATED, details(serde_json::json!({"big_plan": "Glaze every pot"}))),
        entry(OPEN_RUN, None, None, events::EXEC_PROFILE_SELECTED, serde_json::json!({"profile": "gb10"})),
        entry(OPEN_RUN, Some("dispatch-0"), Some("oat-meta"), events::AGENT_ENTER, serde_json::json!({"backend": "claude"})),
        entry(OPEN_RUN, Some("dispatch-1"), Some("worker"), events::AGENT_ENTER, serde_json::json!({"backend": "claude"})),
        entry(OPEN_RUN, Some("dispatch-2"), Some("reviewer"), events::AGENT_ENTER, serde_json::json!({"backend": "codex"})),
        entry(OPEN_RUN, Some("dispatch-2"), Some("reviewer"), events::AGENT_EXIT, serde_json::json!({"outcome": "succeeded"})),
        entry(OPEN_RUN, None, None, events::NEEDS_HUMAN, serde_json::json!({"message": "which glaze?"})),
        entry(CLOSED_RUN, None, None, events::EXEC_PROFILE_NONE, serde_json::json!({"profile": null})),
    ] {
        log.record(&record).unwrap();
    }

    checklist
        .replace_items(OPEN_RUN, vec!["glaze pots".to_string(), "fire kiln".to_string()])
        .unwrap();
    checklist.set_checked(OPEN_RUN, 1, true).unwrap();

    Fixture {
        _dir: dir,
        store,
        log,
        checklist,
    }
}

impl Fixture {
    fn snapshot(&self, run_id: &str) -> RunSnapshot {
        RunSnapshot::read(&self.store, &self.log, run_id).unwrap()
    }

    fn state(&self) -> TuiState {
        TuiState::with_projects(self.snapshot(OPEN_RUN), None)
    }

    fn picker(&self) -> Picker {
        Picker::new(model::run_summaries(&self.store, &self.log))
    }

    fn session_of(&self, role: &str) -> String {
        let snapshot = self.snapshot(OPEN_RUN);
        let agent = snapshot
            .agents()
            .into_iter()
            .find(|agent| agent.role.as_deref() == Some(role))
            .unwrap();
        agent.terminal_handle.unwrap()
    }
}

fn screen(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(paint).unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// The colour of the live pane's top-left corner.
fn live_border(state: &mut TuiState) -> Option<Color> {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| draw(frame, state)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height).find_map(|y| {
        let row: String = (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
        let title = row.find("· live")?;
        let corner = row[..title].rfind('┌')?;
        let x = row[..corner].chars().count() as u16;
        Some(buffer[(x, y)].fg)
    })
}

fn select(state: &mut TuiState, role: &str) {
    let index = state
        .agents()
        .iter()
        .position(|agent| agent.role.as_deref() == Some(role))
        .unwrap();
    state.selected = index + 1;
}

#[test]
fn the_picker_lists_every_run_with_its_title_its_pod_and_who_it_waits_on() {
    let fx = fixture();
    let picker = fx.picker();
    let text = screen(160, 12, |frame| draw_runs(frame, &picker, fx.log.dir(), &HashMap::new()));
    assert!(text.contains(" oat-agents "), "{text}");
    assert!(text.contains("2 runs (1 running)"), "{text}");
    assert!(text.contains(OPEN_RUN) && text.contains(CLOSED_RUN), "{text}");
    assert!(text.contains("pottery"), "the repo's folder names the Run:\n{text}");
    assert!(
        text.contains("which glaze?") && !text.contains("Glaze every pot"),
        "an open question outranks the title:\n{text}"
    );
    assert!(text.contains("Sweep the studio"), "the goal's first line is the title:\n{text}");
    assert!(text.contains("pod:gb10"), "{text}");
    assert!(text.contains("host"), "{text}");
    assert!(text.contains("enter open"), "{text}");
    // Running first: the open Run heads the list.
    assert!(text.find(OPEN_RUN).unwrap() < text.find(CLOSED_RUN).unwrap(), "{text}");
}

#[test]
fn the_picker_opens_a_run_on_enter_or_the_right_arrow() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut picker = fx.picker();
    assert_eq!(picker.on_key(key(KeyCode::Enter), &control), PickerAction::Open(OPEN_RUN.into()));
    assert_eq!(picker.on_key(key(KeyCode::Right), &control), PickerAction::Open(OPEN_RUN.into()));
    picker.on_key(key(KeyCode::Down), &control);
    assert_eq!(picker.on_key(key(KeyCode::Enter), &control), PickerAction::Open(CLOSED_RUN.into()));
    assert_eq!(picker.on_key(key(KeyCode::Char('q')), &control), PickerAction::Quit);
}

#[test]
fn closing_a_run_from_the_picker_takes_a_deliberate_yes() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut picker = fx.picker();

    picker.on_key(key(KeyCode::Char('x')), &control);
    assert!(picker.confirming().is_some());
    picker.on_key(key(KeyCode::Char('n')), &control);
    assert_eq!(picker.notice(), Some("run-a-open left running"));
    assert!(control.calls().is_empty());

    picker.on_key(key(KeyCode::Char('x')), &control);
    picker.on_key(key(KeyCode::Char('y')), &control);
    assert_eq!(control.calls(), vec!["close_run run-a-open"]);
    assert_eq!(picker.notice(), Some("run-a-open closed"));
}

#[test]
fn a_finished_run_can_be_cleaned_from_the_picker_and_an_open_one_cannot() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut picker = fx.picker();

    picker.on_key(key(KeyCode::Char('c')), &control);
    assert_eq!(picker.notice(), Some("run-a-open is still running; close it first"));
    picker.on_key(key(KeyCode::Down), &control);
    picker.on_key(key(KeyCode::Char('x')), &control);
    assert_eq!(picker.notice(), Some("run-b-closed has nothing running"));
    picker.on_key(key(KeyCode::Char('c')), &control);
    assert_eq!(control.calls(), vec!["clean_run run-b-closed"]);
}

#[test]
fn the_meta_agent_is_named_under_meta_as_its_launch_named_it() {
    let fx = fixture();
    let state = fx.state();
    let meta = state.agents().iter().find(|agent| agent.is_meta()).unwrap();
    assert_eq!(meta.hash_id, crate::event_log::hash_id("meta", "glaze", "dispatch-0"));
    assert_eq!(
        meta.terminal_handle.as_deref(),
        Some(crate::session::tmux::Tmux::session_name(&meta.hash_id).as_str())
    );
}

#[test]
fn the_run_view_names_every_dispatch_where_it_runs_and_the_keys() {
    let fx = fixture();
    let mut state = fx.state();
    let text = screen(160, 24, |frame| draw(frame, &mut state));
    assert!(text.contains(" run-a-open "), "{text}");
    assert!(text.contains("3 agents (2 active)"), "{text}");
    for agent in state.agents() {
        assert!(text.contains(&agent.hash_id), "{} missing:\n{text}", agent.hash_id);
    }
    assert!(text.contains("pod:oat-env-worker"), "{text}");
    assert!(text.contains("HOST (no pod)"), "{text}");
    assert!(
        text.contains("gpt-5.5 high"),
        "a Codex reviewer, with no transcript to read, shows the model it was launched on:\n{text}"
    );
    assert!(text.contains("succeeded at"), "a finished agent says how it ended:\n{text}");
    assert!(text.contains("finished"), "the rule separates live from finished:\n{text}");
    assert!(text.contains(WAITING_MARK), "the meta-agent that asked is marked:\n{text}");
    assert!(text.contains(" timeline "), "{text}");
    assert!(text.contains("↑↓ agent"), "{text}");
    assert!(text.contains("e events:all"), "{text}");
    assert!(text.contains("ctrl+\\ console"), "{text}");
    assert!(text.contains("ctrl+l checklist"), "{text}");
}

#[test]
fn a_codex_reviewer_shows_the_tokens_and_cost_its_codex_session_recorded() {
    let fx = fixture();
    let codex = tempfile::tempdir().unwrap();
    let day = codex.path().join("2026/09/26");
    fs::create_dir_all(&day).unwrap();
    let usage = serde_json::json!({"input_tokens": 1_000_000, "cached_input_tokens": 400_000,
                                   "cache_write_input_tokens": 0, "output_tokens": 20_000});
    let lines = [
        serde_json::json!({"type": "session_meta", "payload": {"id": "s", "session_id": "s",
            "cwd": "/repos/pottery.oat-glaze-reviewer", "source": "cli"}}),
        serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-6-sol", "effort": "xhigh"}}),
        serde_json::json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": usage, "last_token_usage": usage}}}),
    ];
    let rollout = lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    fs::write(day.join("rollout-s.jsonl"), rollout).unwrap();

    let mut state = fx.state();
    let mut spend = SpendIndex::new(Some(codex.path().to_owned()));
    state.refresh_spend(&mut spend);
    let text = screen(160, 24, |frame| draw(frame, &mut state));
    // 600k fresh + 400k cached + 20k out; $1.20 + $0.08 + $0.20 at gpt-6-sol's rates.
    assert!(text.contains("1.0M    $1.48"), "{text}");
    assert!(text.contains("gpt-6-sol xhigh"), "what the session ran on, not its launch:\n{text}");
}

#[test]
fn selecting_a_dispatch_narrows_the_timeline_to_its_own_entries() {
    let fx = fixture();
    // A second Dispatch of the same role must not leak into the first one's timeline.
    fx.log
        .record(&entry(OPEN_RUN, Some("dispatch-3"), Some("worker"), events::AGENT_EXIT, Value::Null))
        .unwrap();
    let mut state = fx.state();
    assert_eq!(state.visible_events().len(), 8, "all agents shows every entry");
    select(&mut state, "worker");
    let events: Vec<String> = state
        .visible_events()
        .iter()
        .filter_map(|row| string_field(row, "event"))
        .collect();
    assert_eq!(events, vec![events::AGENT_ENTER.to_string()]);
}

/// Presses ↑ to the top of the roster, then ↓ until `role` is selected.
fn arrow_to(state: &mut TuiState, control: &RecordingControl, role: &str) {
    for _ in 0..=state.agents().len() {
        state.on_key_with(key(KeyCode::Up), control);
    }
    for _ in 0..state.agents().len() {
        if state.selected_agent().and_then(|agent| agent.role.as_deref()) == Some(role) {
            return;
        }
        state.on_key_with(key(KeyCode::Down), control);
    }
    assert_eq!(state.selected_agent().and_then(|agent| agent.role.as_deref()), Some(role));
}

#[test]
fn the_arrows_pick_an_agent_and_its_page_follows_on_the_tab_last_chosen() {
    let fx = fixture();
    let session = fx.session_of("worker");
    let mut control = RecordingControl::default();
    control.alive.insert(session.clone());
    control.screens.insert(session, "running 3 tests ... ok".to_string());
    let mut state = fx.state();
    state.refresh_sessions(&control);

    // Every agent at once is only the Run's timeline.
    assert_eq!(state.tabs(), &[Tab::Timeline]);
    assert_eq!(state.tab(), Tab::Timeline);

    arrow_to(&mut state, &control, "worker");
    assert_eq!(state.tabs(), &[Tab::Live, Tab::Diff, Tab::Timeline, Tab::Log]);
    assert_eq!(state.tab(), Tab::Live, "a running agent opens on its screen");
    state.refresh_preview(&control);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("running 3 tests"), "{text}");

    state.on_key_with(key(KeyCode::Right), &control);
    state.on_key_with(key(KeyCode::Right), &control);
    assert_eq!(state.tab(), Tab::Timeline);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("· timeline "), "{text}");
    assert!(text.contains("agent_enter"), "{text}");

    // A finished agent keeps the timeline chosen, and has no screen to watch.
    arrow_to(&mut state, &control, "reviewer");
    assert_eq!(state.tab(), Tab::Timeline);
    assert!(state.preview.is_none());
}

#[test]
fn the_event_filter_cycles_through_all_lifecycle_and_decisions() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut state = fx.state();
    state.on_key_with(key(KeyCode::Char('e')), &control);
    assert_eq!(state.filter(), EventFilter::Lifecycle);
    assert_eq!(state.visible_events().len(), 4);
    state.on_key_with(key(KeyCode::Char('e')), &control);
    assert_eq!(state.filter(), EventFilter::Decisions);
    assert_eq!(state.visible_events().len(), 1, "the question to a person is a decision point");
    state.on_key_with(key(KeyCode::Char('e')), &control);
    assert_eq!(state.filter(), EventFilter::All);
}

#[test]
fn selecting_a_live_agent_shows_its_screen_and_enter_hands_it_every_key() {
    let fx = fixture();
    let session = fx.session_of("worker");
    let mut control = RecordingControl::default();
    control.alive.insert(session.clone());
    control
        .screens
        .insert(session.clone(), "$ cargo test\nrunning 3 tests ... ok".to_string());
    let mut state = fx.state();
    state.refresh_sessions(&control);
    select(&mut state, "worker");
    state.sync_page(&control);

    // No key opens the page: selecting the agent is enough.
    assert_eq!(state.tab(), Tab::Live);
    assert!(!state.typing(), "the live tab opens watching, not typing");
    state.refresh_preview(&control);

    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("running 3 tests"), "the screen shows while watching:\n{text}");
    assert!(text.contains("· live ─"), "{text}");
    assert!(text.contains("enter type"), "the keys are in the hints, not the title:\n{text}");
    assert_eq!(live_border(&mut state), Some(Color::DarkGray), "the pane is grey until enter");
    state.fit_preview(&control);

    // Watching, keys stay with the view: PgDn scrolls rather than reaching the agent.
    state.on_key_with(key(KeyCode::PageDown), &control);
    assert!(
        !control.calls().iter().any(|call| call.starts_with("type ")),
        "{:?}",
        control.calls()
    );

    state.on_key_with(key(KeyCode::Enter), &control);
    assert!(state.typing(), "enter steps into the agent's terminal");
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("live · typing"), "{text}");
    assert!(text.contains("ctrl+] stop typing"), "{text}");
    assert_eq!(live_border(&mut state), Some(Color::Cyan), "typing lights the pane");

    state.on_key_with(key(KeyCode::Char('q')), &control);
    state.on_key_with(key(KeyCode::Enter), &control);
    assert!(state.typing(), "q is the agent's while typing");
    let calls = control.calls();
    assert!(calls.contains(&format!("type {session} Text(\"q\")")), "{calls:?}");
    assert!(calls.contains(&format!("type {session} Key(\"Enter\")")), "{calls:?}");
    assert!(calls.iter().any(|call| call.starts_with(&format!("resize {session}"))), "{calls:?}");

    state.on_key_with(ctrl(']'), &control);
    assert!(!state.typing());
    state.on_key_with(key(KeyCode::Right), &control);
    assert_eq!(state.tab(), Tab::Diff);
    assert!(!state.typing());
    state.on_key_with(key(KeyCode::Right), &control);
    assert_eq!(state.tab(), Tab::Timeline, "the timeline sits left of the log");
    state.on_key_with(key(KeyCode::Right), &control);
    assert_eq!(state.tab(), Tab::Log);
    state.on_key_with(key(KeyCode::Left), &control);
    assert_eq!(state.tab(), Tab::Timeline);
    assert_eq!(state.on_key_with(key(KeyCode::Esc), &control), Action::Back);
}

#[test]
fn the_diff_tab_shows_what_the_worktree_changed() {
    let fx = fixture();
    let session = fx.session_of("worker");
    let mut control = RecordingControl::default();
    control.alive.insert(session);
    control.diffs.insert(
        "/repos/pottery.oat-glaze-worker".to_string(),
        "diff --git a/x b/x\n+added line".to_string(),
    );
    let mut state = fx.state();
    select(&mut state, "worker");
    state.sync_page(&control);
    state.on_key_with(key(KeyCode::Right), &control);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("added line"), "{text}");
    assert!(text.contains("since main"), "{text}");
}

#[test]
fn a_finished_agent_has_its_timeline_and_its_log_which_says_where_it_ran() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut state = fx.state();
    select(&mut state, "reviewer");
    state.sync_page(&control);
    assert_eq!(state.tabs(), &[Tab::Timeline, Tab::Log]);
    assert_eq!(state.tab(), Tab::Log, "the live tab it would have shown falls back to the log");
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("HOST (no pod): this Run has no execution profile"), "{text}");
    assert!(text.contains("outcome succeeded"), "{text}");
    assert!(text.contains("agent_exit"), "without a transcript the log's own record shows:\n{text}");
    assert!(
        text.contains("Codex keeps no transcript") && !text.contains("Claude Code"),
        "a Codex agent is not told about Claude Code's sessions:\n{text}"
    );

    let mut state = fx.state();
    select(&mut state, "worker");
    state.sync_page(&control);
    state.on_key_with(key(KeyCode::Left), &control);
    assert_eq!(state.tab(), Tab::Log);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("env pod:oat-env-worker   image 0123456789ab"), "{text}");
}

#[test]
fn closing_a_run_from_its_view_takes_a_deliberate_yes() {
    let fx = fixture();
    let control = RecordingControl::default();
    let mut state = fx.state();
    assert_eq!(state.on_key_with(key(KeyCode::Char('x')), &control), Action::Continue);
    assert!(state.closing());
    let text = screen(160, 24, |frame| draw(frame, &mut state));
    assert!(text.contains("close run-a-open and stop 2 live agents"), "{text}");
    assert_eq!(
        state.on_key_with(key(KeyCode::Char('y')), &control),
        Action::CloseRun(OPEN_RUN.to_string())
    );
    state.on_key_with(key(KeyCode::Char('x')), &control);
    assert_eq!(state.on_key_with(key(KeyCode::Char('n')), &control), Action::Continue);
    assert_eq!(state.notice(), Some("left running"));
}

#[test]
fn the_checklist_panel_shows_the_runs_items_and_how_many_are_done() {
    let fx = fixture();
    let mut panel = ChecklistPanel::new();
    panel.open_for(OPEN_RUN.to_string());
    panel.refresh(Some(&fx.checklist), Some(&fx.store));
    let text = screen(100, 30, |frame| checklist_panel::draw(frame, &panel));
    assert!(text.contains("checklist · 1/2"), "{text}");
    assert!(text.contains("[x] 1. glaze pots"), "{text}");
    assert!(text.contains("[ ] 2. fire kiln"), "{text}");
}

#[test]
fn a_linked_checklist_item_shows_its_work_and_flags_work_finished_but_unchecked() {
    let fx = fixture();
    let dispatch = |id: &str, name: &str, settled: Option<crate::store::Settlement>, released: bool| DispatchRecord {
        id: id.to_string(),
        run_id: OPEN_RUN.to_string(),
        role: "worker".to_string(),
        backend: "claude".to_string(),
        worktree: String::new(),
        branch: String::new(),
        created_at: format!("2026-09-26T02:00:0{}.000000000Z", id.len()),
        env_id: None,
        image_id: None,
        env_skipped: None,
        settled,
        report: None,
        released_at: released.then(now_iso),
        name: Some(name.to_string()),
        model: None,
        effort: None,
        headless: false,
        pid: None,
        pgid: None,
        started_at: None,
        identity_warning: None,
    };
    fx.store
        .create_dispatch(&dispatch("dispatch-g", "glaze", Some(crate::store::Settlement::Succeeded), true))
        .unwrap();
    fx.store.create_dispatch(&dispatch("dispatch-k1", "kiln-fix1", None, false)).unwrap();
    fx.checklist
        .replace_items(OPEN_RUN, vec!["glaze pots".into(), "fire kiln".into(), "sweep".into()])
        .unwrap();
    fx.checklist.set_link(OPEN_RUN, 1, Some("glaze".into())).unwrap();
    fx.checklist.set_link(OPEN_RUN, 2, Some("kiln".into())).unwrap();

    let mut panel = ChecklistPanel::new();
    panel.open_for(OPEN_RUN.to_string());
    panel.refresh(Some(&fx.checklist), Some(&fx.store));
    let text = screen(120, 30, |frame| checklist_panel::draw(frame, &panel));
    assert!(text.contains("[ ] 1. glaze pots ? · worker glaze succeeded, released"), "{text}");
    assert!(text.contains("[ ] 2. fire kiln · worker kiln-fix1 active"), "{text}");
    assert!(text.contains("[ ] 3. sweep "), "{text}");
    assert!(!text.contains("3. sweep ·"), "an unlinked item shows no progress:\n{text}");
}

#[test]
fn the_console_window_has_a_screen_and_a_log_and_nothing_to_close_but_itself() {
    let control = RecordingControl::default();
    let handle = control.console(Path::new("/repos/pottery")).unwrap();
    let mut state = console_state(&handle, None);
    assert_eq!(state.agents().len(), 1);
    state.selected = 1;
    assert_eq!(state.tabs(), &[Tab::Live, Tab::Log]);
    assert_eq!(state.agents()[0].hash_id, "console-pottery");

    let text = screen(160, 24, |frame| draw(frame, &mut state));
    assert!(text.contains("x stop console"), "{text}");
    assert!(!text.contains("checklist"), "the console has no Run, so no checklist:\n{text}");

    state.on_key_with(key(KeyCode::Char('x')), &control);
    assert_eq!(state.on_key_with(key(KeyCode::Char('y')), &control), Action::StopConsole);
    state.on_key_with(key(KeyCode::Char('x')), &control);
    state.on_key_with(key(KeyCode::Esc), &control);
    assert_eq!(state.notice(), Some("console left running"));
}

#[test]
fn the_console_key_opens_the_repository_the_view_was_started_in() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    assert!(std::process::Command::new("git").arg("init").arg("-q").arg(&repo).status().unwrap().success());
    let inside = repo.join("src").join("deep");
    std::fs::create_dir_all(&inside).unwrap();
    assert_eq!(console_target(&inside), repo);

    let outside = tempfile::tempdir().unwrap();
    let plain = outside.path().canonicalize().unwrap();
    assert_eq!(console_target(&plain), plain, "outside a repository it is the directory itself");

    assert!(is_console_toggle(&ctrl('\\')));
    assert!(is_console_toggle(&key(KeyCode::F(2))));
}

/// Where `needle` is drawn on a screen from `screen`, as a terminal cell.
fn cell_of(text: &str, needle: &str) -> (u16, u16) {
    text.lines()
        .enumerate()
        .find_map(|(row, line)| {
            let at = line.find(needle)?;
            Some((line[..at].chars().count() as u16, row as u16))
        })
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{text}"))
}

/// A worker's page open on its live screen, drawn once so the box is placed.
fn watching_worker(fx: &Fixture, screen_text: &str) -> (TuiState, RecordingControl, String) {
    let session = fx.session_of("worker");
    let mut control = RecordingControl::default();
    control.alive.insert(session.clone());
    control.screens.insert(session.clone(), screen_text.to_string());
    let mut state = fx.state();
    state.refresh_sessions(&control);
    select(&mut state, "worker");
    state.sync_page(&control);
    state.refresh_preview(&control);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    (state, control, text)
}

#[test]
fn dragging_across_the_live_screen_copies_what_it_crossed_without_moving_the_screen() {
    let fx = fixture();
    let (mut state, control, text) =
        watching_worker(&fx, "$ cargo test\nrunning 3 tests ... ok\ntest result: ok");
    let (from_col, from_row) = cell_of(&text, "cargo test");
    let (to_col, to_row) = cell_of(&text, "3 tests");

    state.on_press(from_col, from_row);
    state.on_drag(to_col, to_row, &control);
    state.on_release(&control);

    assert!(
        control.calls().contains(&"copy \"cargo test\\nrunning 3\"".to_string()),
        "{:?}",
        control.calls()
    );
    assert_eq!(state.notice(), Some("copied 2 lines"));
    let after = screen(120, 30, |frame| draw(frame, &mut state));
    assert_eq!(
        cell_of(&after, "running 3"),
        cell_of(&text, "running 3"),
        "reading back starts where the live screen stood:\n{after}"
    );
    assert!(after.contains("live · scroll"), "{after}");

    // A click is not a drag: it puts the selection down and copies nothing.
    state.on_press(from_col, from_row);
    state.on_release(&control);
    assert!(!state.preview.as_ref().unwrap().has_selection());
    assert_eq!(control.calls().iter().filter(|call| call.starts_with("copy ")).count(), 1);
}

#[test]
fn v_selects_with_the_keys_and_y_copies() {
    let fx = fixture();
    let (mut state, control, _) =
        watching_worker(&fx, "$ cargo test\nrunning 3 tests ... ok\ntest result: ok");

    state.on_key_with(key(KeyCode::Char('v')), &control);
    let text = screen(120, 30, |frame| draw(frame, &mut state));
    assert!(text.contains("live · select"), "{text}");
    assert!(text.contains("y copy"), "{text}");

    // The cursor starts on the newest line: up one, mark, then across to the end of "running".
    state.on_key_with(key(KeyCode::Up), &control);
    state.on_key_with(key(KeyCode::Char('v')), &control);
    for _ in 0..6 {
        state.on_key_with(key(KeyCode::Right), &control);
    }
    // The arrows moved the cursor rather than picking another agent or tab.
    assert_eq!(state.tab(), Tab::Live);
    assert_eq!(state.selected_agent().and_then(|agent| agent.role.as_deref()), Some("worker"));
    state.on_key_with(key(KeyCode::Char('y')), &control);
    assert!(
        control.calls().contains(&"copy \"running\"".to_string()),
        "{:?}",
        control.calls()
    );
    assert!(!state.preview.as_ref().unwrap().selecting_with_keys());
    assert!(
        !control.calls().iter().any(|call| call.starts_with("type ")),
        "selecting never types at the agent: {:?}",
        control.calls()
    );

    // With nothing marked, y takes the cursor's whole line; esc then leaves reading back.
    state.on_key_with(key(KeyCode::Char('v')), &control);
    state.on_key_with(key(KeyCode::Char('y')), &control);
    assert!(control.calls().contains(&"copy \"test result: ok\"".to_string()), "{:?}", control.calls());
    state.on_key_with(key(KeyCode::Esc), &control);
    assert!(!state.preview.as_ref().unwrap().scrolling());
}

#[test]
fn a_copy_reaches_the_terminal_as_base64() {
    assert_eq!(control::base64(b""), "");
    assert_eq!(control::base64(b"f"), "Zg==");
    assert_eq!(control::base64(b"fo"), "Zm8=");
    assert_eq!(control::base64(b"foo"), "Zm9v");
    assert_eq!(control::base64("選取".as_bytes()), "6YG45Y+W");
}

#[test]
fn every_key_is_listed_in_at_most_three_rows_no_wider_than_the_cap() {
    let fx = fixture();
    let state = fx.state();
    // Overview, a wide terminal: every hint shows, wrapped rather than run across it.
    let rows = menu::lines(&state.menu(), 240);
    let shown: String = rows.iter().map(|row| row.to_string() + "\n").collect();
    for item in state.menu().iter().flatten() {
        assert!(shown.contains(item.key) && shown.contains(&item.desc), "{} missing:\n{shown}", item.key);
    }
    assert!((2..=menu::MAX_ROWS).contains(&rows.len()), "{shown}");
    assert!(rows.iter().all(|row| row.width() <= menu::MAX_WIDTH as usize), "{shown}");

    // Narrow, the rows stay three and the way out stays.
    let rows = menu::lines(&state.menu(), 50);
    assert!(rows.len() <= menu::MAX_ROWS);
    assert!(rows.iter().all(|row| row.width() <= 50));
    assert!(rows.last().unwrap().to_string().contains("q quit"));

    let (mut state, _control, _) = watching_worker(&fx, "$ cargo test");
    let text = screen(160, 30, |frame| draw(frame, &mut state));
    for key in ["enter type", "v/drag select", "←→ tab:live", "x close run", "ctrl+l checklist", "q quit"] {
        assert!(text.contains(key), "{key} missing:\n{text}");
    }
}

#[test]
fn the_no_transcript_note_names_the_agents_own_backend() {
    assert_eq!(no_transcript_note(Some("claude"), true), "No transcript found for this worktree yet.");
    assert_eq!(no_transcript_note(Some("claude"), false), "Claude Code's session directory is not reachable.");
    for reachable in [true, false] {
        assert!(no_transcript_note(Some("codex"), reachable).starts_with("Codex keeps no transcript"));
        assert!(!no_transcript_note(None, reachable).contains("Claude Code"));
    }
}
