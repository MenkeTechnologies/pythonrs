# pythonrs

**Python as a fusevm frontend** — a lexer/parser and compiler that lowers Python 3
to `fusevm::Chunk` bytecode running on the fusevm three-tier Cranelift JIT, over a
`PyHost` object heap. There is no bespoke VM and no bespoke JIT: pythonrs is a pure
front end; execution and codegen live in [fusevm](https://github.com/MenkeTechnologies/fusevm).

It is, to our knowledge, the first compiled standalone Python runtime that both
**transparently caches bytecode via rkyv on every run** and **AOT-compiles a script
to a native executable**.

## Headline features

- **Compiled, JIT-traced execution.** Python lowers to fusevm bytecode; hot loops
  trace-compile through Cranelift. Arithmetic and comparisons lower to native ops;
  Python-specific behavior (truthiness, str/list concat, bignum promotion, method
  dispatch) runs through a strict numeric hook and builtin dispatch.
- **Transparent rkyv bytecode cache.** Every `python foo.py` run hashes the source,
  consults `~/.pythonrs/scripts.rkyv`, and on a hit runs the compiled chunks
  directly — lex/parse/lower are skipped entirely. On a miss it compiles, stores,
  and runs. No flags required.
- **AOT to a native executable.** `python --build foo.py` emits a standalone native
  binary (via `fusevm::aot`, linked against the pythonrs runtime staticlib) that
  runs the script with no interpreter present.
- **Arbitrary-precision integers**, real closures, classes with inheritance,
  comprehensions, f-strings, exceptions, `*args`/`**kwargs`.

## Usage

```
python foo.py              # run a script (transparently rkyv-cached)
python -c 'print(1 + 1)'   # run a one-liner
python --build foo.py      # AOT-compile to a native ./foo executable
python --dump-bytecode f.py# print the lowered fusevm bytecode
python --repl              # interactive REPL
python --lsp               # Language Server Protocol over stdio
```

Set `PYTHONRS_TRACE=1` to log cache hit/miss to stderr (silent otherwise).

## Architecture

```
lexer  →  parser  →  AST  →  compiler  →  fusevm::Chunk  →  fusevm VM + JIT
                                              │                    │
                                              └── CallBuiltin ─────→ host (PyHost heap)
```

- `lexer.rs` — indentation-significant tokenizer (INDENT/DEDENT/NEWLINE, f-strings).
- `parser.rs` — recursive-descent Python grammar → `ast.rs`.
- `compiler.rs` — lowers the AST to fusevm ops + `CallBuiltin` dispatches.
- `host.rs` — the PyHost object heap (str/list/dict/tuple/set/instances/…), the
  operator/attribute/item/iteration semantics, and the fusevm run plumbing.
- `builtins.rs` — the `CallBuiltin` handler table, the numeric hook, the Kernel
  builtin functions (`print`/`len`/`range`/…), and per-type methods.
- `cache.rs` — the rkyv-shard bytecode cache.
- `aot_native.rs` — native-executable emission via `fusevm::aot`.

## Status

Runs a large, real subset of Python 3, verified byte-for-byte against CPython on
the example corpus. See [BUGS.md](BUGS.md) for the honest list of unimplemented
features (generators, `async`, `match`, operator dunders, most of the stdlib).

## Building

```
cargo build            # debug build: target/debug/python + libpythonrs.a
```

## License

MIT
