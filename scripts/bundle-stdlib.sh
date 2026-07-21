#!/usr/bin/env bash
# bundle-stdlib.sh — assemble a self-contained pythonrs release tree.
#
# pythonrs (built with --features stdlib-ffi) imports the REAL CPython stdlib
# over an embedded libpython. To ship a binary that runs on a machine WITHOUT
# python@3.14 installed, the release must carry three things next to the binary:
#
#   <prefix>/bin/python                    the pythonrs binary (feature build)
#   <prefix>/lib/python3.14/               the pure .py stdlib
#   <prefix>/lib/python3.14/lib-dynload/   the C-accelerator .so modules
#   <prefix>/lib/libpython3.14.dylib       the CPython runtime itself (.so on Linux)
#
# This is exactly the layout `ffi::resolve_home()` looks for: with the binary at
# `<prefix>/bin/python` it tests `<exe_dir>/../lib/python3.14` and, on a hit, sets
# PYTHONHOME to `<prefix>` before Py_Initialize. So the bundled tree resolves with
# zero env vars — the "bundled" branch of PYTHONRS_STDLIB → bundled → system.
#
# On macOS the feature build hard-links libpython by absolute framework path; this
# script rewrites that load command to `@executable_path/../lib/<dylib>` and
# re-signs (ad-hoc) so the bundled dylib is found at runtime. On Linux it sets an
# `$ORIGIN/../lib` rpath via patchelf (or errors telling you to install it).
#
# Usage:
#   scripts/bundle-stdlib.sh [--prefix DIR] [--bin PATH] [--triple T] [--out DIR]
#
#   --prefix DIR  CPython prefix (default: python3-config --prefix, else sys.prefix)
#   --bin PATH    built pythonrs binary (default: target/release/python, else debug)
#   --triple T    target triple for the dist subdir (default: rustc host triple)
#   --out DIR     staging root (default: dist/<triple>)
#
# Produces the tree under <out> and prints its path. Idempotent: re-running wipes
# and rebuilds <out>.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"

prefix=""
bin=""
triple=""
out=""

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --bin)    bin="$2";    shift 2 ;;
    --triple) triple="$2"; shift 2 ;;
    --out)    out="$2";    shift 2 ;;
    -h|--help) grep '^#' "$0" | cut -c3-; exit 0 ;;
    *) echo "bundle-stdlib: unknown arg: $1" >&2; exit 2 ;;
  esac
done

# ── Resolve inputs ───────────────────────────────────────────────────────────
if [ -z "$prefix" ]; then
  prefix="$(python3-config --prefix 2>/dev/null || python3 -c 'import sys;print(sys.prefix)')"
fi
[ -d "$prefix" ] || { echo "bundle-stdlib: CPython prefix not a dir: $prefix" >&2; exit 1; }

# The stdlib dir name (python3.14) is derived from the prefix, not hardcoded, so
# the script tracks whatever minor the toolchain shipped.
libpy="$(basename "$(ls -d "$prefix"/lib/python3.* 2>/dev/null | head -1)")"
[ -n "$libpy" ] && [ -d "$prefix/lib/$libpy" ] \
  || { echo "bundle-stdlib: no lib/python3.* under $prefix" >&2; exit 1; }
pyver="${libpy#python}"            # e.g. 3.14

if [ -z "$bin" ]; then
  if   [ -x "$here/target/release/python" ]; then bin="$here/target/release/python"
  elif [ -x "$here/target/debug/python" ];   then bin="$here/target/debug/python"
  else echo "bundle-stdlib: no built binary; pass --bin or build --features stdlib-ffi" >&2; exit 1
  fi
fi
[ -x "$bin" ] || { echo "bundle-stdlib: binary not executable: $bin" >&2; exit 1; }

[ -n "$triple" ] || triple="$(rustc -vV | awk '/^host:/{print $2}')"
[ -n "$out" ] || out="$here/dist/$triple"

os="$(uname -s)"
case "$os" in
  Darwin) dylib="libpython${pyver}.dylib" ;;
  Linux)  dylib="libpython${pyver}.so" ;;   # actual soname resolved from the binary below
  *) echo "bundle-stdlib: unsupported OS: $os" >&2; exit 1 ;;
esac

echo "bundle-stdlib:"
echo "  prefix : $prefix"
echo "  stdlib : $libpy  (python $pyver)"
echo "  binary : $bin"
echo "  triple : $triple"
echo "  out    : $out"

# ── Stage the tree ───────────────────────────────────────────────────────────
rm -rf "$out"
mkdir -p "$out/bin" "$out/lib"

