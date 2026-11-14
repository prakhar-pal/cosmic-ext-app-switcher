#[cfg(test)]
mod measurement_tests {
    use super::*;
    #[test]
    fn request_timestamp_round_trip_and_focus_correlation() {
        let request = Request {
            command: Command::Previous,
            sample: Some(Sample {
                pid: 42,
                timestamp_ns: 123456,
            }),
        };
        let decoded = Request::parse(request.encode().as_bytes()).unwrap();
        assert_eq!(decoded, request);
        let mut focus = Some(decoded.sample.unwrap());
        assert_eq!(
            take_focus(&mut focus, 223456),
            Some((
                Sample {
                    pid: 42,
                    timestamp_ns: 123456
                },
                0.1
            ))
        );
        assert_eq!(take_focus(&mut focus, 323456), None);
        assert!(Request::parse(b"next 42 nope").is_none());
    }
    #[test]
    fn memory_values_and_unavailable_pss() {
        let status = "VmRSS:\t123 kB\nVmHWM:\t456 kB\n";
        assert_eq!(
            memory_values(Some(status), Some("Pss: 78 kB\n")),
            (Some(123), Some(456), Some(78))
        );
        assert_eq!(
            memory_values(Some(status), None),
            (Some(123), Some(456), None)
        );
        assert_eq!(memory_values(None, Some("Rss: 9 kB\n")), (None, None, None));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    struct Runtime(std::path::PathBuf);
    impl Runtime {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "prototype-{}-{}",
                std::process::id(),
                ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn bind(&self) -> Listener {
            Listener::bind_at(paths_in(&self.0)).unwrap()
        }
    }
    impl Drop for Runtime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Catch accidentally binding the normal switcher's endpoints.
    #[test]
    fn endpoints_are_isolated() {
        let runtime = Runtime::new();
        let (socket, lock) = paths_in(&runtime.0);
        assert_ne!(socket, runtime.0.join("cosmic-ext-app-switcher.sock"));
        assert_ne!(lock, runtime.0.join("cosmic-ext-app-switcher.lock"));
        let _listener = runtime.bind();
        assert!(socket.exists());
        assert!(lock.exists());
    }

    // Catch reordered commands, wrong wire strings, or accepting invalid input.
    #[test]
    fn ordered_commands_ignore_invalid_messages() {
        use std::io::Write;
        let runtime = Runtime::new();
        let mut listener = runtime.bind();
        let socket = paths_in(&runtime.0).0;
        send_to(&socket, Command::Next).unwrap();
        for bytes in [b"invalid".as_slice()] {
            let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            stream.write_all(bytes).unwrap();
        }
        for command in [Command::Previous, Command::Stop] {
            send_to(&socket, command).unwrap();
        }
        let mut commands = Vec::new();
        for _ in 0..20 {
            if let Some(command) = listener.receive().unwrap() {
                commands.push(command);
            }
        }
        assert_eq!(commands, [Command::Next, Command::Previous, Command::Stop]);
    }

    // Catch removal of live sockets or failure to clean up crashed instances.
    #[test]
    fn lock_excludes_second_listener_and_stale_socket_recovers() {
        let runtime = Runtime::new();
        let paths = paths_in(&runtime.0);
        drop(std::os::unix::net::UnixListener::bind(&paths.0).unwrap());
        let listener = runtime.bind();
        assert!(Listener::bind_at(paths.clone()).is_err());
        assert!(std::os::unix::net::UnixStream::connect(&paths.0).is_ok());
        drop(listener);
        assert!(!paths.0.exists());
        let _replacement = runtime.bind();
    }

    #[test]
    fn disconnected_client_reports_failure() {
        let runtime = Runtime::new();
        assert!(send_to(&paths_in(&runtime.0).0, Command::Next).is_err());
    }

    #[test]
    fn partial_message_does_not_block_polling() {
        use std::io::Write;
        let runtime = Runtime::new();
        let mut listener = runtime.bind();
        let mut stream = UnixStream::connect(paths_in(&runtime.0).0).unwrap();
        stream.write_all(b"ne").unwrap();
        assert_eq!(listener.receive().unwrap(), None);
        stream.write_all(b"xt").unwrap();
        drop(stream);
        assert_eq!(listener.receive().unwrap(), Some(Command::Next));
    }

    #[test]
    fn stalled_client_expires_and_queued_commands_continue_in_order() {
        let runtime = Runtime::new();
        let mut listener = runtime.bind();
        let socket = paths_in(&runtime.0).0;
        let mut stalled = UnixStream::connect(&socket).unwrap();
        stalled.write_all(b"ne").unwrap();
        assert_eq!(listener.receive().unwrap(), None);
        for command in [Command::Next, Command::Previous, Command::Stop] {
            send_to(&socket, command).unwrap();
        }
        let started = std::time::Instant::now();
        let mut received = Vec::new();
        while started.elapsed() < std::time::Duration::from_secs(1) && received.len() < 3 {
            if let Some(command) = listener.receive().unwrap() {
                received.push(command);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(received, [Command::Next, Command::Previous, Command::Stop]);
        drop(stalled);
    }

    #[test]
    fn prototype_lock_file_persists_with_same_inode_between_owners() {
        use std::os::unix::fs::MetadataExt;
        let runtime = Runtime::new();
        let paths = paths_in(&runtime.0);
        let listener = runtime.bind();
        let inode = std::fs::metadata(&paths.1).unwrap().ino();
        assert!(Listener::bind_at(paths.clone()).is_err());
        drop(listener);
        assert!(
            paths.1.exists(),
            "lock file must remain after ownership ends"
        );
        let replacement = runtime.bind();
        assert_eq!(std::fs::metadata(&paths.1).unwrap().ino(), inode);
        assert!(Listener::bind_at(paths.clone()).is_err());
        drop(replacement);
        assert_eq!(std::fs::metadata(&paths.1).unwrap().ino(), inode);
    }

    #[test]
    fn daemon_worker_error_and_stale_surface_are_ignored() {
        use cosmic::app::Application;
        let runtime = Runtime::new();
        let (mut daemon, _) = Daemon::init(Core::default(), runtime.bind());
        daemon.lifecycle.command_request(Request {
            command: Command::Next,
            sample: None,
        });
        let (tx, rx) = std::sync::mpsc::channel();
        daemon.opening = Some(rx);
        tx.send(Err(anyhow::anyhow!("fixture worker failure")))
            .unwrap();
        let _ = daemon.update(DaemonMessage::Tick(std::time::Instant::now()));
        assert!(matches!(daemon.lifecycle.phase, Phase::Idle));
        assert!(daemon.overlay.is_none());
        let _ = daemon.overlay_message(daemon.lifecycle.generation - 1, OverlayMessage::Activate);
        assert!(matches!(daemon.lifecycle.phase, Phase::Idle));
    }

    // Exercise the production snapshot installation and close paths with a worker
    // that reports both its command and the sender being dropped.
    #[test]
    fn daemon_installs_cycles_closes_and_reopens_fresh_snapshots() {
        use crate::wayland::{ActivateCommand, ToplevelEntry};
        use std::sync::mpsc;
        fn snapshot(
            daemon: &mut Daemon,
            count: usize,
            base: usize,
        ) -> mpsc::Receiver<Vec<ActivateCommand>> {
            let (sender, receiver) = mpsc::sync_channel(1);
            let (done, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let commands = receiver.into_iter().collect();
                done.send(commands).unwrap();
            });
            let windows = (0..count)
                .map(|i| ToplevelEntry {
                    app_id: "test".into(),
                    title: "Fixture".into(),
                    is_active: i == 0,
                    handle_key: base + i,
                })
                .collect();
            let (tx, rx) = mpsc::channel();
            daemon.opening = Some(rx);
            tx.send(Ok((
                (windows, sender, worker),
                switcher_config::Theme::Dark,
            )))
            .unwrap();
            let _ = daemon.update(DaemonMessage::Tick(std::time::Instant::now()));
            result
        }
        let runtime = Runtime::new();
        let (mut daemon, _) = Daemon::init(Core::default(), runtime.bind());
        assert!(daemon.overlay.is_none());
        assert!(daemon.lifecycle.command_request(Request {
            command: Command::Next,
            sample: None,
        }));
        daemon.lifecycle.command_request(Request {
            command: Command::Previous,
            sample: None,
        });
        daemon.lifecycle.command_request(Request {
            command: Command::Next,
            sample: None,
        });
        let done = snapshot(&mut daemon, 3, 10);
        let generation = daemon.lifecycle.generation;
        let first_surface = daemon.overlay.as_ref().unwrap().surface_id();
        assert_eq!(daemon.overlay.as_ref().unwrap().selected, 1);
        send_to(&paths_in(&runtime.0).0, Command::Next).unwrap();
        let _ = daemon.update(DaemonMessage::Tick(std::time::Instant::now()));
        assert_eq!(daemon.overlay.as_ref().unwrap().selected, 2);
        let _ = daemon.overlay_message(generation, OverlayMessage::Activate);
        assert!(matches!(daemon.lifecycle.phase, Phase::Closing(None)));
        assert!(daemon.overlay.is_none() && daemon.worker.is_none());
        let _ = daemon.surface_done(generation, first_surface);
        assert!(matches!(daemon.lifecycle.phase, Phase::Idle));
        let commands = done
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            commands.as_slice(),
            [ActivateCommand::Activate(12)]
        ));

        assert!(daemon.lifecycle.command_request(Request {
            command: Command::Previous,
            sample: None,
        }));
        let done = snapshot(&mut daemon, 2, 20);
        let overlay = daemon.overlay.as_ref().unwrap();
        assert_eq!(overlay.selected, 1);
        assert_eq!(overlay.toplevels.len(), 2);
        assert_ne!(overlay.surface_id(), first_surface);
        let _ = daemon.overlay_message(generation, OverlayMessage::Cancel);
        assert!(
            daemon.overlay.is_some(),
            "old surface cannot close the new open"
        );
        let _ = daemon.overlay_message(daemon.lifecycle.generation, OverlayMessage::Cancel);
        let second_surface = daemon.closing_surface.unwrap().1;
        assert!(matches!(daemon.lifecycle.phase, Phase::Closing(None)));
        assert!(daemon.overlay.is_none() && daemon.worker.is_none());
        let generation = daemon.lifecycle.generation;
        let _ = daemon.surface_done(generation, first_surface);
        assert!(matches!(daemon.lifecycle.phase, Phase::Closing(None)));
        let _ = daemon.surface_done(generation, second_surface);
        assert!(matches!(daemon.lifecycle.phase, Phase::Idle));
        let commands = done
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(commands.as_slice(), [ActivateCommand::Cancel]));
    }

    #[test]
    fn protocol_distinguishes_open_directions_and_stop() {
        assert_eq!(Command::parse(b"next"), Some(Command::Next));
        assert_eq!(Command::parse(b"prev"), Some(Command::Previous));
        assert_eq!(Command::parse(b"stop"), Some(Command::Stop));
        assert_eq!(Command::parse(b"other"), None);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Next,
    Previous,
    Stop,
}

