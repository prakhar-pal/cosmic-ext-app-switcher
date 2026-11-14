# Persistent Switcher Prototype Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure whether keeping the COSMIC application alive reduces shortcut-to-focus latency, and quantify idle and active memory.

**Architecture:** An opt-in prototype daemon owns a separate Unix socket and keeps an initially hidden COSMIC application alive. Each open creates a fresh one-shot Wayland worker and layer surface; closing destroys the surface and worker but retains the application. Shortcut clients send commands to the daemon and record monotonic timestamps for latency measurements.

**Tech Stack:** Rust, libcosmic/iced, Wayland, Tokio Unix sockets, Linux `/proc`.

**Spec:** The persistent-process prototype plan approved in this conversation (no separate spec file).

## Global Constraints

- Prototype is opt-in and manually started and stopped; do not change installed shortcuts or session autostart.
- Use separate prototype socket and lock names; retain normal switcher behavior.
- Refresh window list and configuration on every open; preserve forward, reverse, rapid cycling and modifier-release behavior.
- Baseline: release build approximately 292 ms process-start-to-focus and 19 MiB RSS at focus; focus is not first-frame time.
- Existing uncommitted `src/prototype.rs` command parser and `mod prototype` in `src/main.rs` are partial work, not a completed daemon; preserve and extend them.

## Review Focus

- Overlapping opens and queued cycles must preserve command order and not lose the first selection (Task 2 tests).
- An empty snapshot or failed Wayland worker must leave the daemon usable (Task 2 tests).
- A stale socket or a second daemon must not disrupt the normal switcher (Task 1 tests).
- Modifier and timer state from a previous overlay must not close a later overlay (Task 2 tests).
- Worker shutdown and stale surface events must not activate a new snapshot (Task 2 tests).

### Task 1: Isolated daemon protocol

**Files:** Modify `src/prototype.rs`, `src/main.rs`; tests in `src/prototype.rs`.

**Interfaces:** Consumes existing `Command::{Next, Previous, Stop}` and `Command::parse(&[u8]) -> Option<Command>`. Produces `prototype::socket_path() -> PathBuf`, `prototype::lock_path() -> PathBuf`, and `prototype::send(command: Command) -> anyhow::Result<()>`; daemon listener sends parsed commands to the UI in arrival order.

- [ ] **Step 1: Write failing tests.** Add socket tests with a temporary runtime directory asserting prototype and normal socket/lock paths differ; forward, reverse, and stop messages arrive in order; invalid messages are ignored; a second lock acquisition fails; stale socket recovery works. Use unique test directories and no live desktop.
- [ ] **Step 2: Verify RED.** Run `cargo test --offline -p cosmic-ext-app-switcher prototype::tests`; expected: failures for the missing isolated listener/client behavior.
- [ ] **Step 3: Implement the protocol.** Add hidden CLI modes `--prototype-daemon`, `--prototype-next`, `--prototype-prev`, `--prototype-stop`; client modes report connection failures and exit without starting the normal switcher. Bind `cosmic-ext-app-switcher-prototype.sock` under `XDG_RUNTIME_DIR` (same fallback as normal) under a separate `.lock`; reuse the existing lock and cleanup rules. Preserve `next`, `prev`, `stop` wire strings.
- [ ] **Step 4: Verify GREEN.** Run `cargo test --offline -p cosmic-ext-app-switcher prototype::tests`; expected: all protocol tests pass. Run `cargo test --workspace --offline`; expected: all tests pass.
- [ ] **Step 5: Commit.** Stage the protocol files and commit `feat: add isolated prototype command channel`.

### Task 2: Persistent overlay lifecycle

**Files:** Modify `src/prototype.rs`, `src/app.rs`, `src/main.rs`; tests in `src/prototype.rs` and `src/app.rs`.

**Interfaces:** Consumes Task 1 commands. Produces `prototype::run_daemon(listener: std::os::unix::net::UnixListener) -> anyhow::Result<()>`. Model daemon states as Idle, Opening, and Visible; retain FIFO commands during Opening.

