#!/usr/bin/env bash
# runner.sh — m3060-side gate runner for the succ-loop harness.
# Deployed to /home/m3060/loop/runner.sh by gate.sh; the ONLY thing the .39
# driver executes remotely. Subcommands:
#   env                     sanity probe (worktree, symlinks, mem, lock)
#   unit <filter>           offline cargo tests on the rsynced (dirty) tree
#   record <sha> <battery>  gate-of-record on a CLEAN detached checkout of <sha>
#                           battery: smoke | g1 | g1g2 | final | baseline-warnings
# Exit codes: 0 pass/green, 1 red, 70 harness error, 71 lock/mem, 75 inconclusive.
# Never touches: the live validator, /mnt/xrpl-data, ~/xrpl-3.2.0-build/target.
set -u
LOOP=/home/m3060/loop
WT=$LOOP/wt
CARGO=/home/m3060/.cargo/bin/cargo
export CARGO_TARGET_DIR=$LOOP/target
LOCK=$LOOP/build.lock
PIDF=$LOOP/run.pid
NICE="nice -n 15 ionice -c3"
TS=$(date +%Y%m%d-%H%M%S)

say() { echo "[runner] $*"; }

mem_gb() { awk '/MemAvailable/{printf "%d", $2/1024/1024}' /proc/meminfo; }

# Kill a stale cargo group from a previous run that lost its ssh parent.
# Scoped: only if the recorded PGID's cmdline mentions our loop dir.
reap_stale() {
  [ -f "$PIDF" ] || return 0
  local pid; pid=$(cat "$PIDF" 2>/dev/null) || return 0
  [ -n "$pid" ] && [ -d "/proc/$pid" ] || { rm -f "$PIDF"; return 0; }
  if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "/home/m3060/loop"; then
    say "killing stale loop build group $pid"
    kill -9 -- "-$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null
  fi
  rm -f "$PIDF"
}

take_lock() {
  exec 9>"$LOCK"
  flock -w 900 9 || { say "build.lock held >900s"; exit 71; }
}

require_mem() {
  # Optional $1 = minimum GB (default 8). The probe/diff paths pass 4 and
  # drop to -j3 builds: the validator's hydrated mirror pins MemAvailable
  # ~7.5GB, and the flat 8GB gate starved every probe under it (2026-08-31).
  local need=${1:-8}
  local tries=0
  while [ "$(mem_gb)" -lt "$need" ]; do
    tries=$((tries+1))
    [ "$tries" -gt 3 ] && { say "MemAvailable <${need}GB after 3 waits"; exit 71; }
    say "MemAvailable $(mem_gb)GB <${need}GB — waiting 300s ($tries/3)"
    sleep 300
  done
}

# probe_jobs — -j6 wants ~8GB headroom; -j3 peaks ~3GB (measured 2026-08-31,
# manual bypass in the triage-law memo, now the built-in fallback).
probe_jobs() { [ "$(mem_gb)" -lt 8 ] && echo 3 || echo 6; }

# run_cargo <log> <timeout_s> <cmd...>  — setsid group, PGID recorded for scoped kills
run_cargo() {
  local log=$1 tmo=$2; shift 2
  setsid timeout "$tmo" $NICE "$@" >>"$log" 2>&1 &
  local pid=$!
  echo "$pid" > "$PIDF"
  wait "$pid"; local rc=$?
  rm -f "$PIDF"
  return $rc
}

# ---- metric extraction ------------------------------------------------------
last_num() { grep -oE "$1" "$2" | tail -1 | grep -oE '[0-9]+' | tail -1; }
test_summary_pass() {  # 0 args: log file — pass iff a "test result: ok." line exists and no FAILED
  grep -q "^test result: FAILED" "$1" && return 1
  grep -q "^test result: ok\." "$1" && return 0
  return 1
}
net_inconclusive() {   # log file — environmental failure signatures
  grep -qE "EXHAUSTED|rippled_overloaded|Server is overloaded|send_err:|error sending request|operation timed out|connection error|dns error|fetch_mainnet_amendments" "$1" && return 0
  # ran but applied nothing = amendments/pre-state never came up
  local att; att=$(last_num 'attempted:\s+[0-9]+' "$1" || true)
  [ -n "${att:-}" ] && [ "$att" = "0" ] && return 0
  return 1
}