impl Command {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        match bytes {
            b"next" => Some(Self::Next),
            b"prev" => Some(Self::Previous),
            b"stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

// CLOCK_MONOTONIC has a common origin for processes on this boot.
fn monotonic_ns() -> Result<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    pid: u32,
    timestamp_ns: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Request {
    command: Command,
    sample: Option<Sample>,
}
impl Request {
    fn encode(&self) -> String {
        let command = match self.command {
            Command::Next => "next",
            Command::Previous => "prev",
            Command::Stop => "stop",
        };
        match self.sample {
            Some(sample) => format!("{command} {} {}", sample.pid, sample.timestamp_ns),
            None => command.into(),
        }
    }
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut fields = std::str::from_utf8(bytes).ok()?.split_ascii_whitespace();
        let command = Command::parse(fields.next()?.as_bytes())?;
        let sample = match fields.next() {
            Some(pid) => Some(Sample {
                pid: pid.parse().ok()?,
                timestamp_ns: fields.next()?.parse().ok()?,
            }),
            None => None,
        };
        if fields.next().is_some() {
            return None;
        }
        Some(Self { command, sample })
    }
}
fn take_focus(sample: &mut Option<Sample>, now: u64) -> Option<(Sample, f64)> {
    let sample = sample.take()?;
    Some((
        sample,
        now.checked_sub(sample.timestamp_ns)? as f64 / 1_000_000.0,
    ))
}
fn memory_values(
    status: Option<&str>,
    rollup: Option<&str>,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    fn value(text: Option<&str>, key: &str) -> Option<u64> {
        let line = text?.lines().find_map(|line| line.strip_prefix(key))?;
        let mut fields = line.split_whitespace();
        let value = fields.next()?.parse().ok()?;
        (fields.next()? == "kB").then_some(value)
    }
    (
        value(status, "VmRSS:"),
        value(status, "VmHWM:"),
        value(rollup, "Pss:"),
    )
}
fn log_memory(stage: &str) {
    let status = std::fs::read_to_string("/proc/self/status").ok();
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok();
    let (rss, peak, pss) = memory_values(status.as_deref(), rollup.as_deref());
    let show = |value: Option<u64>| {
        value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unavailable".into())
    };
    log::info!(
        "prototype memory stage={stage} daemon_pid={} rss_kib={} peak_rss_kib={} pss_kib={}",
        std::process::id(),
        show(rss),
        show(peak),
        show(pss)
    );
}

fn paths_in(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("cosmic-ext-app-switcher-prototype.sock"),
        dir.join("cosmic-ext-app-switcher-prototype.lock"),
    )
}

fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".into())
        .into()
}

pub fn socket_path() -> PathBuf {
    paths_in(&runtime_dir()).0
}
pub fn lock_path() -> PathBuf {
    paths_in(&runtime_dir()).1
}

pub fn send(command: Command) -> Result<()> {
    send_to(&socket_path(), command)
}

fn send_to(path: &Path, command: Command) -> Result<()> {
    // Timestamp in the shortcut client, before socket connect and daemon polling.
    let request = Request {
        command,
        sample: Some(Sample {
            pid: std::process::id(),
            timestamp_ns: monotonic_ns()?,
        }),
    };
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "could not connect to prototype daemon at {}",
            path.display()
        )
    })?;
    stream
        .write_all(request.encode().as_bytes())
        .context("could not send prototype command")?;
    Ok(())
}

/// Keep the inode stable across owners so flock always protects the same file.
struct PrototypeLock {
    _file: std::fs::File,
}
impl PrototypeLock {
    fn try_acquire(path: PathBuf) -> std::io::Result<Self> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Closing the file releases flock; never unlink its directory entry.
        Ok(Self { _file: file })
    }
}

const CLIENT_READ_DEADLINE: std::time::Duration = std::time::Duration::from_millis(100);

/// Owns the isolated endpoint and lock for the daemon's entire lifetime.
pub struct Listener {
    listener: UnixListener,
    pending: Option<(UnixStream, Vec<u8>, std::time::Instant)>,
    _cleaner: crate::SocketCleaner,
    _lock: PrototypeLock,
}

