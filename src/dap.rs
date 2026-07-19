//! Debug Adapter Protocol server (`python --dap`) — minimal.
//!
//! The full DAP surface (breakpoints, stepping, variable inspection over the
//! statement line markers the `--dap` compile mode emits) is a later wave; this
//! module provides the `on_ext` line-marker hook consumed by the host's debug
//! run path and a stdio server stub that speaks the initialize handshake so an
//! editor can attach without erroring. See BUGS.md.

use fusevm::VM;

/// Extension-handler callback invoked for `Op::Extended`/`CallBuiltin(DBG_LINE)`
/// markers while `--dap` debug mode is active. A no-op until stepping lands.
pub fn on_ext(_vm: &mut VM, _id: u16) {}

/// Run the DAP server over stdio.
pub fn run() -> Result<(), String> {
    Err("the DAP server is not yet implemented (see BUGS.md); use --repl or run the script directly".into())
}