# ---- subcommands ------------------------------------------------------------
cmd_env() {
  local ok=1
  [ -d "$WT/.git" ] || [ -f "$WT/.git" ] || { say "worktree missing"; ok=0; }
  [ -e "$WT/ffi/build/libxrpl_shim.a" ] || { say "ffi/build symlink broken"; ok=0; }
  [ -d "$WT/ffi/vendor" ] || { say "ffi/vendor symlink broken"; ok=0; }
  [ -f "$LOOP/XRPL Rust SDK/crates/xrpl-core/Cargo.toml" ] || { say "XRPL Rust SDK symlink broken"; ok=0; }
  [ -x "$CARGO" ] || { say "cargo missing"; ok=0; }
  local lock_free=1
  exec 9>"$LOCK"; flock -n 9 && flock -u 9 || lock_free=0
  echo "@@RESULT class=env ok=$ok mem_gb=$(mem_gb) lock_free=$lock_free wt_head=$(git -C "$WT" rev-parse --short HEAD 2>/dev/null || echo none)"
  [ "$ok" = "1" ] || exit 70
}

cmd_unit() {
  local filter=${1:-}
  reap_stale; take_lock; require_mem
  local log=$LOOP/logs/unit-$TS.log
  say "unit tests (filter='$filter') on rsynced tree — log $log"
  cd "$WT/crates/xrpl-node" || exit 70
  run_cargo "$log" 600 "$CARGO" test --features ffi -j6 $filter
  local rc=$?
  tail -40 "$log"
  echo "@@RESULT class=$([ $rc -eq 0 ] && echo green || echo red) mode=unit rc=$rc log=$log"
  exit $rc
}

clean_checkout() {
  local sha=$1
  # reset --hard FIRST: the worktree is routinely dirty from unit-mode rsyncs,
  # and `checkout --detach` refuses over local modifications (2026-07-16
  # HARNESS_ERROR). reset moves the detached HEAD and overwrites in one step.
  git -C "$WT" reset --hard "$sha" >/dev/null 2>&1 || { say "reset --hard $sha failed"; return 1; }
  git -C "$WT" clean -fd >/dev/null || return 1
  [ -z "$(git -C "$WT" status --porcelain)" ] || { say "worktree not clean after checkout"; return 1; }
  [ "$(git -C "$WT" rev-parse HEAD)" = "$(git -C "$WT" rev-parse "$sha^{commit}")" ] || { say "HEAD != $sha"; return 1; }
  # symlinks must have survived (gitignored via worktree-local exclude)
  [ -e "$WT/ffi/build/libxrpl_shim.a" ] || { say "ffi/build symlink lost"; return 1; }
  return 0
}

# part <name> <log> — run one battery part; sets ${name}_pass and metrics vars
declare -A M
run_part() {
  local name=$1 tmo=$2; shift 2
  local log=$LOOP/logs/record-$TS-$name.log
  say "part $name: $* (timeout ${tmo}s)"
  local dir=$WT/crates/xrpl-node
  [ "$name" = "suite" ] && dir=$WT
  pushd "$dir" >/dev/null || exit 70
  run_cargo "$log" "$tmo" "${@}"
  local rc=$?
  popd >/dev/null
  M[${name}_rc]=$rc
  if test_summary_pass "$log" && [ $rc -eq 0 ]; then M[${name}_pass]=1; else M[${name}_pass]=0; fi
  M[${name}_log]=$log
  case $name in
    g1|g2)
      # line-anchored: "silent diverged:"/"mutation diverged:" must not feed
      # the plain "diverged:" metric (parity-gate output, backlog_reapply)
      M[${name}_overlay]=$(grep -E 'overlay size:' "$log" | tail -1 | grep -oE '[0-9]+' | tail -1 || echo -)
      M[${name}_diverged]=$(grep -E '^\s*diverged:' "$log" | tail -1 | grep -oE '[0-9]+' | tail -1 || echo -)
      M[${name}_silent]=$(grep -E 'silent diverged:' "$log" | tail -1 | grep -oE '[0-9]+' | tail -1 || echo -)
      M[${name}_mut]=$(grep -E 'mutation diverged:' "$log" | tail -1 | grep -oE '[0-9]+' | tail -1 || echo -)
      M[${name}_attempted]=$(grep -E '^\s*attempted:' "$log" | tail -1 | grep -oE '[0-9]+' | tail -1 || echo -)
      ;;
  esac
  : "${M[g1_overlay]:=-}"; : "${M[g2_overlay]:=-}"
  if [ "${M[${name}_pass]}" = "0" ] && net_inconclusive "$log"; then
    M[inconclusive]=1
  fi
}

