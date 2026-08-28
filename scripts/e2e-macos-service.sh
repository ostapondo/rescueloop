#!/usr/bin/env bash
set -euo pipefail

task_root=$(mktemp -d)
task_home="$task_root/home"
incident_dir="$task_root/state/incidents"
binary="$(pwd)/target/debug/rescueloop"
plist="$task_home/Library/LaunchAgents/dev.rescueloop.agent.plist"
domain="gui/$(id -u)"

cleanup() {
  HOME="$task_home" "$binary" service uninstall >/dev/null 2>&1 || true
  rm -rf "$task_root"
}
trap cleanup EXIT INT TERM

mkdir -p "$task_home/Library/LaunchAgents"
cargo build --quiet -p rescueloop

wait_for_health() {
  expected="$1"
  for _ in $(seq 1 50); do
    if HOME="$task_home" "$binary" --incident-dir "$incident_dir" doctor --json \
      >"$task_root/doctor.json" 2>/dev/null &&
      jq -e --arg expected "$expected" '
        ((.checks[] | select(.name == "watcher") | .state) == $expected)
        and .queue_depth <= .queue_capacity
        and .received <= (.persisted + .grouped + .deduplicated + .journal_pending)
      ' "$task_root/doctor.json" >/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  cat "$task_root/doctor.json" >&2 2>/dev/null || true
  return 1
}

HOME="$task_home" "$binary" --incident-dir "$incident_dir" service install
wait_for_health healthy
first_pid=$(jq -r '.pid' "$task_root/state/watch-health-v1.json")

HOME="$task_home" "$binary" stop
wait_for_health degraded
HOME="$task_home" "$binary" restart
for _ in $(seq 1 50); do
  second_pid=$(jq -r '.pid' "$task_root/state/watch-health-v1.json" 2>/dev/null || true)
  test -n "$second_pid" && test "$first_pid" != "$second_pid" && break
  sleep 0.2
done
test -n "${second_pid:-}" && test "$first_pid" != "$second_pid"
wait_for_health healthy

# A bootout/bootstrap pair models the per-user registration reload performed at login.
launchctl bootout "$domain" "$plist"
wait_for_health degraded
launchctl bootstrap "$domain" "$plist"
wait_for_health healthy

HOME="$task_home" "$binary" stop
cp "$plist" "$task_root/valid.plist"
printf '%s\n' '<plist><dict><broken>' >"$plist"
if HOME="$task_home" "$binary" start >/dev/null 2>&1; then
  echo "corrupted LaunchAgent definition unexpectedly started" >&2
  exit 1
fi
cp "$task_root/valid.plist" "$plist"
HOME="$task_home" "$binary" --incident-dir "$incident_dir" service install
wait_for_health healthy

HOME="$task_home" "$binary" --incident-dir "$incident_dir" run /usr/bin/false >/dev/null
test "$(find "$incident_dir" -name '*.json' -type f | wc -l | tr -d ' ')" -eq 1
HOME="$task_home" "$binary" --incident-dir "$incident_dir" doctor --json >"$task_root/final-doctor.json"
jq -e '
  ((.checks[] | select(.name == "incident store") | .state) == "healthy")
  and ((.checks[] | select(.name == "lineage ledger") | .state) == "healthy")
  and .queue_depth <= .queue_capacity
  and .received <= (.persisted + .grouped + .deduplicated + .journal_pending)
' "$task_root/final-doctor.json" >/dev/null

echo "macOS native service E2E passed."
