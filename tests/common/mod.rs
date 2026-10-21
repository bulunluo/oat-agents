#![allow(dead_code)]
// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors


use oat_agents::environment::TestEnvironment;
use oat_agents::plugins::{self, PluginPin};
use oat_agents::trust::{TrustKey, TrustStore};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A temporary git repository with one commit on `main`, ready to be a Run's repository.
pub struct TempRepo {
    pub dir: tempfile::TempDir,
}

impl TempRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        std::fs::create_dir_all(&path).unwrap();
        run(&path, &["init", "-q", "-b", "main"]);
        run(&path, &["config", "user.email", "test@example.com"]);
        run(&path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        run(&path, &["add", "."]);
        run(&path, &["commit", "-q", "-m", "initial"]);
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("repo")
    }
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// Writes a recording tmux stand-in (ticket §7.1): it appends its whole argv to `log_path` and
/// exits 0 for launches. Optional `tmux-pane-pid`, `tmux-activity`, and `tmux-session-dead`
/// files in `scratch` control read-only queries. No test needs a real model or tmux server.
pub fn tmux_stub(scratch: &Path, log_path: &Path) -> PathBuf {
    let script = scratch.join("fake-tmux.sh");
    let contents = format!(
        r#"#!/bin/sh
umask 022
printf '%s\n' "$*" >> "{}"
fixtures="$(dirname "$0")"
case "$3" in
  has-session) [ ! -f "$fixtures/tmux-session-dead" ]; exit $? ;;
  display-message)
    case "$7" in
      '#{{pane_pid}}') cat "$fixtures/tmux-pane-pid" 2>/dev/null ;;
      '#{{pane_dead}} #{{window_activity}}') cat "$fixtures/tmux-activity" 2>/dev/null ;;
    esac
    exit $? ;;
esac
exit 0
"#,
        log_path.display()
    );
    std::fs::write(&script, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

pub struct TestWorld {
    pub scratch: tempfile::TempDir,
    pub home: PathBuf,
    pub tmux_log: PathBuf,
}

impl TestWorld {
    pub fn new() -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let tmux_log = scratch.path().join("tmux.log");
        Self {
            scratch,
            home,
            tmux_log,
        }
    }

    pub fn env(&self) -> TestEnvironment {
        let tmux = tmux_stub(self.scratch.path(), &self.tmux_log);
        TestEnvironment::new(self.home.clone())
            .with_var("OAT_TMUX_COMMAND", tmux.to_string_lossy().to_string())
            .with_var("OAT_TMUX_SOCKET", "oat-test")
            .with_var(
                "OAT_AGENTS_STATE_DIR",
                self.scratch.path().join("state").to_string_lossy().to_string(),
            )
    }

    pub fn tmux_calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.tmux_log)
            .unwrap_or_default()
            .lines()
            .map(|s| s.to_string())
            .collect()
    }
}

// ---- F5's gate: every test that calls `meta fire` or `console open` needs a repository whose
// `.oat/plugins.toml` pins a real, loadable plugin, trusted on the machine `TestWorld` stands
// in for. `PluginBuilder` writes one directly under a repository (as a path pin), matching the
// plugin format ADR-0002 defines; `finish` pins and trusts it in one step.

pub struct PluginBuilder {
    dir: PathBuf,
}