cmd_record() {
  local sha=$1 battery=$2
  # gate specs: "<testfile>:<filter>" — mission-configurable (Phase-2 retarget);
  # defaults preserve the original succ mission's gates
  local g1spec=${3:-ticket_cluster_reapply:bf6c928f}
  local g2spec=${4:-ticket_cluster_reapply:cluster_103515367}
  local G1_TEST=${g1spec%%:*} G1_FILTER=${g1spec#*:}
  local G2_TEST=${g2spec%%:*} G2_FILTER=${g2spec#*:}
  reap_stale; take_lock; require_mem
  clean_checkout "$sha" || { echo "@@RESULT class=harness reason=checkout"; exit 70; }
  local t0=$SECONDS
  M[inconclusive]=0
  case $battery in
    smoke)
      run_part smoke 300 "$CARGO" test --features ffi -j6 --test era_sentinel_reapply scan_sequence ;;
    g1)
      run_part g1 900 "$CARGO" test --features ffi -j6 --test "$G1_TEST" "$G1_FILTER" -- --ignored --nocapture --test-threads=1 ;;
    g1g2)
      run_part g1 900 "$CARGO" test --features ffi -j6 --test "$G1_TEST" "$G1_FILTER" -- --ignored --nocapture --test-threads=1
      run_part g2 900 "$CARGO" test --features ffi -j6 --test "$G2_TEST" "$G2_FILTER" -- --ignored --nocapture --test-threads=1 ;;
    final)
      run_part g1 900 "$CARGO" test --features ffi -j6 --test "$G1_TEST" "$G1_FILTER" -- --ignored --nocapture --test-threads=1
      run_part g2 900 "$CARGO" test --features ffi -j6 --test "$G2_TEST" "$G2_FILTER" -- --ignored --nocapture --test-threads=1
      run_part era 900 "$CARGO" test --features ffi -j6 --test era_sentinel_reapply -- --ignored --nocapture
      run_part suite 1200 "$CARGO" test -p xrpl-node --features ffi -j6
      find "$WT/crates/xrpl-node/src" -name '*.rs' -exec touch {} +
      local wlog=$LOOP/logs/record-$TS-warnings.log
      ( cd "$WT" && run_cargo "$wlog" 900 "$CARGO" build -p xrpl-node --features ffi -j6 )
      M[warnings]=$(grep -c '^warning:' "$wlog" || echo 0)
      M[warnings_log]=$wlog ;;
    baseline-warnings)
      find "$WT/crates/xrpl-node/src" -name '*.rs' -exec touch {} +
      local wlog=$LOOP/logs/record-$TS-warnings.log
      ( cd "$WT" && run_cargo "$wlog" 1200 "$CARGO" build -p xrpl-node --features ffi -j6 )
      M[warnings]=$(grep -c '^warning:' "$wlog" || echo 0)
      echo "@@RESULT class=baseline warnings=${M[warnings]} sha=$sha"
      exit 0 ;;
    *) say "unknown battery $battery"; exit 70 ;;
  esac

  # verdict
  local class=green pass=1
  for k in g1_pass g2_pass era_pass suite_pass smoke_pass; do
    [ "${M[$k]:-}" = "0" ] && { class=red; pass=0; }
  done
  if [ "$class" = "red" ] && [ "${M[inconclusive]}" = "1" ]; then class=inconclusive; fi
  echo "@@RESULT class=$class pass=$pass sha=$sha battery=$battery g1_pass=${M[g1_pass]:--} g1_overlay=${M[g1_overlay]:--} g1_diverged=${M[g1_diverged]:--} g1_silent=${M[g1_silent]:--} g1_mut=${M[g1_mut]:--} g1_attempted=${M[g1_attempted]:--} g2_pass=${M[g2_pass]:--} g2_diverged=${M[g2_diverged]:--} g2_silent=${M[g2_silent]:--} g2_mut=${M[g2_mut]:--} g2_attempted=${M[g2_attempted]:--} era_pass=${M[era_pass]:--} suite_pass=${M[suite_pass]:--} smoke_pass=${M[smoke_pass]:--} warnings=${M[warnings]:--} dur_s=$((SECONDS-t0))"
  case $class in
    green) exit 0 ;;
    red) exit 1 ;;
    inconclusive) exit 75 ;;
  esac
}

