# Persistent switcher prototype benchmark

The prototype is opt-in and uses its own socket, lock, and application ID. No installed shortcut configuration changes are needed. Run these commands from the checkout in a live COSMIC Wayland session:

```sh
cargo build --release --offline -p cosmic-ext-app-switcher
RUST_LOG=info ./target/release/cosmic-ext-app-switcher --prototype-daemon 2>&1 | tee /tmp/persistent-switcher-benchmark.log
```

In another terminal, invoke the shortcut client, or temporarily bind these exact commands using the desktop's shortcut UI if desired (do not replace the installed binding):

```sh
./target/release/cosmic-ext-app-switcher --prototype-next
./target/release/cosmic-ext-app-switcher --prototype-prev
```

Hold Alt when opening if exercising Alt-release activation. Exercise next/previous while open, release Alt to activate, and Escape to cancel. Wait for close between measured opens. Include one initial open separately, then at least ten warm opens with an unchanged window population. Also check focus-loss dismissal and repeated reopen cycles. Stop the daemon explicitly:

```sh
./target/release/cosmic-ext-app-switcher --prototype-stop
```

The client reads `CLOCK_MONOTONIC` before connecting to the socket and transmits its PID and nanoseconds. The request ID is `client_pid:timestamp_ns`; matching `prototype open` and `prototype focus` entries identify one opening. Subsequent cycling commands do not replace the opening sample. The daemon timestamps the first matching surface `Focused` event with the same clock. `request_to_focus_ms` includes connect, up to one 10 ms daemon polling interval, snapshot work, surface creation, and focus delivery. It excludes client process startup before its timestamp and physical shortcut handling before client execution. It measures receipt of the toolkit focus event, not the first painted frame. Missing focus events produce no latency sample: count and report unmatched open IDs as errors, rather than discarding them silently.

Logs include daemon PID, snapshot window count, and memory in KiB at startup idle, focus, and after close. RSS and lifetime peak RSS come from `/proc/self/status`; PSS comes from `/proc/self/smaps_rollup`. Missing, inaccessible, or malformed measurements are labelled `unavailable` and do not stop the daemon. After-close samples occur immediately after overlay removal and before detached worker joins necessarily finish; let the process settle and inspect `/proc/<daemon-pid>/status` and `smaps_rollup` separately for settled idle memory. Peak RSS is a lifetime high-water mark. Logs do not include window titles or app IDs.

Record the COSMIC version, hardware, build revision, window count, number of measured opens, unmatched IDs, errors, the median and minimum–maximum latency, startup idle RSS/PSS, active RSS/PSS, peak RSS, and settled idle memory after repeated cycles. Compare results cautiously with the earlier **292 ms / 19 MiB** baseline: its start boundary was process launch, whereas this metric begins inside an already launched shortcut client. A direct speedup ratio would be misleading without remeasuring both paths with equal boundaries. Memory must likewise use the same units, window population, and sampling points.

## Results and outstanding validation

No live COSMIC interaction was available during implementation. The ten warm invocations, median/range, window count, active and settled idle memory, and compositor interaction checks remain outstanding. No experimental latency or memory values have been invented. Automated tests validate protocol timestamps, correlation, unavailable memory values, lifecycle handling, socket isolation, and cleanup; they do not establish perceived latency or the experiment's memory goal. Do not deploy this prototype automatically.
