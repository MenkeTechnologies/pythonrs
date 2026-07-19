//! Ahead-of-time compilation (`python --build`).
//!
//! Precompiles the script to fusevm bytecode, warms the on-disk cache
//! (`cache.rs`) so subsequent runs skip lex/parse/lower, and — via fusevm's `aot`
//! feature (a native-object emitter linked against the pythonrs `staticlib`) —
//! emits a standalone native executable that carries the pythonrs extension-op
//! dispatch and the fusevm AOT runtime. The report below is explicit
//! user-requested output.

/// Precompile `file` to a standalone native executable next to the source, and
/// warm the bytecode cache. Returns a one-line report of what was built.
pub fn build(file: &str) -> Result<String, String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let prog = crate::compile(&src)?;
    let (nfns, nprocs, nops) = (prog.functions.len(), prog.procs.len(), prog.main.ops.len());
    crate::cache::store(&src, &prog)?;

    // Emit the native object + link a standalone executable. The output path is
    // the source stem (`foo.py` -> `foo`).
    let stem = std::path::Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a.out".into());
    let out = std::path::Path::new(file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(&stem);
    crate::aot_native::emit_executable(&prog, &out)?;

    Ok(format!(
        "built {file}: {nops} top-level ops, {nfns} functions, {nprocs} blocks -> {} (+ ~/.pythonrs/scripts.rkyv)",
        out.display()
    ))
}
