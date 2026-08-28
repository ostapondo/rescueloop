# RescueLoop

**RescueLoop watches your computer for errors and offers a fix.**

The idea is simple: catch errors anywhere in the system, explain what happened, and offer a solution.
RescueLoop runs in the background and watches for problems. This includes app crashes, failed commands,
stopped services, resource problems, and unhealthy containers.

It does not support every possible error yet. The goal is to keep adding new sources until one tool can
show you problems from across the whole system.

When RescueLoop finds a problem:

1. It tells you **what broke**.
2. It collects the information that can explain **why it broke**.
3. It offers a solution and waits for **your approval**.
4. It applies the fix and checks that the problem is **actually gone**.
5. If the fix does not work, it puts things back when it can.

AI cannot run random commands through RescueLoop. Your problem history stays on your machine. Nothing
is sent to an AI unless you ask for analysis.

**macOS and Windows today. Linux next.**

> RescueLoop is early software. It already handles real incidents, but coverage is still growing.

![RescueLoop incident console](assets/rescueloop-console.png)

## A simple example

Imagine that a local database stops starting after its configuration file changes.

Without RescueLoop, you search through logs, guess what changed, edit files, and hope the database starts.

With RescueLoop, you see the failed start, the configuration change, and a suggested fix in one place.
You approve the fix. RescueLoop then starts the database again to check that it works.

## How it works

1. **Notice:** See that something stopped working.
2. **Explain:** Collect only the clues related to that problem.
3. **Ask:** Show you the proposed fix and wait for permission.
4. **Fix:** Make one known, limited change.
5. **Check:** Try the failed action again and undo the repair if it did not help.

## What works now

- Native crash detection on macOS and Windows
- Failed command and process supervision
- Windows Event Log and macOS Unified Log
- Docker and Podman failures, OOM events, and restart loops
- Local incident history with repeated failures grouped together
- Analysis through Codex CLI, Claude Code, or an HTTP adapter
- Approved repairs for files, JSON config, permissions, services, and containers
- Read-only local MCP access to redacted incidents

## Try it

Clone the repository and start RescueLoop:

```sh
git clone https://github.com/ostapondo/rescueloop.git
cd rescueloop
cargo run -p rescueloop
```

Or install the published CLI package with npm:

```sh
npm install --global rescueloop
rescueloop
```

The first run starts the background watcher and opens the interactive console. Closing the console
leaves detection running in the background. On macOS this uses LaunchAgent; on Windows it uses Task
Scheduler.

The first run also detects supported AI agents and walks you through setup automatically.

## CLI reference

For foreground diagnostics, `rescueloop watch` runs the detector in the current terminal without
opening the console. The advanced `rescueloop service ...` commands remain available for explicit
per-user or system-level service installation.

The main commands, close to the actual CLI output:

| Command | What it does |
| --- | --- |
| `rescueloop` | Ensure the watcher is running and open the interactive incident console |
| `rescueloop start` | Explicitly start the watcher and open the console |
| `rescueloop stop` | Stop the watcher but keep its background registration |
| `rescueloop status` | Show whether the watcher is installed and running |
| `rescueloop doctor` | Explain watcher, event-source, queue, journal, storage, index, ledger, and log-writer health |
| `rescueloop restart` | Restart the registered watcher |
| `rescueloop uninstall` | Stop the watcher and remove its background registration |
| `rescueloop watch` | Run the detector in the current terminal |
| `rescueloop run --record-args <cmd> [args...]` | Supervise a command and save an incident when it fails |
| `rescueloop service install` | Install the background watcher service |
| `rescueloop service status` | Show the background service status |
| `rescueloop mcp` | Start the read-only local MCP server |
| `rescueloop --incident-dir <path> mcp` | Start MCP against a specific incident directory |

Examples:

