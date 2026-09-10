#!/usr/bin/env bash
# Profile rustgres under valgrind (callgrind + dhat) while driving it with
# benches/bench.py. Server source is never modified; this only observes.
#
# Usage: ./benches/profile.sh [--seconds N] [--workload NAME]
# Outputs land in benches/profiles/:
#   callgrind.out.<pid>   instruction-level profile (callgrind_annotate)
#   dhat.out.<pid>        heap profile (dh_view.html)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/benches/profiles"
SECONDS_PER_WORKLOAD=5
WORKLOAD="all"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --seconds)  SECONDS_PER_WORKLOAD="$2"; shift 2 ;;
    --workload) WORKLOAD="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

command -v valgrind >/dev/null || { echo "valgrind not installed" >&2; exit 1; }
command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found" >&2; exit 1; }

cd "$ROOT"
cargo build
mkdir -p "$OUT"

wait_for_port() {
  for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/5433) 2>/dev/null; then
      exec 3>&- 3<&-
      return 0
    fi
    sleep 0.1
  done
  echo "server did not open 127.0.0.1:5433" >&2
  return 1
}

port_in_use() {
  (exec 3<>/dev/tcp/127.0.0.1/5433) 2>/dev/null
}

# Guard: the benchmark must drive OUR server instance, never someone else's.
# If 5433 is already taken, abort instead of silently profiling the void
# while bench.py talks to a stranger.
if port_in_use; then
  echo "ERROR: 127.0.0.1:5433 is already in use." >&2
  echo "Stop the other rustgres instance first, then re-run." >&2
  exit 1
fi

run_under() { # $1 = tool, $2.. = extra valgrind args
  local tool="$1"; shift
  echo "== valgrind --tool=$tool =="
  # Fresh data dir per profiling run: v0.4 persists to ./rustgres-data by
  # default, and profiling must not reuse stale benchmark state (a huge
  # old WAL would dominate replay/startup and skew the profile).
  local datadir
  datadir="$(mktemp -d /tmp/rgprof_XXXXXX)"
  RUSTGRES_DATA_DIR="$datadir" \
    valgrind --tool="$tool" "$@" ./target/debug/rustgres &
  local pid=$!
  if ! wait_for_port; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$datadir"
    return 1
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "ERROR: our server (pid $pid) died; port 5433 must belong to " >&2
    echo "another instance. Aborting rather than benchmarking a stranger." >&2
    rm -rf "$datadir"
    return 1
  fi
  python3 benches/bench.py --seconds "$SECONDS_PER_WORKLOAD" \
      --workload "$WORKLOAD" || true
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -rf "$datadir"
  echo "== $tool done =="
}

run_under callgrind \
  --callgrind-out-file="$OUT/callgrind.out.%p" \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes

run_under dhat \
  --dhat-out-file="$OUT/dhat.out.%p"

echo
echo "profiles saved in $OUT:"
ls -la "$OUT"
echo "see $OUT/README.md for how to read them"
