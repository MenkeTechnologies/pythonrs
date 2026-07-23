//! `python --doctor` — a full diagnostic report of the runtime, the embedded
//! CPython bridge, the rkyv bytecode cache, and the environment. Ported from the
//! fleet's zshrs `run_doctor` (`bins/zshrs.rs`), adapted to pythonrs: the shell's
//! FPATH/compsys/daemon sections are replaced by the embedded-CPython and
//! fusevm-runtime sections that matter for a Python interpreter.
//!
//! Everything here is explicit, user-requested output. It never runs on an
//! ordinary invocation.

use super::{bold, cyan, dim, format_bytes, green, red, yellow};
use crate::cache;

/// Print the diagnostic report. Returns a process exit code (0 always — a
/// missing cache or interpreter is reported, not treated as failure).
pub fn run() -> i32 {
    println!("{}", bold("pythonrs doctor"));
    println!("{}", dim(&"=".repeat(60)));
    println!();

    environment();
    embedded_cpython();
    vendored_stdlib();
    runtime();
    bytecode_cache();
    env_vars();
    path_pythons();

    0
}

/// Version, process, host CPU/OS, interpreter stack.
fn environment() {
    println!("{}", bold("Environment"));
    println!("  version:    pythonrs {}", env!("CARGO_PKG_VERSION"));
    println!("  pid:        {}", std::process::id());
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".to_string());
    println!("  cwd:        {cwd}");
    println!(
        "  host:       {} {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    );
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("  cpus:       {cpus}");
    // main.rs runs the interpreter on a 512 MiB worker stack (deep Python frames
    // recurse through a long Rust call chain); report it so a RecursionError
    // that is really a stack limit is diagnosable.
    println!("  vm stack:   512 MiB (interpreter worker thread)");
    println!();
}

/// The embedded libpython used for `import <stdlib>` / `-m module`. Only present
/// in the default `stdlib-ffi` build; a native-only build reports its absence.
#[cfg(feature = "stdlib-ffi")]
fn embedded_cpython() {
    use pyo3::types::PyAnyMethods;

    println!("{}", bold("Embedded CPython (stdlib-ffi)"));
    crate::ffi::init();
    pyo3::Python::with_gil(|py| {
        let version = py.version().replace('\n', " ");
        println!("  linked:      {} {}", green("OK"), cyan(version.trim()));
        let get = |attr: &str| -> Option<String> {
            let sys = py.import("sys").ok()?;
            sys.getattr(attr).ok()?.extract::<String>().ok()
        };
        if let Some(exe) = get("executable") {
            println!("  executable:  {exe}");
        }
        if let Some(prefix) = get("prefix") {
            println!("  prefix:      {prefix}");
        }
        match std::env::var("PYTHONHOME") {
            Ok(h) => println!("  PYTHONHOME:  {h}"),
            Err(_) => println!("  PYTHONHOME:  {}", dim("(unset — system default)")),
        }
    });
    println!();
}

/// Native-only build: no embedded interpreter. `import <stdlib>` is served from
/// the vendored `pylib/` (see `vendored_stdlib`), not libpython; `-m` is absent.
#[cfg(not(feature = "stdlib-ffi"))]
fn embedded_cpython() {
    println!("{}", bold("Embedded CPython (stdlib-ffi)"));
    println!(
        "  {}",
        green("none — CPython-free build; stdlib served from pylib/ (below)")
    );
    println!();
}

/// The vendored CPython pure-Python stdlib (`pylib/`) that pythonrs runs on its
/// own interpreter. Present and used in the native build; shipped-but-inactive in
/// the bridged build (which prefers libpython until the C-accelerator floor lands).
fn vendored_stdlib() {
    println!("{}", bold("Vendored stdlib (pylib/)"));
    let active = cfg!(not(feature = "stdlib-ffi"));
    match locate_pylib() {
        Some(dir) => {
            let count = count_py(&dir);
            println!("  root:        {}", dir.display());
            println!("  modules:     ~{count} .py files");
            println!(
                "  status:      {}",
                if active {
                    green("active — imports run on pythonrs (no libpython)")
                } else {
                    dim("present, inactive — bridged build prefers libpython")
                },
            );
        }
        None => println!(
            "  {}",
            if active {
                red("not found — set $PYTHONRS_LIB or ship pylib/ beside the binary")
            } else {
                yellow("not found (bridged build does not require it)")
            },
        ),
    }
    println!();
}

/// Resolve the `pylib/` root the same way the importer does (`$PYTHONRS_LIB`,
/// install layout beside the binary, then the in-repo tree).
fn locate_pylib() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("PYTHONRS_LIB") {
        let p = std::path::PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [
                dir.join("../lib/pythonrs/pylib"),
                dir.join("../pylib"),
                dir.join("pylib"),
            ] {
                if cand.is_dir() {
                    return Some(cand);
                }
            }
        }
    }
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pylib");
    dev.is_dir().then_some(dev)
}

