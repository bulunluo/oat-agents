// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::environment::Environment;
use crate::error::{codes, err};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// The oat-agents private tmux server: executable and socket are overridable so tests can
/// substitute a recording script for a real tmux (ticket §7.1).
pub struct Tmux {
    command: String,
    socket: String,
}

impl Tmux {
    pub fn from_env(env: &dyn Environment) -> Self {
        Self {
            command: env.var_or("OAT_TMUX_COMMAND", "tmux"),
            socket: env.var_or("OAT_TMUX_SOCKET", "oat"),
        }
    }

    fn base_args(&self) -> Vec<String> {
        vec!["-L".to_string(), self.socket.clone()]
    }

    fn run(&self, args: &[String]) -> Result<std::process::Output> {
        Command::new(&self.command)
            .args(args)
            .output()
            .map_err(|e| {
                err(
                    codes::TMUX_MISSING,
                    format!("could not run '{}': {e}", self.command),
                )
            })
    }

    pub fn check_available(&self) -> Result<()> {
        let mut args = self.base_args();
        args.push("list-sessions".to_string());
        // A private server with no sessions yet exits nonzero with a "no server" message,
        // which is not a missing tmux; only a failure to exec the binary at all is fatal here.
        self.run(&args).map(|_| ())
    }

    /// The command a person runs to attach to one session on this private server. A plain
    /// `tmux attach` looks on the default server and does not find it.
    pub fn attach_command(&self, session: &str) -> String {
        format!("{} -L {} attach -t {session}", self.command, self.socket)
    }

    /// `oat_<hash_id>` with `.` and `:` replaced by `_`, since tmux treats both as separators.
    pub fn session_name(hash_id: &str) -> String {
        format!("oat_{}", hash_id.replace(['.', ':'], "_"))
    }

