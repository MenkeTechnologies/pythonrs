#!/usr/bin/env bash
# dropin_check.sh — drop-in readiness gauge.
#
# Runs every script in tests/dropin/ through pythonrs and the reference
# `python3` with identical argv and an isolated cwd, then diffs stdout + exit
# code. It measures whether pythonrs can transparently replace `python3` for the
# kinds of scripts an agent actually writes — file I/O, argv, subprocess, the
# common stdlib. This is the real readiness test behind CHECKLIST.md: the fuzzer
# proves language-semantics parity, this proves whole-script parity.
#
# Verdicts per script:
#   OK    both interpreters agree (stdout + exit code)
#   DIFF  both ran, output/exit differ  (behavior gap)
#   ERR   pythonrs failed where python3 succeeded (missing feature/module)
#   SKIP  reference python3 itself rejected it (bug in the corpus script)
#
# Category = filename prefix (io_, re_, json_, ...). Exit 0 iff every script is
# OK. Needs python3 on PATH, so CI never runs it.
#
#   PYTHONRS_BIN=... PYTHONRS_REF=python3.14 ./scripts/dropin_check.sh
set -u

here="$(cd "$(dirname "$0")/.." && pwd)"
ours="${PYTHONRS_BIN:-$here/target/debug/python}"
ref="${PYTHONRS_REF:-python3}"
corpus="$here/tests/dropin"
args=(alpha beta 42)   # the fixed argv every script sees

command -v "$ref" >/dev/null 2>&1 || { echo "dropin_check: no reference '$ref' on PATH"; exit 2; }
[ -x "$ours" ] || { echo "dropin_check: pythonrs not built at $ours (run: cargo build)"; exit 2; }

# Determinism: pin hash seed (sets) and keep the reference from writing .pyc.
export PYTHONHASHSEED=0 PYTHONDONTWRITEBYTECODE=1

echo "reference : $("$ref" --version 2>&1)"
echo "pythonrs  : $ours"
echo

ok=0; diff=0; err=0; skip=0; total=0
declare -A cat_ok cat_tot
fails=()

for f in "$corpus"/*.py; do
  [ -e "$f" ] || continue
  name="$(basename "$f")"
  cat="${name%%_*}"
  total=$((total+1)); cat_tot[$cat]=$(( ${cat_tot[$cat]:-0} + 1 ))

  work="$(mktemp -d)"; run="$work/run"; mkdir -p "$run"
  cp "$f" "$run/prog.py"

  ( cd "$run" && "$ref"  prog.py "${args[@]}" ) >"$work/ref.out" 2>/dev/null; rr=$?
  # reset the sandbox so a file-writing script starts from the same state
  find "$run" -mindepth 1 ! -name prog.py -delete 2>/dev/null
  ( cd "$run" && "$ours" prog.py "${args[@]}" ) >"$work/our.out" 2>/dev/null; oo=$?

  if [ "$rr" != "0" ]; then
    printf 'SKIP %-30s (python3 rejected it)\n' "$name"; skip=$((skip+1)); rm -rf "$work"; continue
  fi
  if cmp -s "$work/ref.out" "$work/our.out" && [ "$oo" = "$rr" ]; then
    printf 'OK   %s\n' "$name"; ok=$((ok+1)); cat_ok[$cat]=$(( ${cat_ok[$cat]:-0} + 1 ))
  elif [ "$oo" != "0" ] && [ ! -s "$work/our.out" ]; then
    printf 'ERR  %-30s exit=%s (missing feature/module)\n' "$name" "$oo"; err=$((err+1)); fails+=("$name")
  else
    # Show the first DIFFERING line (not the first line), so a multi-line match
    # with a divergence deep in the output points at the real gap.
    r1="$(diff "$work/ref.out" "$work/our.out" | grep -m1 '^< ' | cut -c3-)"
    o1="$(diff "$work/ref.out" "$work/our.out" | grep -m1 '^> ' | cut -c3-)"
    [ "$oo" != "$rr" ] && [ -z "$r1$o1" ] && { r1="(exit $rr)"; o1="(exit $oo)"; }
    printf 'DIFF %-30s ref[%.44s] ours[%.44s]\n' "$name" "$r1" "$o1"; diff=$((diff+1)); fails+=("$name")
  fi
  rm -rf "$work"
done

echo
echo "── by category ──"
for c in $(printf '%s\n' "${!cat_tot[@]}" | sort); do
  printf '  %-12s %d/%d\n' "$c" "${cat_ok[$c]:-0}" "${cat_tot[$c]}"
done
echo
pct=0; [ "$total" -gt 0 ] && pct=$(( ok * 100 / total ))
echo "readiness: $ok/$total OK (${pct}%)  |  $diff DIFF, $err ERR, $skip SKIP"
[ "$diff" -eq 0 ] && [ "$err" -eq 0 ] && [ "$skip" -eq 0 ] && exit 0 || exit 1
