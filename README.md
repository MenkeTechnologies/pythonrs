```
██████╗ ██╗   ██╗████████╗██╗  ██╗ ██████╗ ███╗   ██╗██████╗ ███████╗
██╔══██╗╚██╗ ██╔╝╚══██╔══╝██║  ██║██╔═══██╗████╗  ██║██╔══██╗██╔════╝
██████╔╝ ╚████╔╝    ██║   ███████║██║   ██║██╔██╗ ██║██████╔╝███████╗
██╔═══╝   ╚██╔╝     ██║   ██╔══██║██║   ██║██║╚██╗██║██╔══██╗╚════██║
██║        ██║      ██║   ██║  ██║╚██████╔╝██║ ╚████║██║  ██║███████║
╚═╝        ╚═╝      ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝╚═╝  ╚═╝╚══════╝
```

[![CI](https://github.com/MenkeTechnologies/pythonrs/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/pythonrs/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
[![Docs](https://img.shields.io/badge/docs-online-blue.svg)](https://menketechnologies.github.io/pythonrs/)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

### `[PYTHON, COMPILED TO BYTECODE — rkyv-CACHED ON EVERY RUN, AOT-NATIVE]`

> *"CPython compiles to its own bytecode and walks it. pythonrs lowers Python to a shared machine, caches the result on every run, and can bake a script into a native binary."*

**pythonrs** is Python as a [`fusevm`](https://github.com/MenkeTechnologies/fusevm)
frontend — a lexer/parser and compiler that lowers Python 3 to `fusevm::Chunk`
bytecode running on the fusevm three-tier Cranelift JIT, over a `PyHost` object
heap. There is no bespoke VM and no bespoke JIT: pythonrs is a pure front end;
execution and codegen live in `fusevm` — the same engine behind
[`zshrs`](https://github.com/MenkeTechnologies/zshrs),
[`strykelang`](https://github.com/MenkeTechnologies/strykelang),
[`awkrs`](https://github.com/MenkeTechnologies/awkrs),
[`vimlrs`](https://github.com/MenkeTechnologies/vimlrs),
[`elisprs`](https://github.com/MenkeTechnologies/elisprs), and
[`rubylang`](https://github.com/MenkeTechnologies/rubylang).

It is the first compiled standalone Python runtime that both
**transparently caches bytecode via rkyv on every run** and **AOT-compiles a
script to a native executable**.

### [`Read the Docs`](https://menketechnologies.github.io/pythonrs/) &middot; [`Engineering Report`](https://menketechnologies.github.io/pythonrs/report.html) &middot; [`Builtin Reference`](https://menketechnologies.github.io/pythonrs/reference.html)

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Usage](#0x02-usage)
- [\[0x03\] Language Features](#0x03-language-features)
- [\[0x04\] Command-Line Flags](#0x04-command-line-flags)
- [\[0x05\] Architecture](#0x05-architecture)
- [\[0x06\] Parity Harness](#0x06-parity-harness)
- [\[0x07\] Status & Roadmap](#0x07-status--roadmap)
- [\[0x08\] Documentation](#0x08-documentation)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] OVERVIEW

pythonrs keeps Python the language and throws away CPython's execution model. It
lexes and parses Python to an AST, lowers the AST to `fusevm` bytecode, and runs
the bytecode on a stack VM with a Cranelift JIT. Arithmetic and comparisons lower
to native ops; Python-specific behavior — truthiness, `str`/`list` concat, bignum
promotion, exact `int`-against-`float` comparison, attribute and method dispatch —
runs through a strict numeric hook and a numbered builtin-call protocol into the
`PyHost` object heap.

Two things set it apart from every other standalone Python:

- **Transparent rkyv bytecode cache — on every run.** `python foo.py` hashes the
  source, consults `~/.pythonrs/scripts.rkyv`, and on a hit runs the compiled
  chunks directly with lex/parse/lower skipped entirely. No flags, no separate
  build step, no `__pycache__` ritual.
- **AOT to a native executable.** `python --build foo.py` emits a standalone
  native binary (via `fusevm::aot`, linked against the pythonrs runtime
  staticlib) that runs the script with no interpreter present. This path needs
  the libpython-free build (`cargo build --no-default-features`).

## [0x01] INSTALL

```sh
# Via the Homebrew tap (bumped by each release; formula is `pythonrs`)
brew install menketechnologies/menketech/pythonrs

# Or from source
git clone https://github.com/MenkeTechnologies/pythonrs
cd pythonrs && cargo build --release
# binary: target/release/python  (+ libpythonrs.a for AOT linking)
```

The default build links an embedded libpython (the `stdlib-ffi` bridge), so it
needs CPython **3.13 or newer** present at build time — the `abi3-py313` feature
sets that floor. pyo3 finds the interpreter by looking up `python3` on `PATH`, so
a machine whose `python3` is older fails the build with

```
error: cannot set a minimum Python version 3.13 higher than the interpreter
version 3.12 (the minimum Python version is implied by the abi3-py313 feature)
```

even when a newer one is installed alongside. Point pyo3 at it explicitly:

```sh
PYO3_PYTHON=$(command -v python3.13) cargo build
```

The built binary links that interpreter's libpython, and finding its standard
library at RUN time needs `PYTHONHOME` set to the matching prefix (see
[FFI_STDLIB.md](FFI_STDLIB.md)); without it `import os` raises `ModuleNotFoundError`
and `sys.path` comes back nearly empty:

```sh
PYTHONHOME=$(python3.13 -c 'import sys; print(sys.prefix)') ./target/debug/python script.py
```

`cargo build --no-default-features` drops pyo3/libpython entirely and serves
`import` from the vendored `pylib/` tree, with no version floor and no
`PYTHONHOME` to set.

#### Self-contained install (macOS)

```sh
scripts/install.sh --release
```

Installs into `~/.pythonrs` — the binary, the CPython runtime, and every
transitive dylib the C extensions touch — with all load commands rewritten to
`@rpath` and re-signed, so nothing under `/opt/homebrew` is referenced and
`brew uninstall python` leaves pythonrs working. Put `~/.pythonrs/bin` on `PATH`
(or **symlink** `bin/python`; a bare `cp` breaks the `@executable_path` rpath).

#### Zsh tab completion

```sh
cp completions/_python "${fpath[1]}/"
# or: fpath=(/path/to/pythonrs/completions $fpath) in .zshrc
```

## [0x02] USAGE

```sh
python foo.py               # run a script (transparently rkyv-cached)
python -c 'print(1 + 1)'    # run a one-liner
python --build foo.py       # AOT-compile to a native ./foo executable
python --dump-bytecode f.py # print the lowered fusevm bytecode
python --tiers f.py         # run it, then report which fusevm tiers took it
python --repl               # interactive REPL
python --lsp                # Language Server Protocol over stdio
python --doctor             # runtime / CPython / cache / env diagnostic report
python --cacheview          # list the compiled programs in the bytecode cache
python --cache-clear        # delete the bytecode cache shard
```

The REPL is a `reedline` line editor: **Tab** pops a columnar completion menu
(Shift+Tab / BackTab cycles backward). On a bare word it offers the language
keywords, builtins, `math.*`, per-type method names, and the live module
globals / class names of the persistent session. After a `name.` it switches to
**type-aware attribute completion** — it reads the receiver's live runtime type
and offers exactly that surface: `str`/`list`/`dict`/`set`/`tuple`/`int`/`float`
methods for a builtin value, an imported module's own namespace, or an instance's
attributes plus every method reachable along its class MRO. History persists to
`~/.pythonrs/history`.

Set `PYTHONRS_TRACE=1` to log cache hit/miss to stderr (silent otherwise). Set
`PYTHONRS_CACHE=0` (or `false`/`no`) to disable the transparent bytecode cache
entirely — every run recompiles and nothing is stored. `PYTHONRS_STDLIB` overrides
the embedded-CPython stdlib prefix (checked before the bundled and system
locations); `PYTHONRS_LIB` overrides the vendored `pylib/` search path used by a
`--no-default-features` build.

`PYTHONHASHSEED` is honoured exactly as CPython honours it. A pinned seed
installs the same `_Py_HashSecret` CPython derives (`lcg_urandom` over the seed;
`0` zeroes it), so `hash('abc')` is byte-identical to `PYTHONHASHSEED=N python3`
for every `N` in `[0, 4294967295]` — not just for `0`. Unset or `random` draws
per-process entropy, as CPython does, and a value CPython rejects is rejected
here with the same message and exit code.

## [0x03] LANGUAGE FEATURES

Arbitrary-precision integers, real closures, classes with inheritance, operator
dunders, generators (`yield` / `yield from` / lazy generator expressions, backed
by stackful `corosensei` coroutines), `match`/`case` structural pattern matching,
own-scope comprehensions (list/dict/set) and proper `nonlocal`, f-strings,
exceptions, and full call-site and literal `*` / `**` unpacking. The `PyHost`
heap implements the `str` / `list` / `dict` / `tuple` / `set` / instance object
model with the operator, attribute, item, and iteration protocols. See
[\[0x07\]](#0x07-status--roadmap) and [BUGS.md](BUGS.md) for the honest list of
what is not yet implemented.

## [0x04] COMMAND-LINE FLAGS

| Flag | Effect |
|---|---|
| *(none)* | Run the script/one-liner, transparently rkyv-cached. |
| `-c SRC` | Execute a one-liner (`python -c 'print(1+1)'`). |
| `-m MODULE …` | Run a library module as a script. Delegates to the embedded CPython (`runpy`), so `-m pip` / `-m venv` / `-m http.server` / `-m json.tool` behave exactly like `python3 -m`; every token after the module is the module's own `sys.argv`. Needs the `stdlib-ffi` bridge (default build). |
| `-u` | Sets `PYTHONUNBUFFERED` for the embedded interpreter. pythonrs's own `print` is already unbuffered on every stream, so the flag changes nothing on that side — see [BUGS.md](BUGS.md) for the buffering divergence this implies. |
| `-E -I -O -S -B -W` | CPython interpreter flags, accepted for drop-in compatibility (`-u`/`-W` take real effect via the embedded interpreter; the rest are tolerated no-ops). |
| `--build` | AOT-compile the script to a standalone native executable. Needs a libpython-free runtime — build with `--no-default-features`; a `stdlib-ffi` build refuses up front (its CPython symbols can't be statically linked). |
| `--dump-bytecode` | Print the lowered `fusevm` bytecode and exit. |
| `--dump-tokens` | Print the lexer token stream and exit. |
| `--dump-ast` | Print the parsed AST and exit. |
| `--disasm` | Print a `fusevm` bytecode disassembly listing and exit. |
| `--tiers` | Run the script, then report which fusevm execution tier took each of its chunks. |
| `--repl` | Start the interactive REPL. |
| `--lsp` | Run the Language Server Protocol server over stdio. |
| `--dap` | Run the Debug Adapter Protocol server over stdio — breakpoints, stepping, stack trace, locals, expression `evaluate`. |
| `--doctor` | Print a diagnostic report — runtime, embedded CPython, fusevm engine, bytecode cache, `PYTHON*` env, and every `python*` interpreter on `PATH` — and exit. |
| `--cacheview` | List the compiled programs held in the rkyv bytecode cache (`~/.pythonrs/scripts.rkyv`): per-entry hashes, blob size, and op/function/try/warning counts. |
| `--cache-clear` | Delete the rkyv bytecode cache shard and exit. |

## [0x05] ARCHITECTURE

```
lexer  →  parser  →  AST  →  compiler  →  fusevm::Chunk  →  fusevm VM + JIT
                                              │                    │
                                              └── CallBuiltin ─────→ host (PyHost heap)
```

- `lexer.rs` — indentation-significant tokenizer (INDENT/DEDENT/NEWLINE, f-strings).
- `parser.rs` — recursive-descent Python grammar → `ast.rs`.
- `compiler.rs` — lowers the AST to fusevm ops + `CallBuiltin` dispatches.
- `host.rs` — the `PyHost` object heap (str/list/dict/tuple/set/instances/…), the
  operator/attribute/item/iteration semantics, and the fusevm run plumbing.
- `builtins.rs` — the `CallBuiltin` handler table, the numeric hook, the Kernel
  builtin functions (`print` / `len` / `range` / …), and per-type methods.
- `cache.rs` — the rkyv-shard bytecode cache.
- `aot_native.rs` — native-executable emission via `fusevm::aot`.
- `pylib/` — the vendored CPython pure-Python standard library (`.py` sources)
  shipped **with** pythonrs. In the native build these are imported by compiling
  and executing them on pythonrs's own interpreter — no libpython.

### Standard library: two build modes

The `import` path resolves a module from native inline arms first, then:

| Build | Command | `import <stdlib>` source |
|---|---|---|
| **Native (CPython-free)** | `cargo build --no-default-features` | The vendored `pylib/*.py`, compiled on pythonrs and run on fusevm. No pyo3, no libpython — CPython is not in the dependency graph. This is the shipping target (`brew install pythonrs` lays `pylib/` beside the binary). |
| **Bridged (drop-in)** | `cargo build` | The real CPython stdlib over an embedded libpython (pyo3, the `stdlib-ffi` feature). Kept primary while the native build's C-accelerator floor (`posix`/`_io`/`_sre`/…) is completed. |

Imports are memoized through the host's `sys.modules` cache, so a module's
vendored `.py` executes at most once (CPython run-once identity semantics).
`$PYTHONRS_LIB` overrides the `pylib/` search path.

## [0x06] PARITY HARNESS

Correctness is measured, not asserted: an example corpus runs through both
pythonrs and the reference `python3`, and the output is diffed byte-for-byte.
pythonrs runs a large, real subset of Python 3, verified against CPython on that
corpus.

Beyond the fixed corpus, the `parity-fuzz` binary is a differential fuzzer. It
generates thousands of grammar-driven, deterministic-output snippets — biased
toward the historically fragile areas (float `repr`, integer `//`/`%` sign
rules, bignum, slices, the `format` mini-language, string methods, containers
whose elements key through user `__hash__`/`__eq__`, `re` match positions over
subjects mixing 1-, 2-, 3- and 4-byte characters, and the exception boundary
between user code and the standard library — a stdlib `KeyError` coming back
with its key, a `@contextlib.contextmanager` driving a user generator through
`next`/`throw`/`close`) — runs each
through `python -c` and the reference `python3 -c`, and reports every case where
stdout or accept/reject diverges. Each case is seeded, so any divergence is
delta-debugged to a minimal reproducer and replays exactly:

A curated corpus has one structural blind spot worth naming: it can only report
constructs somebody thought to write down. A keyword-only argument, a function
nobody happened to call, or a method missing from the note-taker's mental list
stays invisible no matter how many cases run. Diffing the names `src/` dispatches
against the identifiers the corpus actually contains is what turns that blind
spot into a work list — `--mode containertail` exists because that diff surfaced
a dozen `itertools`/`collections`/`math` gaps at once, several of them silently
wrong answers rather than errors.

```sh
cargo build --bin parity-fuzz
./target/debug/parity-fuzz --count 5000          # fuzz every mode
./target/debug/parity-fuzz --formatspec          # one surface only
./target/debug/parity-fuzz --seed 51 --once      # replay + minimize one case
```

The generator is written not to emit nondeterministic output, so a reported
divergence is meant to be a real gap — but that is a rule about the generator,
not something the harness can verify: `gen_dataclass` printed a sentinel whose
`repr` carries an object address and produced a permanent false divergence
until it was caught by reading the report. `PYTHONHASHSEED` is pinned to the
same value on both children and **swept** across cases rather than frozen at
`0`, so `str`/`bytes` hashing and string-keyed container order are measured
across the seed axis instead of at one point of it.
`PYTHONRS_FUZZ_PYTHON` names the reference interpreter; a
`--baseline` allowlist keeps known gaps from failing while new ones exit non-zero.
A clean run has to be a run that measured something: cases the reference did not
answer (timed out, exited non-zero, or printed nothing) are reported as `barren`
and excluded from `productive`, and a run that executed no case — or none the
reference answered — exits non-zero instead of printing `divergences : 0`.

## [0x07] STATUS & ROADMAP

Active, in development. The runtime executes a substantial real subset of Python
3. The full CPython standard library is importable by default — the `stdlib-ffi`
bridge (on by default) delegates `import os`/`json`/`random`/… to an embedded
libpython, so only a `--no-default-features` build is limited to the native
module subset. `re` and `itertools` are the exception: they are native in BOTH
builds and never reach CPython. [BUGS.md](BUGS.md) is the honest ledger of remaining gaps. A DAP
debug adapter (`--dap`) — source-line and function breakpoints, stepping, call
stack, locals, and expression `evaluate` — ships today, alongside man pages and
the generated `reference.html`.

## [0x08] DOCUMENTATION

- **Docs site** — <https://menketechnologies.github.io/pythonrs/>
- **Engineering report** — <https://menketechnologies.github.io/pythonrs/report.html>
- **Builtin reference** — <https://menketechnologies.github.io/pythonrs/reference.html>
- **The shared VM** — [`fusevm`](https://github.com/MenkeTechnologies/fusevm), also behind `zshrs`, `strykelang`, `awkrs`, `vimlrs`, `elisprs`, `rubylang`.

## [0xFF] LICENSE

MIT.
