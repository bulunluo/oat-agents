// SPDX-License-Identifier: MIT
// Copyright (c) 2026 oat-agents contributors

use crate::environment::Environment;
use anyhow::{Context, Result, anyhow, ensure};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const STALE_AFTER: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Matches Claude Code's proper-lockfile protocol: mkdir, mtime renewal, rmdir.
/// No file lock can exclude a process using this directory lock.
struct ConfigLock {
    path: Option<PathBuf>,
    stop: Option<Sender<()>>,
    heartbeat: Option<JoinHandle<io::Result<()>>>,
}

impl ConfigLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let mut recovered_stale = false;
        loop {
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for Claude config lock"
            );
            match fs::create_dir(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = match fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    };
                    ensure!(metadata.is_dir(), "Claude config lock is not a directory");
                    if !recovered_stale
                        && metadata.modified()?.elapsed().unwrap_or_default() > STALE_AFTER
                    {
                        match fs::remove_dir(&path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        recovered_stale = true;
                        continue;
                    }
                }
                Err(error) => return Err(error.into()),
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Install cleanup before any fallible setup, including spawning the renewal thread.
        let mut guard = Self {
            path: Some(path.clone()),
            stop: None,
            heartbeat: None,
        };
        let directory = File::open(path)?;
        let (stop, receiver) = mpsc::channel();
        guard.stop = Some(stop);
        guard.heartbeat = Some(thread::Builder::new().spawn(move || {
            loop {
                match receiver.recv_timeout(REFRESH_INTERVAL) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        directory.set_modified(SystemTime::now())?
                    }
                    _ => return Ok(()),
                }
            }
        })?);
        Ok(guard)
    }

    fn stop_heartbeat(&mut self) -> Result<()> {
        self.stop.take(); // Disconnect wakes the thread immediately, even during unwinding.
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat
                .join()
                .map_err(|_| anyhow!("Claude config lock renewal panicked"))??;
        }
        Ok(())
    }

    fn release(mut self) -> Result<()> {
        let renewal = self.stop_heartbeat();
        let removal = fs::remove_dir(self.path.as_ref().unwrap());
        if removal.is_ok() {
            self.path = None;
        }
        renewal?;
        Ok(removal?)
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = self.stop_heartbeat();
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir(path);
        }
    }
}