- [ ] **Step 1: Write failing lifecycle tests.** Assert idle `Next` opens with selection 1 and idle `Previous` opens with selection `n-1`; while Opening, successive commands are applied in FIFO order once the snapshot arrives; Visible commands cycle; activate and cancel return to Idle; a second open gets a fresh snapshot and reset modifier/timer state; empty snapshot and worker error return to Idle; stale worker/surface completion is ignored; a held modifier and the existing 100 ms quick-release settling semantics work on every open.
- [ ] **Step 2: Verify RED.** Run `cargo test --offline -p cosmic-ext-app-switcher prototype`; expected: lifecycle tests fail because the persistent state machine is not implemented.
- [ ] **Step 3: Implement lifecycle.** Initialize libcosmic with no main window and no layer surface; on Idle command asynchronously load theme/scope and call `wayland::spawn_wayland_thread`, holding its returned sender and worker for that open. Create a fresh layer surface, selection and visuals for its snapshot. Queue commands while Opening. On activation/cancel send exactly one worker command, destroy the surface, drop/join the worker without blocking the UI and return to Idle; ignore results for older open generations. Refresh on every open. Do not use `iced::exit()` except for Stop. If no windows or an error occurs, log and return to Idle. Preserve normal non-prototype behavior.
- [ ] **Step 4: Verify GREEN.** Run `cargo test --workspace --offline`; expected: all tests pass, including FIFO, failures, lifecycle reset and existing tests.
- [ ] **Step 5: Commit.** Stage affected files and commit `feat: keep prototype switcher process warm`.

### Task 3: Benchmark and live validation

**Files:** Modify `src/prototype.rs`; create `docs/persistent-switcher-benchmark.md`.

**Interfaces:** Consumes Task 2 daemon. Prototype client includes a `CLOCK_MONOTONIC` request timestamp with each open command; daemon logs matching request-to-focus elapsed milliseconds and process identity. Log `/proc/self/status` RSS and peak RSS, and `/proc/self/smaps_rollup` PSS when readable, at idle, focus, and after close; unavailable values are labelled unavailable.

- [ ] **Step 1: Write failing tests.** Assert timestamp encoding/decoding round-trips; memory parsers read known RSS/HWM/PSS values and handle absent or inaccessible PSS without failing the daemon; request identifiers correlate open and focus samples.
- [ ] **Step 2: Verify RED.** Run `cargo test --offline -p cosmic-ext-app-switcher prototype`; expected: measurement tests fail until implemented.
- [ ] **Step 3: Implement measurement.** Use monotonic timestamps shared across client and daemon, log request-to-focus rather than process-start-to-focus, and collect memory at startup idle, focus, and after each close. Log no titles or app IDs. Provide manual commands for starting/stopping the daemon and running a shortcut client without changing installed configuration.
- [ ] **Step 4: Verify GREEN and build.** Run `cargo test --workspace --offline` and `cargo build --release --offline -p cosmic-ext-app-switcher`; expected: all tests and build pass.
- [ ] **Step 5: Measure in COSMIC.** With the user's live session, record at least ten warm shortcut invocations, median and range request-to-focus, idle/active RSS and PSS, peak RSS, and idle memory after repeated cycles. Record the number of windows, measurement method, errors, and compare cautiously with the prior 292 ms / 19 MiB baseline, whose start boundary differs. If live interaction is unavailable, document it as outstanding rather than inventing data.
- [ ] **Step 6: Commit.** Stage affected files and commit `perf: measure persistent switcher prototype`.

## Final verification

- Run `cargo test --workspace --offline`, `cargo build --release --offline -p cosmic-ext-app-switcher`, and `git diff --check`; all must pass.
- Review the full branch for the five Review Focus cases, socket isolation, cleanup, and baseline comparability. Report whether the prototype meets the experiment goal and all limitations; do not deploy it automatically.