# 1) binary → <prefix>/bin/python
install -m 0755 "$bin" "$out/bin/python"

# 2) pure stdlib + lib-dynload → <prefix>/lib/python3.14/
#    __pycache__ is dropped (regenerated at runtime; arch/version-specific bloat).
echo "  copying stdlib (this includes lib-dynload/*.so) ..."
cp -R "$prefix/lib/$libpy" "$out/lib/$libpy"
find "$out/lib/$libpy" -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null || true
if [ ! -d "$out/lib/$libpy/lib-dynload" ]; then
  echo "bundle-stdlib: WARNING: no lib-dynload/ — C-accelerator modules (_sre, _hashlib, ...) missing" >&2
fi

# 3) libpython → <prefix>/lib/<dylib>  (dereference: the prefix copy is a symlink
#    into the framework/real dylib; -L copies the real file).
if [ "$os" = "Darwin" ]; then
  src_dylib="$prefix/lib/$dylib"
  [ -e "$src_dylib" ] || { echo "bundle-stdlib: missing $src_dylib" >&2; exit 1; }
  cp -L "$src_dylib" "$out/lib/$dylib"
  chmod u+w "$out/lib/$dylib"
else
  # Linux: the binary names the exact soname (e.g. libpython3.14.so.1.0); copy the
  # real file the loader resolves, keeping that name so the rpath lookup matches.
  src_dylib="$(ldd "$out/bin/python" 2>/dev/null | awk '/libpython/{print $3; exit}')"
  [ -n "$src_dylib" ] && [ -e "$src_dylib" ] \
    || { echo "bundle-stdlib: could not resolve linked libpython via ldd" >&2; exit 1; }
  dylib="$(basename "$src_dylib")"
  cp -L "$src_dylib" "$out/lib/$dylib"
  chmod u+w "$out/lib/$dylib"
fi
echo "  bundled runtime: lib/$dylib"

# ── Make the bundled libpython findable at runtime ───────────────────────────
if [ "$os" = "Darwin" ]; then
  # Rewrite the absolute framework reference to @executable_path/../lib/<dylib>.
  # awk splits on whitespace, so $1 is the load path with the leading tab stripped
  # (a tab-prefixed name would make install_name_tool -change a silent no-op).
  old="$(otool -L "$out/bin/python" \
    | awk '/Python\.framework\/Versions\/[0-9.]+\/Python|libpython[0-9.]+\.dylib/{print $1; exit}')"
  if [ -n "$old" ]; then
    install_name_tool -change "$old" "@executable_path/../lib/$dylib" "$out/bin/python"
    echo "  relinked: $old"
    echo "         -> @executable_path/../lib/$dylib"
  else
    echo "bundle-stdlib: WARNING: no libpython load command found to relink" >&2
  fi
  # install_name_tool invalidates the binary's ad-hoc signature, AND the copied
  # libpython dylib + lib-dynload/*.so carry signatures bound to their original
  # on-disk slice; once relocated into the bundle, Apple Silicon's dyld rejects
  # them ("code signature invalid" -> Abort trap: 6). Ad-hoc re-sign EVERY Mach-O
  # in the tree so the whole bundle loads on arm64.
  _resign() {
    codesign --remove-signature "$1" 2>/dev/null || true
    codesign -s - -f "$1" 2>/dev/null \
      || echo "bundle-stdlib: WARNING: codesign failed for $1; may need manual signing" >&2
  }
  _resign "$out/bin/python"
  _resign "$out/lib/$dylib"
  if [ -d "$out/lib/$libpy/lib-dynload" ]; then
    for _so in "$out/lib/$libpy/lib-dynload"/*.so; do
      [ -e "$_so" ] && _resign "$_so"
    done
  fi
else
  # Linux: point the binary at $ORIGIN/../lib for the bundled soname.
  if command -v patchelf >/dev/null 2>&1; then
    # $ORIGIN must reach patchelf literally (resolved by the loader, not the shell).
    # shellcheck disable=SC2016
    patchelf --set-rpath '$ORIGIN/../lib' "$out/bin/python"
    echo "  set rpath: \$ORIGIN/../lib"
  else
    echo "bundle-stdlib: patchelf not found — install it, or users must run with" >&2
    echo "  LD_LIBRARY_PATH=<prefix>/lib to locate the bundled libpython." >&2
  fi
fi

echo
echo "bundle-stdlib: self-contained tree at $out"
echo "  verify:  ( cd $out && env -u PYTHONHOME -u PYTHONPATH bin/python -c 'import sys;print(sys.prefix)' )"
