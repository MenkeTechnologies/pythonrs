//! Native AOT for pythonrs via `fusevm::aot` (`python --build`).
//!
//! Mirrors the elisprs/vimlrs approach: lower the program to a `fusevm::Chunk`,
//! embed the function/try tables (which live on the host, not the chunk) as a
//! JSON image inside `chunk.names`, emit a relocatable object with
//! `fusevm::aot::compile_object`, then link it against the pythonrs runtime
//! staticlib (which carries fusevm's AOT runtime + this module's
//! `fusevm_aot_register_builtins`) and a tiny C entry into a standalone
//! executable.
//!
//! The pythonrs catch is simpler than elisp's: pythonrs chunk constants are
//! native `Value::Str`/`Int`/`Float` only (Python `str`/`list`/… are built at
//! runtime via `MKSTR`/`MKLIST`), so there is no heap image to reconstruct — the
//! only host state a chunk depends on is the function/try tables, which the
//! image below restores before the main chunk runs.

use crate::ast::Span;
use crate::compiler::Program;
use crate::host::{self, FuncDef, TryDef};
use fusevm::{Chunk, VM};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Recompute a chunk's `op_hash` the way `fusevm::ChunkBuilder::build` does (a
/// `DefaultHasher` over ops then constants). `op_hash` is `#[serde(skip)]`, so a
/// bincode-deserialized AOT chunk carries `0`; restoring it (the pre-rebase ops
/// match the compile-time hash) lets caret lookups by `op_hash` hit.
fn restore_op_hash(chunk: &mut Chunk) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    chunk.ops.hash(&mut h);
    chunk.constants.hash(&mut h);
    chunk.op_hash = h.finish();
    for sub in &mut chunk.sub_chunks {
        restore_op_hash(sub);
    }
}

/// Marker prefix for the embedded program image in `chunk.names`.
const PROG_IMAGE_TAG: &str = "\u{1}pythonrs-prog-image:";

#[derive(serde::Serialize, serde::Deserialize)]
struct ProgImage {
    functions: Vec<FuncDef>,
    tries: Vec<TryDef>,
    /// The program's full source text and its display filename, so the AOT binary
    /// renders an uncaught exception's traceback (source lines + carets) exactly
    /// like the interpreter.
    #[serde(default)]
    source: String,
    #[serde(default)]
    filename: String,
    /// Traceback-caret op-index → span tables, keyed by chunk `op_hash` — the same
    /// set the interpreter registers at run time (see `compiler::Program`).
    #[serde(default)]
    positions: Vec<(u64, Vec<Span>)>,
}

/// Compile a program to a standalone native executable at `out`. `src`/`filename`
/// are embedded so the binary can render tracebacks.
pub fn emit_executable(prog: &Program, src: &str, filename: &str, out: &Path) -> Result<(), String> {
    let obj = std::env::temp_dir().join("pythonrs_aot.o");
    emit_object(prog, src, filename, &obj)?;

    let main_c = std::env::temp_dir().join("pythonrs_aot_main.c");
    std::fs::write(
        &main_c,
        // `pythonrs_aot_run_embedded` (below) wraps fusevm's runner so an uncaught
        // pythonrs exception (set on the host, not returned as a native VM error)
        // renders a proper traceback and exits non-zero.
        "extern long pythonrs_aot_run_embedded(void);\n\
         int main(void) { return (int)pythonrs_aot_run_embedded(); }\n",
    )
    .map_err(|e| e.to_string())?;

    let lib = staticlib_path()?;
    let mut cmd = std::process::Command::new("cc");
    cmd.arg(&main_c).arg(&obj).arg(&lib).arg("-o").arg(out);
    if cfg!(target_os = "macos") {
        cmd.args([
            // NOTE: the linker prints a benign "no platform load command found in
            // <aot>.o, assuming: macOS" — the cranelift-object `.o` emitted by
            // fusevm::aot has no LC_BUILD_VERSION. It is cosmetic (the executable
            // links and runs correctly). The real fix is upstream in fusevm::aot
            // (stamp the Mach-O platform at object emission); a pythonrs-side
            // -Wl,-platform_version only introduces conflicting-version warnings,
            // so we deliberately do NOT pass one here.
            "-framework",
            "CoreFoundation",
            "-framework",
            "Security",
            "-liconv",
            "-lc++",
        ]);
    } else {
        cmd.args(["-lpthread", "-ldl", "-lm", "-lrt"]);
    }
    let status = cmd.status().map_err(|e| format!("cc: {e}"))?;
    if !status.success() {
        return Err(format!("link failed (cc exit {:?})", status.code()));
    }
    Ok(())
}

/// Emit just the relocatable AOT object (embedding the program image).
fn emit_object(prog: &Program, src: &str, filename: &str, obj: &Path) -> Result<(), String> {
    let mut chunk = prog.main.clone();
    let image = ProgImage {
        functions: prog.functions.iter().map(|(_, f)| f.clone()).collect(),
        tries: prog.tries.clone(),
        source: src.to_string(),
        filename: filename.to_string(),
        positions: prog.positions.clone(),
    };
    let json = serde_json::to_string(&image).map_err(|e| e.to_string())?;
    chunk.names.push(format!("{PROG_IMAGE_TAG}{json}"));
    fusevm::aot::compile_object(&chunk, obj).map_err(|e| format!("pythonrs --build: {e}"))
}