cmd_probe() {
  # scout mode: replay a pre-rsynced fixture (~/loop/scout/l<seq>_*) through
  # parity_probe. Read-only wrt the worktree.
  # Optional $2 = RPC URL for pre-state hydration (default s2 full-history;
  # the hunter passes .39 so nightly sweeps stay off public infrastructure).
  local seq=$1 rpc=${2:-}
  local blobs=$LOOP/scout/l${seq}_blobs.txt exp=$LOOP/scout/l${seq}_expected.json
  [ -f "$blobs" ] && [ -f "$exp" ] || { say "fixture for #$seq not staged"; exit 70; }
  reap_stale; take_lock; require_mem 4
  # Built from the HARNESS worktree, same as cmd_diff, and built UNCONDITIONALLY.
  # The old form pointed at $LOOP/target built from $WT and only built if absent,
  # so the binary sat at 2026-07-21 / 4005fb3 and never picked up probe fixes --
  # notably cae6b85, which makes a dropped fetch withhold the verdict instead of
  # reporting a phantom divergence. cargo is a no-op when the tree is current.
  local bin=$LOOP/harness/target/debug/parity_probe
  local jobs; jobs=$(probe_jobs)
  say "building parity_probe (harness, -j$jobs)…"
  local blog=$LOOP/logs/probe-build-$TS.log
  ( cd "$LOOP/harness/crates/xrpl-node" && CARGO_TARGET_DIR=$LOOP/harness/target run_cargo "$blog" 1800 "$CARGO" build --features ffi -j"$jobs" --bin parity_probe ) || { say "probe build failed"; tail -5 "$blog"; exit 70; }
  local log=$LOOP/logs/probe-$TS-$seq.log
  if [ -n "$rpc" ]; then
    run_cargo "$log" 900 "$bin" "$blobs" "$exp" --rpc "$rpc"
  else
    run_cargo "$log" 900 "$bin" "$blobs" "$exp"
  fi
  local rc=$?
  tail -20 "$log"
  echo "@@RESULT class=$([ $rc -eq 0 ] && echo clean || { [ $rc -eq 1 ] && echo divergent || echo error; }) mode=probe seq=$seq rc=$rc log=$log"
  exit $rc
}

cmd_diff() {
  # differential mode: measure the NATIVE engine vs mainnet on a staged fixture
  # (~/loop/scout/l<seq>_{blobs.txt,expected.json}). Read-only; builds bin if
  # absent. The binary comes from the HARNESS worktree (loop/differential-harness
  # branch — the corpus-100% engine), NOT the fixer worktree.
  local seq=$1 rpc=${2:-https://s2.ripple.com:51234}
  local blobs=$LOOP/scout/l${seq}_blobs.txt exp=$LOOP/scout/l${seq}_expected.json
  [ -f "$blobs" ] && [ -f "$exp" ] || { say "fixture for #$seq not staged"; exit 70; }
  reap_stale; take_lock; require_mem 4
  local bin=$LOOP/harness/target/debug/differential_probe
  if [ ! -x "$bin" ]; then
    local jobs; jobs=$(probe_jobs)
    say "building differential_probe (harness, -j$jobs)…"
    local blog=$LOOP/logs/diff-build-$TS.log
    ( cd "$LOOP/harness/crates/xrpl-node" && CARGO_TARGET_DIR=$LOOP/harness/target run_cargo "$blog" 1800 "$CARGO" build --features ffi -j"$jobs" --bin differential_probe ) || { say "diff build failed"; tail -5 "$blog"; exit 70; }
  fi
  local log=$LOOP/logs/diff-$TS-$seq.log
  run_cargo "$log" 900 "$bin" "$blobs" "$exp" --rpc "$rpc"
  local rc=$?
  cat "$log"
  echo "@@RESULT class=$([ $rc -eq 0 ] && echo allmatch || { [ $rc -eq 1 ] && echo divergent || echo error; }) mode=diff seq=$seq rc=$rc log=$log"
  exit $rc
}

case ${1:-} in
  env) cmd_env ;;
  unit) shift; cmd_unit "${1:-}" ;;
  record) shift; [ $# -ge 2 ] || { say "record needs <sha> <battery>"; exit 70; }; cmd_record "$@" ;;
  probe) shift; [ $# -ge 1 ] || { say "probe needs <seq>"; exit 70; }; cmd_probe "$1" "${2:-}" ;;
  diff) shift; [ $# -ge 1 ] || { say "diff needs <seq>"; exit 70; }; cmd_diff "$1" "${2:-}" ;;
  *) say "usage: runner.sh env|unit <filter>|record <sha> <battery>|probe <seq>|diff <seq> [rpc]"; exit 70 ;;
esac