struct PendingFile(PathBuf);

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Updates trust under Claude's lock. Callers report errors without preventing launch.
/// Use the launch Environment, which also supplies CLAUDE_CONFIG_DIR to the session
/// through session_environment, rather than reading the oat process environment directly.
/// Empty and absent values both become unset in launch.sh, so both fall back to HOME here.
pub fn trust_claude_workspace(env: &dyn Environment, worktree: &Path) -> Result<()> {
    let config_dir = env
        .var("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env.home_dir())
        .context("home directory unavailable for Claude workspace trust")?;
    let config = config_dir.join(".claude.json");
    let lock = ConfigLock::acquire(config_dir.join(".claude.json.lock"))?;
    let result = (|| -> Result<()> {
        // Keep the configured link intact. Resolve only after acquiring the config lock.
        let path = match fs::symlink_metadata(&config) {
            Ok(metadata) if metadata.is_symlink() => fs::canonicalize(&config)?,
            Ok(_) => config,
            Err(error) if error.kind() == io::ErrorKind::NotFound => config,
            Err(error) => return Err(error.into()),
        };
        let parent = path
            .parent()
            .context("Claude config has no parent directory")?;
        // Re-read after acquiring the lock; malformed or unreadable settings stay intact.
        let mut root: serde_json::Value = match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
            Err(error) => return Err(error.into()),
        };
        let projects = root
            .as_object_mut()
            .context("Claude config must be an object")?
            .entry("projects")
            .or_insert_with(|| serde_json::json!({}));
        let project = projects
            .as_object_mut()
            .context("Claude projects must be an object")?
            .entry(worktree.to_string_lossy().to_string())
            .or_insert_with(|| serde_json::json!({}));
        project
            .as_object_mut()
            .context("Claude project must be an object")?
            .insert("hasTrustDialogAccepted".into(), true.into());
        let permissions = match fs::metadata(&path) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let (pending, mut temp) = loop {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temp_path = parent.join(format!(
                ".claude.json.{}.{sequence}.tmp",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp_path)
            {
                Ok(file) => break (PendingFile(temp_path), file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        serde_json::to_writer_pretty(&mut temp, &root)?;
        temp.flush()?;
        if let Some(permissions) = permissions {
            temp.set_permissions(permissions)?;
        }
        temp.sync_all()?;
        fs::rename(&pending.0, &path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let released = lock.release();
    result.and(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::TestEnvironment;

    #[test]
    fn fresh_directory_lock_excludes_writers_until_released() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json.lock");
        fs::create_dir(&path).unwrap(); // Simulate Claude holding proper-lockfile's lock.
        thread::scope(|scope| {
            let writer = scope.spawn(|| {
                trust_claude_workspace(
                    &TestEnvironment::new(home.path().to_path_buf()),
                    Path::new("workspace"),
                )
            });
            thread::sleep(Duration::from_millis(250));
            assert!(!writer.is_finished());
            assert!(fs::symlink_metadata(&path).unwrap().is_dir());
            assert!(!home.path().join(".claude.json").exists());
            fs::write(
                home.path().join(".claude.json"),
                r#"{"projects":{"claude":{"hasTrustDialogAccepted":true}}}"#,
            )
            .unwrap();
            fs::remove_dir(&path).unwrap();
            writer.join().unwrap().unwrap();
        });
        assert!(!path.exists());
        let root: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join(".claude.json")).unwrap()).unwrap();
        assert_eq!(root["projects"]["claude"]["hasTrustDialogAccepted"], true);
        assert_eq!(
            root["projects"]["workspace"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn stale_directory_is_recovered_and_symlinked_config_keeps_its_target() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join(".claude.json.lock");
        fs::create_dir(&lock).unwrap();
        File::open(&lock)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(11))
            .unwrap();
        let target_dir = home.path().join("config");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("settings.json");
        fs::write(&target, r#"{"keep":42}"#).unwrap();
        let config = home.path().join(".claude.json");
        std::os::unix::fs::symlink("config/settings.json", &config).unwrap();
        trust_claude_workspace(
            &TestEnvironment::new(home.path().to_path_buf()),
            Path::new("workspace"),
        )
        .unwrap();
        assert!(fs::symlink_metadata(&config).unwrap().is_symlink());
        let root: serde_json::Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(root["keep"], 42);
        assert_eq!(
            root["projects"]["workspace"]["hasTrustDialogAccepted"],
            true
        );
        assert!(!lock.exists());
        assert_eq!(fs::read_dir(target_dir).unwrap().count(), 1);
    }

    #[test]
    fn lock_is_renewed_and_guards_clean_up_during_unwinding() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json.lock");
        let initial;
        {
            let _lock = ConfigLock::acquire(path.clone()).unwrap();
            assert!(fs::symlink_metadata(&path).unwrap().is_dir());
            initial = fs::metadata(&path).unwrap().modified().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while fs::metadata(&path).unwrap().modified().unwrap() <= initial {
                assert!(
                    Instant::now() < deadline,
                    "lock heartbeat did not renew mtime"
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
        assert!(!path.exists());
        let temp = home.path().join("pending.tmp");
        let unwound = std::panic::catch_unwind(|| {
            let _lock = ConfigLock::acquire(path.clone()).unwrap();
            fs::write(&temp, "pending").unwrap();
            let _pending = PendingFile(temp.clone());
            panic!("simulated interruption");
        });
        assert!(unwound.is_err());
        assert!(!path.exists());
        assert!(!temp.exists());
    }

    #[test]
    fn invalid_shapes_and_failed_rename_preserve_settings_and_remove_artifacts() {
        for contents in [
            "[]",
            r#"{"projects":[]}"#,
            r#"{"projects":{"workspace":false}}"#,
        ] {
            let home = tempfile::tempdir().unwrap();
            let config = home.path().join(".claude.json");
            fs::write(&config, contents).unwrap();
            assert!(
                trust_claude_workspace(
                    &TestEnvironment::new(home.path().to_path_buf()),
                    Path::new("workspace")
                )
                .is_err()
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), contents);
            assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
        }
        // A rename failure exercises temporary-file cleanup after creation.
        let home = tempfile::tempdir().unwrap();
        let temp = home.path().join("pending.tmp");
        let target = home.path().join("directory");
        fs::create_dir(&target).unwrap();
        {
            fs::write(&temp, "pending").unwrap();
            let pending = PendingFile(temp.clone());
            assert!(fs::rename(&pending.0, &target).is_err());
        }
        assert!(!temp.exists());
    }
}