/// Locate `libpythonrs.a` (a sibling of the running `python` binary, or
/// `$PYTHONRS_STATICLIB`).
fn staticlib_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PYTHONRS_STATICLIB") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let lib = exe.parent().ok_or("no exe dir")?.join("libpythonrs.a");
    if lib.exists() {
        Ok(lib)
    } else {
        Err(format!(
            "libpythonrs.a not found next to {}; build the staticlib or set PYTHONRS_STATICLIB",
            exe.display()
        ))
    }
}

/// The AOT runtime hook: install the pythonrs builtins + numeric hook and reload
/// the embedded function/try tables before the main chunk runs. Required link
/// symbol for a standalone pythonrs AOT binary.
///
/// # Safety
/// `vm` must be a valid, exclusively-borrowable pointer (fusevm's AOT entry
/// passes one).
#[no_mangle]
pub unsafe extern "C" fn fusevm_aot_register_builtins(vm: *mut VM) {
    let vm = unsafe { &mut *vm };
    crate::builtins::install(vm);
    vm.set_numeric_hook(Arc::new(crate::builtins::numeric_hook));
    let images: Vec<ProgImage> = vm
        .chunk
        .names
        .iter()
        .filter_map(|n| n.strip_prefix(PROG_IMAGE_TAG))
        .filter_map(|j| serde_json::from_str(j).ok())
        .collect();
    for mut img in images {
        // Restore the source/filename so a traceback shows its `File`/source
        // lines, register the caret position tables, then install the
        // function/try tables the main chunk calls into.
        let has_file = !img.filename.is_empty();
        host::init_runtime(
            vec![if has_file {
                img.filename.clone()
            } else {
                String::new()
            }],
            has_file.then(|| img.filename.clone()),
            &img.source,
            if has_file { &img.filename } else { "<string>" },
            has_file && !img.filename.starts_with('<'),
        );
        for (op_hash, table) in img.positions {
            host::register_positions(op_hash, table);
        }
        // Function/try bodies deserialize with `op_hash == 0` (serde-skipped); the
        // caret registry is keyed by the real hash, so restore it on each.
        for f in &mut img.functions {
            restore_op_hash(&mut f.chunk);
        }
        for t in &mut img.tries {
            restore_op_hash(&mut t.body);
            for (typ, _, handler) in &mut t.handlers {
                if let Some(typ) = typ {
                    restore_op_hash(typ);
                }
                restore_op_hash(handler);
            }
            if let Some(e) = &mut t.orelse {
                restore_op_hash(e);
            }
            if let Some(fb) = &mut t.finalbody {
                restore_op_hash(fb);
            }
        }
        host::with_host(|h| h.load_program(img.functions, img.tries));
    }
}

/// The AOT entry the emitted `main` calls: run the embedded chunk, then surface
/// an uncaught pythonrs exception the way the interpreter does — a rendered
/// traceback (with carets) to stderr and a non-zero exit — rather than fusevm's
/// generic runner, which only reports NATIVE VM errors and silently drops a
/// pythonrs error left on the host.
///
/// # Safety
/// Relies on the fusevm-emitted `fusevm_aot_chunk_blob`/`_len`/`fusevm_aot_entry`
/// link symbols, exactly as `fusevm::aot::fusevm_aot_run_embedded` does.
#[no_mangle]
pub extern "C" fn pythonrs_aot_run_embedded() -> i64 {
    #[allow(improper_ctypes)]
    extern "C" {
        static fusevm_aot_chunk_blob: u8;
        static fusevm_aot_chunk_len: u64;
        fn fusevm_aot_entry(vm: *mut VM) -> i64;
    }
    let mut chunk: Chunk = unsafe {
        let len = fusevm_aot_chunk_len as usize;
        let bytes = std::slice::from_raw_parts(&fusevm_aot_chunk_blob as *const u8, len);
        match bincode::deserialize(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("pythonrs aot: corrupt embedded chunk: {e}");
                return 1;
            }
        }
    };
    // The deserialized main chunk lost its `op_hash` (serde-skipped); restore it so
    // an error's caret span is found in the registered position table.
    restore_op_hash(&mut chunk);
    let mut vm = VM::new(chunk);
    // SAFETY: pointer is valid for the call; the hook borrows it exclusively.
    unsafe { fusevm_aot_register_builtins(&mut vm as *mut VM) };
    // SAFETY: the compiled entry has the declared C ABI and reads `vm`.
    unsafe { fusevm_aot_entry(&mut vm as *mut VM) };

    // A pythonrs error (`IndexError`, `TypeError`, `NameError`, `sys.exit`, …) is
    // left on the host by `builtins::abort`; render it like the interpreter (a
    // traceback with carets, or a `SystemExit` code/message). fusevm's own runner
    // reports only NATIVE VM errors and drops this, exiting 0 silently.
    match host::with_host(|h| h.take_error()) {
        None => 0,
        Some(e) => match host::classify_top_error(&e) {
            host::TopExit::SystemExit { code, message } => {
                if let Some(msg) = message {
                    eprint!("{msg}");
                }
                code as i64
            }
            host::TopExit::Uncaught { traceback } => {
                eprint!("{traceback}");
                1
            }
        },
    }
}
