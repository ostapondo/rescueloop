# Contributing to RescueLoop

Thanks for taking a look. RescueLoop is early, and useful contributions do not have to be large.
Bug reports, documentation fixes, platform research, redacted failure samples, and code are all
welcome. Linux support and new evidence sources are especially useful areas to help with.

## Before you start

For a small fix, open a pull request when it is ready. For a new feature or a change to the safety
model, open an issue first so the design can be discussed before a lot of work is done.

Please do not post secrets, usernames, private paths, tokens, raw crash reports, or other personal
data. Reduce a failure sample to the smallest safe fixture you can share.

Security problems belong in a private report, not a public issue. See [SECURITY.md](SECURITY.md).

## Development

Install a current stable Rust toolchain, then run:

```sh
cargo build --workspace
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Changes to MCP must also cover protocol initialization, tool discovery, invalid input, redaction,
and the absence of mutation tools:

```sh
./scripts/validate-mcp.sh
```

Platform-specific and end-to-end checks live in the `scripts/` directory and run in CI.

The native lifecycle gates assert both command outcomes and the bounded `doctor --json` health
snapshot. They also enforce `received <= persisted + grouped + deduplicated + journal_pending`, so
an accepted observation cannot disappear behind a green lifecycle exit code.

Running `cargo run -p rescueloop` uses the normal combined startup flow: it registers or starts the
per-user watcher and opens the TUI. Use `cargo run -p rescueloop -- watch` when you only want a
foreground watcher during development. Clean up a development registration with:

```sh
cargo run -p rescueloop -- stop
cargo run -p rescueloop -- uninstall
```

Lifecycle changes must be checked on the affected native platform because macOS LaunchAgent and
Windows Task Scheduler behavior cannot be fully validated by cross-platform unit tests.

Self-health changes must keep `watch-health-v1.json` bounded, versioned, atomically replaced, and
free of paths, arguments, evidence, and secrets. `rescueloop doctor` is local-only; exposing health
through MCP or a network metrics endpoint requires a separate compatibility and threat-model review.

### Windows

The same `cargo` commands work on Windows (Rust toolchain via
[rustup](https://rustup.rs/)). Script-specific equivalents:

| Purpose | Unix/macOS | Windows (PowerShell) |
| --- | --- | --- |
| MCP validation | `./scripts/validate-mcp.sh` | `powershell -ExecutionPolicy Bypass -File scripts/validate-mcp.ps1` |
| Native Windows E2E | — | `powershell -ExecutionPolicy Bypass -File scripts/e2e-windows.ps1` |
| Native macOS service E2E | `./scripts/e2e-macos-service.sh` | — |
| Persistence interruption | `./scripts/validate-observation-recovery.sh` | Included in `e2e-windows.ps1` |
| Storage failures | `./scripts/validate-storage-failures.sh` | Included in `e2e-windows.ps1` |
| Installation | `./scripts/install.sh` | `powershell -ExecutionPolicy Bypass -File scripts/install.ps1` |
| Idle benchmark | `./scripts/benchmark-idle.sh` | `powershell -ExecutionPolicy Bypass -File scripts/benchmark-idle-windows.ps1` |

`e2e-macos-service.sh` covers LaunchAgent bootstrap, bootout, restart, login-style reload, malformed
plist recovery, disposable-index rebuild, and package replacement while the watcher is active.
`e2e-windows.ps1` covers Task Scheduler install, `/Run`, `/End`, concurrent starts, logon-trigger
registration, locale-independent state inspection, persistence interruption, index rebuild, and
package replacement. Hosted CI cannot reboot its runner mid-job; the ONLOGON registration plus a
fresh `/Run` is the bounded, repeatable reboot/logon contract exercised on every Windows run.

## Pull requests

Keep a pull request focused on one problem. Explain what changed, why it changed, and how you tested
it. Tests are expected for behavior changes. If a test is not practical, say why.

Every user-visible change to incidents, evidence, analysis, repair plans, lifecycle state, or history
must assess the MCP surface. Update its schemas, redaction, documentation, and tests when agents
should see the feature. Otherwise, state why no MCP change is needed.

By contributing, you agree that your contribution is licensed under the MIT License.
