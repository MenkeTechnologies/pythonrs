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
#[cfg_attr(feature = "stdlib-ffi", allow(unused_variables))]
pub fn build(file: &str) -> Result<String, String> {
    // A standalone AOT executable cannot statically embed libpython, so the
    // `stdlib-ffi` staticlib (which pulls in pyo3/CPython symbols) will not link.
    // Fail up front with a clear instruction instead of a cryptic linker dump.
    #[cfg(feature = "stdlib-ffi")]
    return Err(
        "--build needs a libpython-free runtime: this pythonrs was built with the \
         stdlib-ffi bridge, whose CPython symbols cannot be statically linked into a \
         standalone binary. Rebuild with `cargo build --no-default-features` (or point \
         PYTHONRS_STATICLIB at a no-default-features libpythonrs.a) and retry."
            .to_string(),
    );
    #[cfg(not(feature = "stdlib-ffi"))]
    build_impl(file)
}

#[cfg(not(feature = "stdlib-ffi"))]
fn build_impl(file: &str) -> Result<String, String> {
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
    // The traceback filename matches what the interpreter would show for this
    // script (its absolute path when resolvable), so an AOT crash reads the same.
    let tb_name = std::fs::canonicalize(file)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file.to_string());
    crate::aot_native::emit_executable(&prog, &src, &tb_name, &out)?;

    Ok(format!(
        "built {file}: {nops} top-level ops, {nfns} functions, {nprocs} blocks -> {} (+ ~/.pythonrs/scripts.rkyv)",
        out.display()
    ))
}
