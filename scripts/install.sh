#!/usr/bin/env bash
# install.sh — install a fully self-contained pythonrs into ~/.pythonrs.
#
# pythonrs (feature `stdlib-ffi`) runs on an embedded libpython and imports the
# REAL CPython stdlib. To be independent of Homebrew — so `brew uninstall python`
# (and openssl/sqlite/xz/zstd/mpdecimal) leaves pythonrs working — this vendors
# EVERY non-system dylib the runtime touches into ~/.pythonrs/lib and rewrites all
# load commands to `@rpath`, then re-signs. The result references nothing under
# /opt/homebrew.
#
# Layout produced (co-located with the ~/.pythonrs bytecode cache):
#   ~/.pythonrs/bin/python                 the pythonrs binary
#   ~/.pythonrs/lib/libpython3.14.dylib    the CPython runtime
#   ~/.pythonrs/lib/lib{crypto,ssl,...}    the C-extensions' transitive deps
#   ~/.pythonrs/lib/python3.14/            the pure stdlib + lib-dynload/*.so
#
# `ffi::resolve_home()` finds this tree (via `<exe>/../lib` or the ~/.pythonrs
# fallback) and pins PYTHONHOME to it before Py_Initialize. Put ~/.pythonrs/bin on
# PATH (or symlink bin/python) to use it as `python`.
#
# Usage: scripts/install.sh [--prefix DIR] [--bin PATH] [--release]
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
prefix="" bin="" build_release=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    --release) build_release=1; shift ;;
    -h|--help) grep '^#' "$0" | cut -c3-; exit 0 ;;
    *) echo "install: unknown arg: $1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "install: this script targets macOS" >&2; exit 1; }

# ── Resolve inputs ───────────────────────────────────────────────────────────
[ -n "$prefix" ] || prefix="$(python3-config --prefix 2>/dev/null || python3 -c 'import sys;print(sys.base_prefix)')"
[ -d "$prefix" ] || { echo "install: CPython prefix not a dir: $prefix" >&2; exit 1; }
libpy="$(basename "$(ls -d "$prefix"/lib/python3.* 2>/dev/null | head -1)")"
[ -n "$libpy" ] || { echo "install: no lib/python3.* under $prefix" >&2; exit 1; }
pyver="${libpy#python}"
dylib="libpython${pyver}.dylib"

if [ -z "$bin" ]; then
  if [ "$build_release" = 1 ]; then
    ( cd "$here" && cargo build --release --bin python )
    bin="$here/target/release/python"
  elif [ -x "$here/target/release/python" ]; then bin="$here/target/release/python"
  elif [ -x "$here/target/debug/python" ];   then bin="$here/target/debug/python"
  else echo "install: no built binary; run with --release or 'cargo build'" >&2; exit 1
  fi
fi
[ -x "$bin" ] || { echo "install: binary not executable: $bin" >&2; exit 1; }

out="$HOME/.pythonrs"
echo "install: prefix=$prefix  python=$pyver  bin=$bin  ->  $out"

# ── Stage the tree ───────────────────────────────────────────────────────────
# Only wipe the runtime dirs; keep the sibling scripts.rkyv bytecode cache.
rm -rf "$out/bin" "$out/lib"
mkdir -p "$out/bin" "$out/lib"

install -m 0755 "$bin" "$out/bin/python"

echo "  copying stdlib (with lib-dynload/*.so) ..."
cp -R "$prefix/lib/$libpy" "$out/lib/$libpy"
find "$out/lib/$libpy" -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null || true

# The runtime itself: `lib/libpython3.14.dylib` is a symlink into the framework;
# -L copies the real Mach-O.
cp -L "$prefix/lib/$dylib" "$out/lib/$dylib"
chmod u+w "$out/lib/$dylib"