    /// The exact argv `new-session` is invoked with, exposed so a test can assert on it
    /// (ticket §7.1) without starting anything real.
    pub fn launch_argv(
        &self,
        session: &str,
        cwd: &Path,
        env_vars: &[(String, String)],
        command: &str,
    ) -> Vec<String> {
        let mut args = self.base_args();
        args.push("new-session".to_string());
        args.push("-d".to_string());
        args.push("-s".to_string());
        args.push(session.to_string());
        args.push("-c".to_string());
        args.push(cwd.to_string_lossy().to_string());
        for (k, v) in env_vars {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push("--".to_string());
        args.push(command.to_string());
        args
    }

    pub fn start_session(
        &self,
        session: &str,
        cwd: &Path,
        env_vars: &[(String, String)],
        command: &str,
    ) -> Result<()> {
        let args = self.launch_argv(session, cwd, env_vars, command);
        let output = self.run(&args)?;
        if !output.status.success() {
            return Err(err(
                codes::INTERNAL_ERROR,
                format!(
                    "tmux new-session failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }
        Ok(())
    }

    pub fn session_alive(&self, session: &str) -> Result<bool> {
        let mut args = self.base_args();
        args.push("has-session".to_string());
        args.push("-t".to_string());
        args.push(session.to_string());
        let output = self.run(&args)?;
        Ok(output.status.success())
    }

    pub fn kill_session(&self, session: &str) -> Result<()> {
        let mut args = self.base_args();
        args.push("kill-session".to_string());
        args.push("-t".to_string());
        args.push(session.to_string());
        let _ = self.run(&args)?;
        Ok(())
    }

    /// The visible screen of a session, for `dispatch read --source terminal`.
    pub fn capture_pane(&self, session: &str) -> Result<String> {
        let mut args = self.base_args();
        args.push("capture-pane".to_string());
        args.push("-p".to_string());
        args.push("-t".to_string());
        args.push(session.to_string());
        let output = self.run(&args)?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Types literal text into a session without attaching to it (the live view's live tab).
    pub fn send_text(&self, session: &str, text: &str) -> Result<()> {
        let mut args = self.base_args();
        args.push("send-keys".to_string());
        args.push("-t".to_string());
        args.push(session.to_string());
        args.push("-l".to_string());
        args.push(text.to_string());
        let output = self.run(&args)?;
        if !output.status.success() {
            return Err(err(
                codes::INTERNAL_ERROR,
                format!("tmux send-keys failed: {}", String::from_utf8_lossy(&output.stderr)),
            ));
        }
        Ok(())
    }

    /// Sends a named key (`Enter`, `Escape`, ...) rather than literal text.
    pub fn send_key(&self, session: &str, key: &str) -> Result<()> {
        let mut args = self.base_args();
        args.push("send-keys".to_string());
        args.push("-t".to_string());
        args.push(session.to_string());
        args.push(key.to_string());
        let output = self.run(&args)?;
        if !output.status.success() {
            return Err(err(
                codes::INTERNAL_ERROR,
                format!("tmux send-keys failed: {}", String::from_utf8_lossy(&output.stderr)),
            ));
        }
        Ok(())
    }

    /// Resizes a session's window, so the live tab's pane matches the live view's pane size.
    /// The window is pinned to that size first, or tmux sizes it back to whichever client is
    /// attached.
    pub fn resize_window(&self, session: &str, cols: u16, rows: u16) -> Result<()> {
        let target = pane(session);
        self.checked(&["set-option", "-t", &target, "window-size", "manual"])?;
        let cols = cols.max(20).to_string();
        let rows = rows.max(5).to_string();
        self.checked(&["resize-window", "-t", &target, "-x", &cols, "-y", &rows])?;
        Ok(())
    }

    /// The visible screen with its colours (SGR escapes), as tmux draws it — the live tab's
    /// view of a session.
    pub fn capture(&self, session: &str) -> Result<String> {
        self.capture_with(session, &[])
    }

    /// The whole scrollback plus the screen, with colours, for reading back on the live tab.
    pub fn capture_history(&self, session: &str) -> Result<String> {
        self.capture_with(session, &["-S", "-", "-E", "-"])
    }

    fn capture_with(&self, session: &str, range: &[&str]) -> Result<String> {
        let target = pane(session);
        let mut args = vec!["capture-pane", "-p", "-e", "-J", "-t", &target];
        args.extend_from_slice(range);
        let output = self.checked(&args)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Pastes `text` into the session as one bracketed paste, without pressing Enter: what a
    /// terminal does when a person pastes.
    pub fn paste(&self, session: &str, text: &str) -> Result<()> {
        use std::io::Write;
        use std::process::Stdio;
        let mut args = self.base_args();
        args.extend(["load-buffer", "-b", "oat", "-"].map(String::from));
        let mut child = Command::new(&self.command)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| err(codes::TMUX_MISSING, format!("could not run '{}': {e}", self.command)))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(failed("load-buffer", &output));
        }
        self.checked(&["paste-buffer", "-p", "-d", "-b", "oat", "-t", &pane(session)])?;
        Ok(())
    }

    /// Whether the program in the session is on the alternate screen, and whether it asked
    /// for mouse reports — which decide who scrolls it.
    pub fn screen_mode(&self, session: &str) -> Option<(bool, bool)> {
        let text = self.display(
            session,
            "#{alternate_on} #{?#{||:#{mouse_any_flag},#{||:#{mouse_button_flag},#{mouse_standard_flag}}},1,0}",
        )?;
        let mut fields = text.split_whitespace();
        Some((fields.next()? == "1", fields.next()? == "1"))
    }

    /// Where the session's cursor is, when the program in it shows one.
    pub fn cursor(&self, session: &str) -> Option<(u16, u16)> {
        let text = self.display(session, "#{cursor_x} #{cursor_y} #{cursor_flag}")?;
        let mut fields = text.split_whitespace();
        let x = fields.next()?.parse().ok()?;
        let y = fields.next()?.parse().ok()?;
        (fields.next()? == "1").then_some((x, y))
    }

    /// Read the agent pane's process id without changing the session.
    pub fn pane_pid(&self, session: &str) -> Option<u32> {
        self.display(session, "#{pane_pid}")?.trim().parse().ok().filter(|pid| *pid > 0)
    }

    /// Read structured pane death and window activity; unavailable values remain unknown.
    pub fn activity(&self, session: &str) -> (Option<bool>, Option<i64>) {
        self.display(session, "#{pane_dead} #{window_activity}")
            .map(|text| parse_activity(&text))
            .unwrap_or_default()
    }

    fn display(&self, session: &str, format: &str) -> Option<String> {
        let output = self
            .checked(&["display-message", "-p", "-t", &pane(session), format])
            .ok()?;
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn checked(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut all = self.base_args();
        all.extend(args.iter().map(|arg| arg.to_string()));
        let output = self.run(&all)?;
        if !output.status.success() {
            return Err(failed(args.first().copied().unwrap_or("tmux"), &output));
        }
        Ok(output)
    }
}

/// `=name:` targets exactly that session's pane: without the `=` tmux matches a prefix, and
/// without the colon a pane command reads the whole string as a pane name.
fn pane(session: &str) -> String {
    format!("={session}:")
}

fn failed(what: &str, output: &std::process::Output) -> anyhow::Error {
    err(
        codes::INTERNAL_ERROR,
        format!("tmux {what} failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
    )
}

fn parse_activity(text: &str) -> (Option<bool>, Option<i64>) {
    let mut fields = text.split_whitespace();
    let dead = match fields.next() {
        Some("0") => Some(false),
        Some("1") => Some(true),
        _ => None,
    };
    let activity = fields.next().and_then(|value| value.parse().ok()).filter(|epoch| *epoch > 0);
    (dead, activity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_structured_and_missing_values_stay_unknown() {
        assert_eq!(parse_activity("1 1700000000\n"), (Some(true), Some(1700000000)));
        assert_eq!(parse_activity("0 0"), (Some(false), None));
        assert_eq!(parse_activity("invalid nope"), (None, None));
        assert_eq!(parse_activity(""), (None, None));
    }
}
