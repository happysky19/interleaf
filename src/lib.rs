//! interleaf: an MCP server for LaTeX papers written by people and agents together.
//!
//! The paper lives on Overleaf (through its git bridge) or in any git repository. Agents edit the
//! files in a local clone with their own tools; interleaf syncs the clone, turns edits into
//! suggestions a co-author accepts or rejects, and checks what an agent wrote.
mod cite;
mod config;
mod native;
mod numbers;
mod review;
mod rules;
mod server;
pub mod suggest;
mod sync;
mod tex;
mod text;

pub use native::NativeSession;
pub use server::{INSTRUCTIONS, VERSION, serve, serve_with_native};
