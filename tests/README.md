# Hyprsession tests

## Offline checks

Run with a current Rust toolchain and the locked dependencies, inside an isolated environment:

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked --offline
cargo test --all-targets --locked --offline -- --test-threads=1
cargo clippy --all-targets --locked --offline
```

The Rust tests cover snapshot serialization and compatibility, restore planning and placement, workspace addressing, terminal command non-replay, incomplete-restore protection, daemon locking, and final-save/freeze/logout. CLI tests execute the real binary against fake IPC sockets and private snapshots, not a real compositor.

## Isolation

Use a disposable container or namespaces with a fresh PID namespace and `/proc`, no network, private `/tmp` and `/run`, and no real Wayland/X11/DBus sockets. Start with an empty environment and private `HOME`, XDG directories, `TMPDIR` and `HYPRSESSION_PATH`; supply only the toolchain and dependency cache needed for offline builds. Bound the entire test process with an external timeout.

`HYPRSESSION_PATH` alone does not isolate compositor IPC, desktop-entry discovery, process inspection or logout commands. Do not run destructive tests against the working desktop. Unit-test fixtures must not depend on installed GUI applications or overwrite user launchers.

## Compositor smoke checks

Use a separate nested Hyprland with private XDG/DBus state. Noctalia needs its own short XDG runtime directory and Wayland/Hyprland socket links to avoid connecting to the main shell. Never run `clear`, `load` or `logout` against the main session.

The obsolete inherited NixOS VM harness is not shipped; offline Rust tests do not establish hardware, live-desktop or full session-restart behavior.
