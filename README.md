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

It is, to our knowledge, the first compiled standalone Python runtime that both
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
promotion, attribute and method dispatch — runs through a strict numeric hook and
a numbered builtin-call protocol into the `PyHost` object heap.

Two things set it apart from every other standalone Python:

- **Transparent rkyv bytecode cache — on every run.** `python foo.py` hashes the
  source, consults `~/.pythonrs/scripts.rkyv`, and on a hit runs the compiled
  chunks directly with lex/parse/lower skipped entirely. No flags, no separate
  build step, no `__pycache__` ritual.
- **AOT to a native executable.** `python --build foo.py` emits a standalone
  native binary (via `fusevm::aot`, linked against the pythonrs runtime
  staticlib) that runs the script with no interpreter present.

## [0x01] INSTALL

```sh
# Via the Homebrew tap (bumped by each release; formula is `pythonrs`)
brew install menketechnologies/menketech/pythonrs

# Or from source
git clone https://github.com/MenkeTechnologies/pythonrs
cd pythonrs && cargo build --release
# binary: target/release/python  (+ libpythonrs.a for AOT linking)
```

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
python --repl               # interactive REPL
python --lsp                # Language Server Protocol over stdio
```

Set `PYTHONRS_TRACE=1` to log cache hit/miss to stderr (silent otherwise).

## [0x03] LANGUAGE FEATURES

Arbitrary-precision integers, real closures, classes with inheritance,
comprehensions (list/dict/set), f-strings, exceptions, and `*args` / `**kwargs`.
The `PyHost` heap implements the `str` / `list` / `dict` / `tuple` / `set` /
instance object model with the operator, attribute, item, and iteration
protocols. See [\[0x07\]](#0x07-status--roadmap) and [BUGS.md](BUGS.md) for the
honest list of what is not yet implemented.

## [0x04] COMMAND-LINE FLAGS

| Flag | Effect |
|---|---|
| *(none)* | Run the script/one-liner, transparently rkyv-cached. |
| `--build` | AOT-compile the script to a standalone native executable. |
| `--dump-bytecode` | Print the lowered `fusevm` bytecode and exit. |
| `--repl` | Start the interactive REPL. |
| `--lsp` | Run the Language Server Protocol server over stdio. |

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

## [0x06] PARITY HARNESS

Correctness is measured, not asserted: an example corpus runs through both
pythonrs and the reference `python3`, and the output is diffed byte-for-byte.
pythonrs runs a large, real subset of Python 3, verified against CPython on that
corpus.

## [0x07] STATUS & ROADMAP

Active, in development. The runtime executes a substantial real subset of Python
3. [BUGS.md](BUGS.md) is the honest ledger of unimplemented features — generators,
`async`, `match`, operator dunders, and most of the standard library are not yet
carried. A DAP debug adapter (`--dap`), man pages, and the generated
`reference.html` land alongside the growing builtin surface.

## [0x08] DOCUMENTATION

- **Docs site** — <https://menketechnologies.github.io/pythonrs/>
- **Engineering report** — <https://menketechnologies.github.io/pythonrs/report.html>
- **Builtin reference** — <https://menketechnologies.github.io/pythonrs/reference.html>
- **The shared VM** — [`fusevm`](https://github.com/MenkeTechnologies/fusevm), also behind `zshrs`, `strykelang`, `awkrs`, `vimlrs`, `elisprs`, `rubylang`.

## [0xFF] LICENSE

MIT.