/// Count `.py` files under a directory tree (best-effort, for the report).
fn count_py(dir: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("py") {
                n += 1;
            }
        }
    }
    n
}

/// The execution engine: fusevm bytecode VM + Cranelift JIT, shared with the
/// rest of the fleet.
fn runtime() {
    println!("{}", bold("Runtime (fusevm)"));
    println!(
        "  engine:      {} + Cranelift JIT",
        cyan("fusevm bytecode VM")
    );
    println!(
        "  {}",
        dim("shared codegen crate — no bespoke VM/JIT in pythonrs")
    );
    println!();
}

/// The rkyv bytecode shard: enable state, path, schema, entry count, size.
fn bytecode_cache() {
    println!("{}", bold("Bytecode cache (rkyv)"));
    let enabled = cache::cache_enabled();
    println!(
        "  enabled:     {}",
        if enabled {
            green("yes")
        } else {
            yellow("no (PYTHONRS_CACHE)")
        }
    );
    let path = cache::default_cache_path();
    println!("  schema:      v{}", cache::schema_version());
    if path.exists() {
        let (count, bytes) = cache::stats();
        println!(
            "  shard:       {} {}  {}",
            path.display(),
            format_bytes(bytes),
            green("OK"),
        );
        println!("  entries:     {count} compiled programs");
        println!("  {}", dim("inspect with: python --cacheview"));
    } else {
        println!(
            "  shard:       {} {}",
            path.display(),
            yellow("(absent — nothing compiled yet)"),
        );
    }
    println!();
}

/// Interpreter-relevant environment variables (`PYTHON*` from CPython, `PYTHONRS_*`
/// from this runtime). Only set ones are shown.
fn env_vars() {
    println!("{}", bold("Environment variables"));
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("PYTHON"))
        .collect();
    vars.sort();
    if vars.is_empty() {
        println!("  {}", dim("(none set)"));
    } else {
        for (k, v) in &vars {
            // PATH-like values can be long; keep the report readable.
            let shown = if v.len() > 68 {
                format!("{}…", &v[..67])
            } else {
                v.clone()
            };
            println!("  {k} = {shown}");
        }
    }
    println!();
}

/// Every `python*` interpreter discoverable on `PATH` (so a version/skew problem
/// between `python`, `python3`, and this binary is visible at a glance).
fn path_pythons() {
    println!("{}", bold("Python interpreters on PATH"));
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut found = 0usize;
    let mut seen: Vec<String> = Vec::new();
    for dir in path_var.split(':').filter(|s| !s.is_empty()) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `python`, `python3`, `python3.12` — but not `pythonX-config` etc.
            let is_interp = name == "python"
                || (name.starts_with("python")
                    && name[6..].chars().all(|c| c.is_ascii_digit() || c == '.'));
            if !is_interp {
                continue;
            }
            let full = entry.path();
            let disp = full.display().to_string();
            if seen.contains(&disp) {
                continue;
            }
            seen.push(disp.clone());
            let exec = full.metadata().map(is_executable).unwrap_or(false);
            println!(
                "  {} {}",
                if exec { green("✓") } else { red("✗") },
                disp,
            );
            found += 1;
        }
    }
    if found == 0 {
        println!("  {}", yellow("(no python* on PATH)"));
    }
    println!();
}

/// Whether a file's mode has any execute bit set. On non-Unix, assume yes.
#[cfg(unix)]
fn is_executable(meta: std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: std::fs::Metadata) -> bool {
    true
}
