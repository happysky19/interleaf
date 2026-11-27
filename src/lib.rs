//! interleaf: an MCP server for LaTeX papers written by people and agents together.
//!
//! The paper lives on Overleaf (through its git bridge) or in any git repository. Agents edit the
//! files in a local clone with their own tools; interleaf syncs the clone, turns edits into
//! suggestions a co-author accepts or rejects, and checks what an agent wrote.
mod cite;
mod config;
pub mod login;
mod native;
mod numbers;
mod review;
mod rules;
mod server;
pub mod suggest;
mod sync;
mod tex;
mod text;

pub use native::{NativeSession, native_operation, native_operations, native_tool_is_mutation};
pub use server::{INSTRUCTIONS, VERSION, serve, serve_native, serve_with_native};
