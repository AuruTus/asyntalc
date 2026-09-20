#!/usr/bin/env bash
# Run from the repository root after `cargo build --locked`.
# Requires Bash and Python 3. Uses only the fake runner; retains evidence in /tmp.
set -euo pipefail
demo_bin="${1:-./target/debug/asyntalc}"
demo_dir="${ASYNTALC_DEMO_DIR:-}"
if [[ -z "$demo_dir" ]]; then
  demo_dir="$(mktemp -d /tmp/asyntalc-inspect.XXXXXX)"
fi
demo_pid=""
cleanup() {
  if [[ -n "$demo_pid" ]]; then
    kill "$demo_pid" 2>/dev/null || true
    wait "$demo_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT
"$demo_bin" --data-dir "$demo_dir" daemon --runner fake --fake-delay-ms 50 \
  >"$demo_dir/daemon.stdout" 2>"$demo_dir/daemon.stderr" &
demo_pid=$!
ready=false
for _ in {1..100}; do
  if "$demo_bin" --data-dir "$demo_dir" ping >"$demo_dir/ping.json" 2>/dev/null; then
    ready=true
    break
  fi
  if ! kill -0 "$demo_pid" 2>/dev/null; then
    cat "$demo_dir/daemon.stderr" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ "$ready" != true ]]; then
  echo 'Daemon did not become ready' >&2
  exit 1
fi
printf 'Inspect the durable run' >"$demo_dir/prompt.txt"
"$demo_bin" --data-dir "$demo_dir" submit --session demo --input "$demo_dir/prompt.txt" \
  --idempotency-key first >"$demo_dir/receipt.json"
"$demo_bin" --data-dir "$demo_dir" submit --session demo --input "$demo_dir/prompt.txt" \
  --idempotency-key first >"$demo_dir/retry.json"
"$demo_bin" --data-dir "$demo_dir" submit --session demo --input "$demo_dir/prompt.txt" \
  --idempotency-key second >"$demo_dir/second.json"
# Rediscover handles without relying on the submit receipt.
"$demo_bin" --data-dir "$demo_dir" list --session demo --limit 1 >"$demo_dir/page1.json"
read -r demo_run demo_cursor < <(python3 - "$demo_dir/page1.json" <<'PY'
import json, sys
page = json.load(open(sys.argv[1]))
print(page['runs'][0]['run_id'], page['next_after'])
PY
)
"$demo_bin" --data-dir "$demo_dir" list --session demo --after "$demo_cursor" --limit 1 >"$demo_dir/page2.json"
"$demo_bin" --data-dir "$demo_dir" wait --run "$demo_run" --timeout-ms 5000 >"$demo_dir/wait.json"
"$demo_bin" --data-dir "$demo_dir" result --run "$demo_run" --output text >"$demo_dir/result.txt"
"$demo_bin" --data-dir "$demo_dir" logs --run "$demo_run" --limit 1 >"$demo_dir/log1.json"
"$demo_bin" --data-dir "$demo_dir" logs --run "$demo_run" --after-seq 1 >"$demo_dir/log2.json"
python3 - "$demo_dir" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
def read(name):
    return json.loads((root / name).read_text())
first, second = read('page1.json'), read('page2.json')
assert first['has_more'] and not second['has_more']
assert first['runs'][0]['run_id'] == read('receipt.json')['run_id'] == read('retry.json')['run_id']
assert second['runs'][0]['run_id'] == read('second.json')['run_id']
assert 'input' not in first['runs'][0]
assert read('wait.json')['status'] == 'completed'
assert (root / 'result.txt').read_text() == '[fake] Inspect the durable run'
events = read('log1.json')['events'] + read('log2.json')['events']
assert [e['kind'] for e in events] == ['run.submitted', 'run.started', 'run.completed']
assert [e['sequence'] for e in events] == [1, 2, 3]
print(f'Inspection demo passed. JSON evidence and SQLite: {root}')
PY