```sh
# Open the interactive incident console
rescueloop

# Supervise a command (args recorded only with --record-args)
rescueloop run --record-args ./deploy.sh --env prod

# Background watcher lifecycle
rescueloop status
rescueloop doctor
rescueloop stop
rescueloop start

# MCP server (read-only incident access)
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

On Windows, use the same commands from PowerShell with the compiled binary:

```powershell
.\target\release\rescueloop.exe start
.\target\release\rescueloop.exe status
.\target\release\rescueloop.exe --incident-dir C:\absolute\path\.rescueloop\incidents mcp
```

`uninstall` removes only the watcher registration. It does not uninstall the `rescueloop`
executable or delete incident history.

## Self-health

`rescueloop doctor` reads a bounded local health snapshot and answers whether the watcher is
running, what each event source can currently see, and whether queueing or durable local state is
degraded. The TUI shows the same summary, refreshes it every two seconds, and opens the complete
component/source view with `V`.

The watcher publishes `watch-health-v1.json` atomically inside the private RescueLoop state
directory. It contains source names, counters, timestamps, queue occupancy, backoff, uptime, and a
bounded shutdown reason. An unclosed snapshot from a watcher that is no longer running is reported
as `abnormal_or_interrupted`. Exact duplicates and grouped recurrences are counted separately, and
log-writer failures come from the background watcher rather than the inspecting CLI process. The
snapshot never contains raw artifacts, launch arguments, incident evidence,
tokens, or private paths. The file is a disposable operational projection; incident JSON and the
lineage ledger remain the durable sources of truth.

No network listener, Prometheus endpoint, or telemetry export is enabled by this feature.

## Local metrics

RescueLoop keeps a typed, process-local metrics registry and includes the watcher's latest bounded
snapshot in `watch-health-v1.json`. `rescueloop doctor` and the TUI self-health view show these
values locally:

- `events_received_total{source}` and `events_dropped_total{reason}` use closed, bounded label sets;
- `source_reconnects_total`, `queue_depth`, `rollback_total`, `log_write_failures_total`,
  `index_rebuild_total`, and `journal_pending_count` are saturating counters or gauges;
- `incident_persist_duration`, `incident_grouping_duration`, `analysis_duration`,
  `repair_duration`, and `verification_duration` retain only count, total, maximum, and latest
  duration in microseconds. They do not retain incident IDs, paths, evidence, or arguments.

`index_rebuild_total` includes explicit rebuilds plus successful automatic recovery when the
projection is missing, stale, or corrupt. Failed rebuild attempts are not reported as successes.

Metrics reset with the process and are operational hints rather than durable audit history. Incident
JSON and the lineage ledger remain authoritative. Metric export is disabled: there is no metrics
socket, Prometheus endpoint, OTLP metrics client, background network task, or implicit environment
discovery. Any future exporter must be explicit opt-in, bounded, and redacted.

Use `a` to analyze an incident, `r` to review a repair, and `y` to approve it.

Arguments may contain secrets, so RescueLoop records them only when `--record-args` is present. They
are never included in evidence sent to an AI agent.

## Safety

RescueLoop can change a machine, so the boundary is deliberately narrow:

- Detection is local and automatic.
- Analysis and repair are explicit actions.
- AI output is untrusted data, not executable code.
- Repairs use known action types and must match collected evidence.
- Every change is shown before approval and checked afterwards.
- Supported file and configuration changes are backed up for rollback.
- MCP cannot repair, replay, run a shell, or read arbitrary files.

The MCP server is local and read-only. It exposes only `list_incidents` and `get_incident`:

```sh
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

See [SECURITY.md](SECURITY.md) for the full security boundary and vulnerability reporting.

## Where it is going

The goal is one local view of what broke, why it broke, and whether it was actually fixed.

Next steps include Linux, a desktop app, broader system and network signals, better correlation,
application-specific health checks, and more safe repair types.

See [ROADMAP.md](ROADMAP.md) for the longer version.

## Build and test

```sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Contributing

Bug reports, redacted failure samples, documentation, platform research, and code are welcome. Linux
support and new evidence sources are especially useful.

Start with [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE) © ostapondo
