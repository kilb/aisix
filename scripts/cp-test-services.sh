#!/usr/bin/env bash
# A throwaway Postgres cluster and one kine process for the control-plane
# tests, on loopback only. Not for anything else: the cluster trusts every
# connection and runs without fsync.
#
# Usage: scripts/cp-test-services.sh start|stop|env
#
#   start  create the cluster (first time only) and start it, create database
#          kine_test, and start kine on it, fetching the pinned binary with
#          scripts/fetch-kine.sh unless CP_TEST_KINE_BIN names one
#   env    print the export lines the tests read:
#            eval "$(scripts/cp-test-services.sh env)"
#   stop   stop kine, then Postgres (the data directory is kept)
#
# Environment:
#   CP_TEST_DIR        state directory (default ${TMPDIR:-/tmp}/aisix-cp-test-$USER)
#   CP_TEST_PG_PORT    Postgres port (default 55432)
#   CP_TEST_KINE_PORT  kine's etcd port (default 52379)
#   CP_TEST_KINE_BIN   a kine binary to run instead of fetching one
#
# Pass the same environment to every subcommand: `env` and `stop` derive the
# ports and paths from it the way `start` did.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." >/dev/null && pwd)"
DIR="${CP_TEST_DIR:-${TMPDIR:-/tmp}/aisix-cp-test-${USER:-$(id -un)}}"
PG_PORT="${CP_TEST_PG_PORT:-55432}"
KINE_PORT="${CP_TEST_KINE_PORT:-52379}"
KINE_BIN="${CP_TEST_KINE_BIN:-$DIR/kine}"
PG_URL="postgres://postgres@127.0.0.1:${PG_PORT}/postgres"

pg_bindir() {
  local bindir
  bindir="$(pg_config --bindir 2>/dev/null || true)"
  if [ -n "$bindir" ] && [ -x "$bindir/initdb" ]; then
    echo "$bindir"
  else
    echo /usr/lib/postgresql/18/bin
  fi
}

pg_running() {
  [ -f "$DIR/data/PG_VERSION" ] && "$PG_BIN/pg_ctl" -D "$DIR/data" status >/dev/null 2>&1
}

# The pid file names a live kine process (not a recycled pid).
kine_running() {
  local pid
  [ -f "$DIR/kine.pid" ] || return 1
  pid="$(cat "$DIR/kine.pid")"
  kill -0 "$pid" 2>/dev/null || return 1
  tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q -- "--listen-address 127.0.0.1:${KINE_PORT}"
}

port_open() {
  (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

start() {
  mkdir -p "$DIR"
  if [ ! -f "$DIR/data/PG_VERSION" ]; then
    "$PG_BIN/initdb" -U postgres --auth=trust -D "$DIR/data" >"$DIR/initdb.log"
  fi
  if ! pg_running; then
    # No Unix socket: its path would have to fit in 107 bytes, and the tests
    # connect over TCP anyway.
    "$PG_BIN/pg_ctl" -D "$DIR/data" -l "$DIR/postgres.log" -w \
      -o "-p ${PG_PORT} -c listen_addresses=127.0.0.1 -c unix_socket_directories='' -c fsync=off -c synchronous_commit=off -c full_page_writes=off" \
      start >/dev/null
  fi
  if [ "$("$PG_BIN/psql" "$PG_URL" -tAc "SELECT 1 FROM pg_database WHERE datname = 'kine_test'")" != 1 ]; then
    "$PG_BIN/psql" "$PG_URL" -qc "CREATE DATABASE kine_test"
  fi
  if ! kine_running; then
    if [ -z "${CP_TEST_KINE_BIN:-}" ] && [ ! -x "$KINE_BIN" ]; then
      "$ROOT/scripts/fetch-kine.sh" "$KINE_BIN"
    fi
    [ -x "$KINE_BIN" ] || { echo "cp-test-services: $KINE_BIN is not executable" >&2; exit 1; }
    nohup "$KINE_BIN" \
      --endpoint "postgres://postgres@127.0.0.1:${PG_PORT}/kine_test?sslmode=disable" \
      --listen-address "127.0.0.1:${KINE_PORT}" \
      --metrics-bind-address 0 \
      >"$DIR/kine.log" 2>&1 &
    echo $! >"$DIR/kine.pid"
    for _ in $(seq 50); do
      port_open "$KINE_PORT" && break
      sleep 0.2
    done
    port_open "$KINE_PORT" || {
      echo "cp-test-services: kine never listened on 127.0.0.1:${KINE_PORT}; see $DIR/kine.log" >&2
      exit 1
    }
  fi
  echo "cp-test-services: Postgres on 127.0.0.1:${PG_PORT}, kine on 127.0.0.1:${KINE_PORT} (state in $DIR)" >&2
}

stop() {
  if kine_running; then
    local pid
    pid="$(cat "$DIR/kine.pid")"
    kill "$pid"
    for _ in $(seq 50); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.2
    done
  fi
  rm -f "$DIR/kine.pid"
  if pg_running; then
    "$PG_BIN/pg_ctl" -D "$DIR/data" -m fast -w stop >/dev/null
  fi
}

print_env() {
  printf 'export CP_TEST_DATABASE_URL=%q\n' "$PG_URL"
  printf 'export CP_TEST_KINE_ENDPOINT=%q\n' "http://127.0.0.1:${KINE_PORT}"
  printf 'export CP_TEST_KINE_BIN=%q\n' "$KINE_BIN"
  printf 'export CP_E2E_PG_URL=%q\n' "$PG_URL"
  printf 'export CP_E2E_KINE_BIN=%q\n' "$KINE_BIN"
}

PG_BIN="$(pg_bindir)"
case "${1:-}" in
  start) start ;;
  stop) stop ;;
  env) print_env ;;
  *)
    echo "usage: scripts/cp-test-services.sh start|stop|env" >&2
    exit 2
    ;;
esac
