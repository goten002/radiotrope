//! MCP server (`radiotrope --mcp`)
//!
//! Lets AI agents drive the player over the Model Context Protocol. The
//! protocol side is rmcp's; this module holds the tools and the transport.

pub mod agents;
pub mod local;
pub mod network;
pub mod server;
pub mod tools;

#[cfg(test)]
mod tests;
