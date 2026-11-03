mod app;
mod icons;
mod ui;
mod wayland;
mod prototype;

use anyhow::Result;
use clap::Parser;
use std::io::Write;
use std::os::unix::net::UnixStream;
use cosmic::cosmic_config::ConfigGet;
use switcher_config::{Theme, WorkspaceScope, APP_ID, CONFIG_VERSION};


pub fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(dir).join("cosmic-ext-app-switcher.sock")
}

pub fn lock_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(dir).join("cosmic-ext-app-switcher.lock")
}

pub struct InstanceLock {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl InstanceLock {
    pub fn try_acquire(path: std::path::PathBuf) -> Result<Self, std::io::Error> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;

        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(InstanceLock { file, path })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct SocketCleaner(pub std::path::PathBuf);

impl Drop for SocketCleaner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Parser, Debug)]
#[command(name = "cosmic-ext-app-switcher", version)]
pub struct Args {
    #[arg(long, default_value_t = false)]
    pub reverse: bool,

    #[arg(long, hide = true)]
    pub shortcut: bool,

    #[arg(long, hide = true, group = "prototype_mode")]
    pub prototype_daemon: bool,
    #[arg(long, hide = true, group = "prototype_mode")]
    pub prototype_next: bool,
    #[arg(long, hide = true, group = "prototype_mode")]
    pub prototype_prev: bool,
    #[arg(long, hide = true, group = "prototype_mode")]
    pub prototype_stop: bool,

    /// Report whether the compositor offers the Wayland interfaces the switcher needs, then exit
    #[arg(long, default_value_t = false)]
    pub check_compat: bool,
}

fn load_theme() -> Theme {
    use cosmic::cosmic_config::Config;
    Config::new(APP_ID, CONFIG_VERSION)
        .ok()
        .and_then(|c| c.get::<Theme>("theme").ok())
        .unwrap_or_default()
}

fn load_scope() -> WorkspaceScope {
    use cosmic::cosmic_config::Config;
    Config::new(APP_ID, CONFIG_VERSION)
        .ok()
        .and_then(|c| c.get::<WorkspaceScope>("workspace_scope").ok())
        .unwrap_or_default()
}

/// Prints one line per required Wayland global. Exits with status 1 if any is unusable.
fn check_compat() -> Result<()> {
    let statuses = wayland::probe_required_globals()?;
    for status in &statuses {
        println!("{status}");
    }
    if !statuses.iter().all(wayland::GlobalStatus::ok) {
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    if args.prototype_daemon {
        return prototype::run_daemon();
    }
    let prototype_command = if args.prototype_next {
        Some(prototype::Command::Next)
    } else if args.prototype_prev {
        Some(prototype::Command::Previous)
    } else if args.prototype_stop {
        Some(prototype::Command::Stop)
    } else {
        None
    };
    if let Some(command) = prototype_command {
        return prototype::send(command);
    }

    if args.check_compat {
        return check_compat();
    }

    // If a switcher is already running, signal it to cycle and exit.
    let cmd = if args.reverse { b"prev" as &[u8] } else { b"next" as &[u8] };
    if let Ok(mut s) = UnixStream::connect(socket_path()) {
        let _ = s.write_all(cmd);
        return Ok(());
    }

    // Ensure only one instance runs at a time.
    let _instance_lock = match InstanceLock::try_acquire(lock_path()) {
        Ok(lock) => lock,
        Err(_) => {
            // Another instance holds the lock. Retry connecting to its socket in case
            // it was still binding when we checked above.
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if let Ok(mut s) = UnixStream::connect(socket_path()) {
                    let _ = s.write_all(cmd);
                    return Ok(());
                }
            }
            log::warn!("another instance of cosmic-ext-app-switcher is already live; exiting");
            return Ok(());
        }
    };

    // We are the only instance. Clean up any stale socket from a crash, then
    // immediately bind the listener so subsequent rapid invocations can connect
    // and queue without spawning duplicate switcher instances.
    let _ = std::fs::remove_file(socket_path());
    let _cleaner = SocketCleaner(socket_path());
    let listener = match std::os::unix::net::UnixListener::bind(socket_path()) {
        Ok(l) => {
            let _ = l.set_nonblocking(true);
            Some(l)
        }
        Err(e) => {
            log::warn!("could not pre-bind socket at {:?}: {e}", socket_path());
            None
        }
    };

    let theme = load_theme();
    let scope = load_scope();
    let (toplevels, cmd_tx, join_handle) = wayland::spawn_wayland_thread(scope)?;
    if toplevels.is_empty() {
        drop(cmd_tx);
        let _ = join_handle.join();
        return Ok(());
    }

    let run_res = app::run(toplevels, args.reverse, args.shortcut, cmd_tx, theme, listener);
    let _ = join_handle.join();
    run_res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_lock_fails_while_first_is_held() {
        let temp = std::env::temp_dir().join(format!("instance-lock-test-{}", std::process::id()));
        let lock1 = InstanceLock::try_acquire(temp.clone()).expect("first lock should succeed");

        let lock2 = InstanceLock::try_acquire(temp.clone());
        assert!(lock2.is_err(), "second lock acquisition should fail while first is held");

        drop(lock1);
        let lock3 = InstanceLock::try_acquire(temp.clone());
        assert!(lock3.is_ok(), "lock acquisition should succeed after previous is dropped");
    }

    #[test]
    fn socket_cleaner_removes_file_on_drop() {
        let temp = std::env::temp_dir().join(format!("socket-cleaner-test-{}", std::process::id()));
        std::fs::write(&temp, b"test").unwrap();
        assert!(temp.exists());

        {
            let _cleaner = SocketCleaner(temp.clone());
        }

        assert!(!temp.exists(), "SocketCleaner should remove file on drop");
    }
}