impl Listener {
    pub fn bind() -> Result<Self> {
        Self::bind_at((socket_path(), lock_path()))
    }

    fn bind_at((socket, lock): (PathBuf, PathBuf)) -> Result<Self> {
        // Never remove a socket until exclusive ownership has been established.
        let instance_lock = PrototypeLock::try_acquire(lock)
            .context("prototype daemon is already running or its lock is unavailable")?;
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&socket)?;
        let cleaner = crate::SocketCleaner(socket);
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            pending: None,
            _cleaner: cleaner,
            _lock: instance_lock,
        })
    }

    /// Poll one connection, preserving arrival order and never blocking the UI.
    /// Each connection carries one command and EOF terminates its message.
    #[cfg(test)]
    pub fn receive(&mut self) -> std::io::Result<Option<Command>> {
        Ok(self.receive_request()?.map(|r| r.command))
    }
    fn receive_request(&mut self) -> std::io::Result<Option<Request>> {
        // Bound head-of-line delay even when a client never closes its message.
        // Reads do not extend the deadline.
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, _, accepted)| accepted.elapsed() >= CLIENT_READ_DEADLINE)
        {
            self.pending = None;
        }
        if self.pending.is_none() {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    self.pending = Some((stream, Vec::new(), std::time::Instant::now()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let (stream, bytes, _) = self.pending.as_mut().unwrap();
        let mut buffer = [0; 16];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    let command = Request::parse(bytes);
                    self.pending = None;
                    return Ok(command);
                }
                Ok(size) => {
                    bytes.extend_from_slice(&buffer[..size]);
                    if bytes.len() > 96 {
                        self.pending = None;
                        return Ok(None);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.pending = None;
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn closing_waits_for_surface_done_and_last_request_wins() {
        let mut state = Lifecycle::default();
        assert!(state.command_request(Request {
            command: Command::Next,
            sample: None,
        }));
        let closing_generation = state.generation;

        state.begin_close();
        assert!(!state.command_request(Request {
            command: Command::Previous,
            sample: Some(Sample {
                pid: 10,
                timestamp_ns: 100,
            }),
        }));
        assert!(!state.command_request(Request {
            command: Command::Next,
            sample: Some(Sample {
                pid: 11,
                timestamp_ns: 200,
            }),
        }));

        assert!(matches!(state.phase, Phase::Closing(Some(_))));
        assert_eq!(
            state.surface_done(closing_generation + 1),
            SurfaceDone::Stale
        );
        assert!(matches!(state.phase, Phase::Closing(Some(_))));
        assert_eq!(
            state.surface_done(closing_generation),
            SurfaceDone::Reopen(Request {
                command: Command::Next,
                sample: Some(Sample {
                    pid: 11,
                    timestamp_ns: 200,
                }),
            })
        );
        assert_eq!(state.generation, closing_generation + 1);
        assert!(
            matches!(state.phase, Phase::Opening(ref commands) if commands == &[Command::Next])
        );
    }

    #[test]
    fn surface_done_without_pending_open_returns_to_idle() {
        let mut state = Lifecycle::default();
        assert!(state.command_request(Request {
            command: Command::Previous,
            sample: None,
        }));
        let generation = state.generation;
        state.begin_close();

        assert_eq!(state.surface_done(generation), SurfaceDone::Closed);
        assert!(matches!(state.phase, Phase::Idle));
        assert_eq!(state.surface_done(generation), SurfaceDone::Stale);
    }

    #[test]
    fn opening_retains_fifo_and_reopens_fresh() {
        let mut state = Lifecycle::default();
        assert!(state.command_request(Request {
            command: Command::Next,
            sample: None
        }));
        let generation = state.generation;
        state.command_request(Request {
            command: Command::Previous,
            sample: None,
        });
        state.command_request(Request {
            command: Command::Previous,
            sample: None,
        });
        assert_eq!(state.snapshot(generation, 4), Some(3));
        state.begin_close();
        assert_eq!(state.surface_done(generation), SurfaceDone::Closed);
        assert!(state.command_request(Request {
            command: Command::Previous,
            sample: None
        }));
        assert_eq!(state.snapshot(generation, 7), None);
        assert_eq!(state.snapshot(state.generation, 3), Some(2));
    }
    #[test]
    fn empty_and_failed_snapshots_return_idle() {
        let mut state = Lifecycle::default();
        state.command_request(Request {
            command: Command::Next,
            sample: None,
        });
        assert_eq!(state.snapshot(state.generation, 0), None);
        assert!(matches!(state.phase, Phase::Idle));
        assert!(state.command_request(Request {
            command: Command::Next,
            sample: None
        }));
        state.abort();
        assert!(matches!(state.phase, Phase::Idle));
    }
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Opening(Vec<Command>),
    Visible,
    Closing(Option<Request>),
}
#[derive(Debug, PartialEq, Eq)]
enum SurfaceDone {
    Stale,
    Closed,
    Reopen(Request),
}
#[derive(Default)]
struct Lifecycle {
    phase: Phase,
    generation: u64,
}
impl Lifecycle {
    fn command_request(&mut self, request: Request) -> bool {
        match &mut self.phase {
            Phase::Idle => {
                self.generation += 1;
                self.phase = Phase::Opening(vec![request.command]);
                true
            }
            Phase::Opening(queue) => {
                queue.push(request.command);
                false
            }
            Phase::Visible => false,
            Phase::Closing(pending) => {
                *pending = Some(request);
                false
            }
        }
    }
    fn snapshot(&mut self, generation: u64, count: usize) -> Option<usize> {
        if generation != self.generation {
            return None;
        }
        let Phase::Opening(queue) = &self.phase else {
            return None;
        };
        if count == 0 {
            self.abort();
            return None;
        }
        let mut selected = 0;
        for command in queue {
            selected = match command {
                Command::Next => (selected + 1) % count,
                Command::Previous => (selected + count - 1) % count,
                Command::Stop => selected,
            };
        }
        self.phase = Phase::Visible;
        Some(selected)
    }
    fn begin_close(&mut self) {
        self.phase = Phase::Closing(None);
    }
    fn surface_done(&mut self, generation: u64) -> SurfaceDone {
        if generation != self.generation {
            return SurfaceDone::Stale;
        }
        let Phase::Closing(pending) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return SurfaceDone::Stale;
        };
        if let Some(request) = pending {
            self.command_request(request);
            SurfaceDone::Reopen(request)
        } else {
            SurfaceDone::Closed
        }
    }
    fn abort(&mut self) {
        self.phase = Phase::Idle;
    }
}

use crate::app::{AppSwitcher, Message as OverlayMessage};
use cosmic::{
    Element,
    app::{Application, Core, Settings, Task},
    iced::{self, Subscription, window::Id},
};
type Worker = (
    Vec<crate::wayland::ToplevelEntry>,
    std::sync::mpsc::SyncSender<crate::wayland::ActivateCommand>,
    std::thread::JoinHandle<()>,
);
struct Daemon {
    core: Core,
    focus_sample: Option<Sample>,
    listener: Listener,
    lifecycle: Lifecycle,
    overlay: Option<AppSwitcher>,
    closing_surface: Option<(u64, Id)>,
    worker: Option<std::thread::JoinHandle<()>>,
    opening: Option<std::sync::mpsc::Receiver<Result<(Worker, switcher_config::Theme)>>>,
}
#[derive(Debug, Clone)]
enum DaemonMessage {
    Tick(std::time::Instant),
    Overlay(u64, OverlayMessage),
    Focused(u64),
    SurfaceDone(u64, Id),
}
impl Daemon {
    fn finish(&mut self) {
        let closing_generation = self.lifecycle.generation;
        let closing_surface = self.overlay.as_ref().map(AppSwitcher::surface_id);
        log::info!(
            "prototype surface destroy_requested daemon_pid={} generation={} surface={:?}",
            std::process::id(),
            closing_generation,
            closing_surface
        );
        if let Some(surface) = closing_surface {
            self.lifecycle.begin_close();
            self.closing_surface = Some((closing_generation, surface));
        } else {
            self.lifecycle.abort();
            self.closing_surface = None;
        }
        self.overlay.take();
        if let Some(worker) = self.worker.take() {
            std::thread::spawn(move || {
                let _ = worker.join();
            });
        }
        log::info!(
            "prototype overlay state dropped daemon_pid={} generation={} awaiting_surface_done={:?}",
            std::process::id(),
            closing_generation,
            self.closing_surface
        );
        self.focus_sample = None;
        log_memory("after_close");
    }
    fn start_opening(&mut self, sample: Option<Sample>) {
        self.focus_sample = sample;
        if let Some(sample) = sample {
            log::info!(
                "prototype open daemon_pid={} request_id={}:{}",
                std::process::id(),
                sample.pid,
                sample.timestamp_ns
            );
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.opening = Some(rx);
        std::thread::spawn(move || {
            let result = (|| {
                let theme = crate::load_theme();
                let worker = crate::wayland::spawn_wayland_thread(crate::load_scope())?;
                Ok((worker, theme))
            })();
            if let Err(error) = tx.send(result) {
                if let Ok(((windows, sender, worker), _)) = error.0 {
                    drop(windows);
                    drop(sender);
                    let _ = worker.join();
                }
            }
        });
    }
    fn surface_done(&mut self, generation: u64, surface: Id) -> Task<DaemonMessage> {
        if self.closing_surface != Some((generation, surface)) {
            log::debug!(
                "prototype stale surface done daemon_pid={} generation={} surface={:?} waiting_for={:?}",
                std::process::id(),
                generation,
                surface,
                self.closing_surface
            );
            return Task::none();
        }
        self.closing_surface = None;
        log::info!(
            "prototype surface teardown_confirmed daemon_pid={} generation={} surface={:?}",
            std::process::id(),
            generation,
            surface
        );
        match self.lifecycle.surface_done(generation) {
            SurfaceDone::Stale => Task::none(),
            SurfaceDone::Closed => {
                self.focus_sample = None;
                log_memory("after_surface_done");
                Task::none()
            }
            SurfaceDone::Reopen(request) => {
                self.start_opening(request.sample);
                Task::none()
            }
        }
    }
    fn overlay_message(&mut self, generation: u64, message: OverlayMessage) -> Task<DaemonMessage> {
        if generation != self.lifecycle.generation {
            return Task::none();
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return Task::none();
        };
        let task = overlay
            .update_session(message, true)
            .map(move |action| map_overlay_action(generation, action));
        if overlay.toplevels.is_empty() {
            self.finish();
        }
        task
    }
}
impl Application for Daemon {
    type Executor = cosmic::executor::Default;
    type Flags = Listener;
    type Message = DaemonMessage;
    const APP_ID: &'static str = "io.github.cosmic-ext-app-switcher.prototype";
    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }
    fn init(core: Core, listener: Listener) -> (Self, Task<Self::Message>) {
        log_memory("startup_idle");
        (
            Self {
                core,
                focus_sample: None,
                listener,
                lifecycle: Lifecycle::default(),
                overlay: None,
                closing_surface: None,
                worker: None,
                opening: None,
            },
            Task::none(),
        )
    }
    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            DaemonMessage::Focused(generation) => {
                log::info!(
                    "prototype surface focused daemon_pid={} generation={} current_generation={} visible={}",
                    std::process::id(),
                    generation,
                    self.lifecycle.generation,
                    self.overlay.is_some()
                );
                if generation == self.lifecycle.generation && self.overlay.is_some() {
                    if let Ok(now) = monotonic_ns() {
                        if let Some((sample, elapsed)) = take_focus(&mut self.focus_sample, now) {
                            log::info!(
                                "prototype focus daemon_pid={} request_id={}:{} request_to_focus_ms={elapsed:.3}",
                                std::process::id(),
                                sample.pid,
                                sample.timestamp_ns
                            );
                            log_memory("focus");
                        }
                    }
                }
                return Task::none();
            }
            DaemonMessage::SurfaceDone(generation, surface) => {
                return self.surface_done(generation, surface);
            }
            DaemonMessage::Overlay(generation, message) => {
                return self.overlay_message(generation, message);
            }
            DaemonMessage::Tick(now) => {
                match self.listener.receive_request() {
                    Ok(Some(Request {
                        command: Command::Stop,
                        ..
                    })) => {
                        self.finish();
                        return iced::exit();
                    }
                    Ok(Some(request)) => {
                        let command = request.command;
                        if matches!(self.lifecycle.phase, Phase::Visible) {
                            let message = if command == Command::Previous {
                                OverlayMessage::SelectPrev
                            } else {
                                OverlayMessage::SelectNext
                            };
                            return self.overlay_message(self.lifecycle.generation, message);
                        }
                        if self.lifecycle.command_request(request) {
                            self.start_opening(request.sample);
                        }
                    }
                    Err(error) => log::error!("prototype command: {error}"),
                    _ => {}
                }
                let ready = self.opening.as_ref().and_then(|rx| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some(Err(anyhow::anyhow!("snapshot worker disconnected")))
                    }
                    _ => None,
                });
                if let Some(result) = ready {
                    self.opening = None;
                    match result {
                        Ok(((windows, sender, worker), theme)) => {
                            log::info!(
                                "prototype snapshot daemon_pid={} windows={}",
                                std::process::id(),
                                windows.len()
                            );
                            let generation = self.lifecycle.generation;
                            if let Some(selected) =
                                self.lifecycle.snapshot(generation, windows.len())
                            {
                                let (mut overlay, task) = AppSwitcher::init(
                                    Core::default(),
                                    (windows, false, true, sender, theme, None),
                                );
                                overlay.selected = selected;
                                log::info!(
                                    "prototype surface create_requested daemon_pid={} generation={} surface={:?} windows={} selected={}",
                                    std::process::id(),
                                    generation,
                                    overlay.surface_id(),
                                    overlay.toplevels.len(),
                                    selected
                                );
                                self.worker = Some(worker);
                                self.overlay = Some(overlay);
                                return task
                                    .map(move |action| map_overlay_action(generation, action));
                            }
                            self.focus_sample = None;
                            log_memory("after_close");
                            log::info!("prototype snapshot has no windows; returning to idle");
                            drop(sender);
                            std::thread::spawn(move || {
                                let _ = worker.join();
                            });
                        }
                        Err(error) => {
                            log::warn!("prototype snapshot: {error:#}");
                            self.finish();
                        }
                    }
                }
                return self.overlay_message(self.lifecycle.generation, OverlayMessage::Tick(now));
            }
        }
    }
    fn subscription(&self) -> Subscription<Self::Message> {
        let generation = self.lifecycle.generation;
        let surface = self
            .overlay
            .as_ref()
            .map(AppSwitcher::surface_id)
            .or_else(|| self.closing_surface.map(|(_, surface)| surface));
        let events = iced::event::listen_with(|event, _, window| Some((event, window)))
            .with((generation, surface))
            .filter_map(|((generation, surface), (event, window))| {
                if Some(window) != surface {
                    return None;
                }
                if matches!(
                    &event,
                    iced::Event::PlatformSpecific(
                        iced::event::PlatformSpecific::Wayland(
                            iced::event::wayland::Event::Layer(
                                iced::event::wayland::LayerEvent::Done,
                                _,
                                _,
                            ),
                        ),
                    )
                ) {
                    log::info!("prototype surface done_event_received daemon_pid={} generation={} surface={:?}", std::process::id(), generation, surface);
                    return surface.map(|surface| DaemonMessage::SurfaceDone(generation, surface));
                }
                if matches!(&event, iced::Event::Window(_)) {
                    log::info!("prototype surface window_event generation={} surface={:?} event={:?}", generation, surface, event);
                }
                if matches!(event, iced::Event::Window(iced::window::Event::Focused)) {
                    return Some(DaemonMessage::Focused(generation));
                }
                let message = match event {
                    iced::Event::Keyboard(event) => OverlayMessage::KeyEvent(event),
                    iced::Event::Window(iced::window::Event::Unfocused) => OverlayMessage::Cancel,
                    _ => return None,
                };
                Some(DaemonMessage::Overlay(generation, message))
            });
        Subscription::batch([
            events,
            iced::time::every(std::time::Duration::from_millis(10)).map(DaemonMessage::Tick),
        ])
    }
    fn view(&self) -> Element<'_, Self::Message> {
        match self.overlay.as_ref() {
            Some(overlay) => {
                let generation = self.lifecycle.generation;
                overlay
                    .view()
                    .map(move |m| DaemonMessage::Overlay(generation, m))
            }
            None => cosmic::widget::text("").into(),
        }
    }
    fn view_window(&self, id: Id) -> Element<'_, Self::Message> {
        if self
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.surface_id() == id)
        {
            self.view()
        } else {
            cosmic::widget::text("").into()
        }
    }
}
pub fn run_daemon() -> Result<()> {
    let listener = Listener::bind()?;
    cosmic::app::run::<Daemon>(Settings::default().no_main_window(true), listener)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

fn map_overlay_action(
    generation: u64,
    action: cosmic::Action<OverlayMessage>,
) -> cosmic::Action<DaemonMessage> {
    match action {
        cosmic::Action::App(message) => {
            cosmic::Action::App(DaemonMessage::Overlay(generation, message))
        }
        cosmic::Action::Cosmic(action) => cosmic::Action::Cosmic(action),
        cosmic::Action::None => cosmic::Action::None,
    }
}
