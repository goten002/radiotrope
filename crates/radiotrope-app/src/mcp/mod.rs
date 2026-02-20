//! MCP stdio server
//!
//! Manual implementation of the Model Context Protocol over stdin/stdout.
//! No async runtime — runs on a plain std::thread.

pub mod server;
pub mod tools;
pub mod types;
