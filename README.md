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
| `rescueloop diagnostics export` | Preview a bounded, redacted local support bundle; add `--confirm` to write it |
| `rescueloop timeline <incident.json> [--json]` | Show the bounded, hash-linked lifecycle timeline for one incident |
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
rescueloop timeline .rescueloop/incidents/<incident-id>.json
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

### Local SLO assertions

The doctor and TUI self-health views evaluate eight local guarantees as `PASS`, `FAIL`, or
`UNKNOWN`: accepted-observation durability, independent event-source workers, bounded queue
occupancy, the 30-second graceful-shutdown deadline, SQLite projection rebuildability from
incident JSON, verification outcome integrity, ledger coverage for terminal repair receipts, and
the built-in redaction negative probes. `UNKNOWN` is used when an older or inactive watcher has not
published enough evidence; it is never silently treated as success.

These are local safety assertions rather than a remote uptime promise. The watcher journals an
observation before accepting it into the bounded queue, isolates sources in cancellable tasks, and
records the measured duration of each shutdown. Doctor rebuilds a temporary SQLite projection from
the authoritative JSON and audits the bounded local ledger and repair receipts. Terminal repair
receipts are published only after their ledger lineage is durable. Redaction probes use synthetic
sentinels and never read incident evidence or user secrets.

The assertions remain local and add no telemetry, network listener, or MCP tool. They expose
privileged implementation health in the CLI/TUI only; the existing read-only MCP incident surface
is unchanged.

## Diagnostic bundles

`rescueloop diagnostics export` first prints an exact preview of the fixed archive members, their
sizes, the number of included recent log records, the enforced bounds, and excluded private data.
Preview mode never writes a file. After review, repeat with `--confirm`; optionally select a new
destination with `--output <file.tar.gz>`. Existing files are never overwritten.

The gzip-compressed tar archive contains version/platform metadata, the bounded doctor health
snapshot, typed local metrics, event-source status, JSON/index/ledger integrity results, local SLO
assertions, allowlisted configuration, and up to 200 recent structured log records. Log input,
individual members, rendered logs, and the final archive all have explicit size bounds. Logs pass
through a second support-export redaction step.

Incident evidence, filesystem paths, launch arguments, tokens, secrets, model payloads, repair
contents, and configuration values that identify local resources are excluded. Configuration only
reports enabled source names and whether optional provider or OTLP features are configured; it does
not include endpoints or credentials. Bundles remain local and are never uploaded automatically.
The feature adds no MCP tool because support export is a privileged local filesystem operation.

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

The MCP server is local and read-only. It exposes `list_incidents`, `get_incident`, and
`get_incident_timeline`:

```sh
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

See [SECURITY.md](SECURITY.md) for the full security boundary and vulnerability reporting.

## Incident timeline

Each newly observed incident gets one durable lifecycle view assembled from the versioned incident
JSON and append-only hash-chained ledger. It follows the operation from `observed`, `normalized`, and
`persisted` through optional grouping and explicit analysis, approval, repair, verification, and the
terminal `committed` or `rolled_back` result. Every event includes a timestamp, per-operation
correlation ID, component, transition and outcome, bounded explanation, ledger entry ID, and—when
applicable—a bounded delay or refusal reason.

Press `T` in the console to open the selected incident's timeline, use the `timeline` command for
text or JSON output, or call the read-only MCP timeline tool. Timeline output is capped at 256 events
while retaining the origin events and latest activity. It contains no raw artifacts, launch
arguments, working directories, provider errors, or other private evidence. MCP remains additive,
local-stdio-only, read-only, bounded, and redacted; it cannot approve or execute repairs.

## Correlation and tracing

RescueLoop assigns typed local UUIDs to every lifecycle scope: `observation_id`, `incident_id`,
`occurrence_id`, `analysis_id`, `plan_id`, `repair_transaction_id`, and `verification_id`.
Observation, incident, and occurrence IDs are stored with incident and journal data; analysis and
plan IDs are stored in the validated analysis document; repair and verification IDs are stored with
the typed transaction or receipt. The hash-chained timeline links the applicable IDs at every stage,
and structured operational logs use the same identifiers. Older documents remain readable through
stable incident-scope fallbacks and optional additive fields. Analysis request schema version 3 adds
the locally generated `analysis_id`; IDs returned by a model are never trusted and are replaced
locally.

Tracing is off by default. To explicitly export a small allowlist of lifecycle spans over OTLP/HTTP,
set `RESCUELOOP_OTLP_TRACES_ENDPOINT` to an `http://` or `https://` collector `/v1/traces`
endpoint. The exporter has a 1,024-span queue, batches at most 128 spans, waits at most 10 seconds
per export, and shuts down with the process. The endpoint is length-bounded and cannot contain URL
credentials. RescueLoop does not export tracing events or arbitrary application spans. Exported
attributes contain only opaque lifecycle IDs, bounded enum-like stage metadata, and provider names;
raw paths, command arguments, evidence, error text, and model payloads are excluded. This setting
does not enable metrics export, a Prometheus listener, or any inbound network service.

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