impl PluginBuilder {
    /// Starts a plugin directory at `repo.join(relative_path)`.
    pub fn new(repo: &Path, relative_path: &str, name: &str) -> Self {
        let dir = repo.join(relative_path);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oat-plugin.toml"),
            format!("format_version = 1\nname = \"{name}\"\ndescription = \"test fixture\"\n"),
        )
        .unwrap();
        Self { dir }
    }

    pub fn role(self, name: &str, start: &str, instructions: &str) -> Self {
        self.role_with(name, start, instructions, &[], false, false)
    }

    pub fn role_with(
        self,
        name: &str,
        start: &str,
        instructions: &str,
        skills: &[&str],
        exec_environment: bool,
        prior_verification: bool,
    ) -> Self {
        let role_dir = self.dir.join("roles").join(name);
        std::fs::create_dir_all(&role_dir).unwrap();
        let skills_line = skills_toml_line(skills);
        std::fs::write(
            role_dir.join("role.toml"),
            format!(
                "description = \"role\"\nstart = \"{start}\"\nexec_environment = {exec_environment}\nprior_verification = {prior_verification}\n{skills_line}"
            ),
        )
        .unwrap();
        std::fs::write(role_dir.join("instructions.md"), instructions).unwrap();
        self
    }

    /// Gives an already-written role the plugin's default `max_concurrent`.
    pub fn max_concurrent(self, role: &str, limit: u32) -> Self {
        let path = self.dir.join("roles").join(role).join("role.toml");
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str(&format!("max_concurrent = {limit}\n"));
        std::fs::write(path, contents).unwrap();
        self
    }

    /// Appends `lines` to an already-written role's `role.toml`.
    pub fn role_toml(self, role: &str, lines: &str) -> Self {
        let path = self.dir.join("roles").join(role).join("role.toml");
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str(lines);
        std::fs::write(path, contents).unwrap();
        self
    }

    pub fn core(self, slug: &str, instructions: &str) -> Self {
        self.core_with(slug, instructions, &[])
    }

    pub fn core_with(self, slug: &str, instructions: &str, skills: &[&str]) -> Self {
        let core_dir = self.dir.join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(core_dir.join(format!("{slug}-instruction.md")), instructions).unwrap();
        if !skills.is_empty() {
            std::fs::write(core_dir.join(format!("{slug}.toml")), skills_toml_line(skills)).unwrap();
        }
        self
    }

    /// Writes `core/<slug>.toml` as given, replacing any written by `core_with`.
    pub fn core_toml(self, slug: &str, contents: &str) -> Self {
        let core_dir = self.dir.join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(core_dir.join(format!("{slug}.toml")), contents).unwrap();
        self
    }

    pub fn skill(self, name: &str, files: &[(&str, &[u8], bool)]) -> Self {
        let skill_dir = self.dir.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        for (relative_path, contents, executable) in files {
            let path = skill_dir.join(relative_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            #[cfg(unix)]
            if *executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        self
    }

    /// A minimal, always-present pair of core instructions, for a test whose plugin only cares
    /// about one core role or about its own roles — every plugin set still needs both, or
    /// `plugin::load_catalog` refuses it.
    pub fn with_default_core(self) -> Self {
        self.core("oat-meta", "Coordinate.").core("oat-console", "Observe.")
    }

    /// Pins this plugin (by its current content) in `repo`'s `.oat/plugins.toml` and trusts it
    /// on `world`'s machine, so `meta fire` / `console open` against `repo` pass the gate.
    pub fn finish(self, world: &TestWorld, repo: &Path, relative_path: &str, name: &str) -> PluginPin {
        let pin = plugins::path_pin_from_current_content(repo, relative_path, name).unwrap();
        write_plugins_config(repo, std::slice::from_ref(&pin));
        trust_pin(world, &pin);
        pin
    }
}

fn skills_toml_line(skills: &[&str]) -> String {
    if skills.is_empty() {
        String::new()
    } else {
        format!(
            "skills = [{}]\n",
            skills.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")
        )
    }
}

pub fn write_plugins_config(repo: &Path, pins: &[PluginPin]) {
    let path = repo.join(".oat/plugins.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, plugins::render_config(pins)).unwrap();
}

pub fn trust_key_for(pin: &PluginPin) -> TrustKey {
    match pin {
        PluginPin::Embedded { name } => TrustKey::Embedded {
            name: name.clone(),
            version: oat_agents::plugins::embedded::example_version().to_string(),
        },
        PluginPin::Git { url, commit, .. } => TrustKey::Git { url: url.clone(), commit: commit.clone() },
        PluginPin::Path { content_hash, .. } => TrustKey::Path { content_hash: content_hash.clone() },
    }
}

pub fn trust_pin(world: &TestWorld, pin: &PluginPin) {
    let mut store = TrustStore::open(&world.env()).unwrap();
    store.grant(trust_key_for(pin), "2026-01-01T00:00:00Z".to_string()).unwrap();
}

pub fn trust_pins(world: &TestWorld, pins: &[PluginPin]) {
    for pin in pins {
        trust_pin(world, pin);
    }
}

/// Copies a real plugin directory (e.g. one of `tests/fixtures/...`) into `repo` so it can be
/// pinned by path — a path pin only ever points inside the repository.
pub fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    }
}

/// The shared "pottery" fixture plugin several lifecycle tests assert instruction text against:
/// a `worker` role (fresh, carrying a `glazing` skill) and a `reviewer` role (existing), pinned
/// and trusted in one call.
/// A minimal plugin naming only `console_instructions` as its `oat-console` instructions (plus
/// the always-required `oat-meta` instructions), pinned and trusted in `repo`.
pub fn install_console_plugin(world: &TestWorld, repo: &Path, console_instructions: &str) {
    PluginBuilder::new(repo, "fixture-plugin", "fixture-plugin")
        .core("oat-meta", "Coordinate.")
        .core("oat-console", console_instructions)
        .finish(world, repo, "fixture-plugin", "fixture-plugin");
}

pub fn install_pottery_plugin(world: &TestWorld, repo: &Path) {
    PluginBuilder::new(repo, "fixture-plugin", "fixture-plugin")
        .role_with("worker", "fresh", "Write the pottery this task asks for.", &["glazing"], false, false)
        .role("reviewer", "existing", "Review the pottery someone else made.")
        .skill("glazing", &[("SKILL.md", b"# Glazing", false)])
        .core("oat-meta", "Coordinate the pottery workshop.")
        .core("oat-console", "Observe the pottery workshop.")
        .finish(world, repo, "fixture-plugin", "fixture-plugin");
}
