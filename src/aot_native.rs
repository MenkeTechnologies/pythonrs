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

use crate::compiler::Program;
use crate::host::{self, FuncDef, TryDef};
use fusevm::VM;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Marker prefix for the embedded program image in `chunk.names`.
const PROG_IMAGE_TAG: &str = "\u{1}pythonrs-prog-image:";

#[derive(serde::Serialize, serde::Deserialize)]
struct ProgImage {
    functions: Vec<FuncDef>,
    tries: Vec<TryDef>,
}

/// Compile a program to a standalone native executable at `out`.
pub fn emit_executable(prog: &Program, out: &Path) -> Result<(), String> {
    let obj = std::env::temp_dir().join("pythonrs_aot.o");
    emit_object(prog, &obj)?;

    let main_c = std::env::temp_dir().join("pythonrs_aot_main.c");
    std::fs::write(
        &main_c,
        "extern long fusevm_aot_run_embedded(void);\n\
         int main(void) { return (int)fusevm_aot_run_embedded(); }\n",
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
fn emit_object(prog: &Program, obj: &Path) -> Result<(), String> {
    let mut chunk = prog.main.clone();
    let image = ProgImage {
        functions: prog.functions.iter().map(|(_, f)| f.clone()).collect(),
        tries: prog.tries.clone(),
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
    host::with_host(|h| {
        for img in images {
            h.load_program(img.functions, img.tries);
        }
    });
}
