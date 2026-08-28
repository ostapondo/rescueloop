#!/usr/bin/env bash
set -euo pipefail

task_root=$(mktemp -d)
incident_dir="$task_root/state/incidents"
binary="$(pwd)/target/debug/rescueloop"
trap 'chmod 700 "$task_root/state/observation-journal" 2>/dev/null || true; rm -rf "$task_root"' EXIT INT TERM

cargo build --quiet -p rescueloop
"$binary" --incident-dir "$incident_dir" run /usr/bin/false >/dev/null
baseline_incidents=$(find "$incident_dir" -name '*.json' -type f | wc -l | tr -d ' ')

journal="$task_root/state/observation-journal"
mkdir -p "$journal"
chmod 500 "$journal"
if "$binary" --incident-dir "$incident_dir" run /usr/bin/false >/dev/null 2>&1; then
  echo "permission-denied observation unexpectedly succeeded" >&2
  exit 1
fi
chmod 700 "$journal"

if RESCUELOOP_TEST_STORAGE_FAILURE=capacity \
  "$binary" --incident-dir "$incident_dir" run /usr/bin/false >/dev/null 2>&1; then
  echo "storage-capacity observation unexpectedly succeeded" >&2
  exit 1
fi

test "$(find "$incident_dir" -name '*.json' -type f | wc -l | tr -d ' ')" -eq "$baseline_incidents"
"$binary" --incident-dir "$incident_dir" doctor --json >"$task_root/doctor.json"
jq -e '
  ((.checks[] | select(.name == "incident store") | .state) == "healthy")
  and ((.checks[] | select(.name == "lineage ledger") | .state) == "healthy")
  and .queue_depth <= .queue_capacity
  and .received <= (.persisted + .grouped + .deduplicated + .journal_pending)
' "$task_root/doctor.json" >/dev/null

echo "Storage failure durability validation passed."