# ── Vendor + relink every non-system dylib to @rpath ─────────────────────────
# A worklist of Mach-O files to scan; each /opt/homebrew (or framework) dependency
# is copied into lib/ once, its id set to @rpath/<base>, and every referrer's load
# command rewritten to @rpath/<base>. New copies are appended, so transitive deps
# (libssl -> libcrypto) are followed to a fixpoint.
lib="$out/lib"
declare -a work=()
work+=("$out/bin/python")
work+=("$lib/$dylib")
for so in "$lib/$libpy"/lib-dynload/*.so; do [ -e "$so" ] && work+=("$so"); done

# Map a dependency load path to the vendored basename. libpython is copied under a
# fixed name (its framework path has basename "Python").
vend_base() {
  case "$1" in
    *"/Python.framework/"*"/Python") echo "$dylib" ;;
    *) basename "$1" ;;
  esac
}

is_vendorable() {
  case "$1" in
    /opt/homebrew/*|*/Python.framework/*/Python) return 0 ;;
    *) return 1 ;;
  esac
}

i=0
while [ "$i" -lt "${#work[@]}" ]; do
  f="${work[$i]}"; i=$((i + 1))
  chmod u+w "$f" 2>/dev/null || true
  # Its own id (line 1 of otool -L for a dylib) -> @rpath/<base> for cleanliness.
  if [ "$f" != "$out/bin/python" ]; then
    install_name_tool -id "@rpath/$(basename "$f")" "$f" 2>/dev/null || true
  fi
  # Each vendorable dependency.
  while IFS= read -r dep; do
    is_vendorable "$dep" || continue
    base="$(vend_base "$dep")"
    if [ ! -e "$lib/$base" ]; then
      cp -L "$dep" "$lib/$base"
      chmod u+w "$lib/$base"
      work+=("$lib/$base")            # follow its own deps next
    fi
    install_name_tool -change "$dep" "@rpath/$base" "$f" 2>/dev/null || true
  done < <(otool -L "$f" | awk 'NR>1{print $1}')
done

# ── rpaths: every referrer must reach lib/ via @rpath ────────────────────────
install_name_tool -add_rpath "@executable_path/../lib" "$out/bin/python" 2>/dev/null || true
for d in "$lib"/*.dylib; do
  [ -e "$d" ] && install_name_tool -add_rpath "@loader_path" "$d" 2>/dev/null || true
done
for so in "$lib/$libpy"/lib-dynload/*.so; do
  [ -e "$so" ] && install_name_tool -add_rpath "@loader_path/../.." "$so" 2>/dev/null || true
done

# ── Re-sign every Mach-O (relocation + install_name_tool invalidate signatures;
#    arm64 dyld hard-rejects an invalid signature). ─────────────────────────────
resign() { codesign --remove-signature "$1" 2>/dev/null || true; codesign -s - -f "$1" >/dev/null 2>&1 || echo "install: WARN codesign $1" >&2; }
resign "$out/bin/python"
for d in "$lib"/*.dylib; do [ -e "$d" ] && resign "$d"; done
for so in "$lib/$libpy"/lib-dynload/*.so; do [ -e "$so" ] && resign "$so"; done

# ── Verify: nothing under /opt/homebrew remains ──────────────────────────────
leaks="$( { for f in "$out/bin/python" "$lib"/*.dylib "$lib/$libpy"/lib-dynload/*.so; do
             [ -e "$f" ] && otool -L "$f" 2>/dev/null | awk 'NR>1{print $1}'
           done; } | grep -c "/opt/homebrew" || true )"

echo
echo "install: staged $out  ($(du -sh "$out/lib" 2>/dev/null | cut -f1))"
echo "install: /opt/homebrew references remaining: $leaks"
if [ "$leaks" -ne 0 ]; then
  echo "install: NOT self-contained — $leaks Homebrew reference(s) left:" >&2
  for f in "$out/bin/python" "$lib"/*.dylib "$lib/$libpy"/lib-dynload/*.so; do
    [ -e "$f" ] && otool -L "$f" 2>/dev/null | awk 'NR>1{print $1}' | grep -q /opt/homebrew \
      && { echo "  $f:" >&2; otool -L "$f" | awk 'NR>1{print $1}' | grep /opt/homebrew | sed 's/^/    /' >&2; }
  done
  exit 1
fi
echo "install: self-contained. Add ~/.pythonrs/bin to PATH (or symlink bin/python)."
