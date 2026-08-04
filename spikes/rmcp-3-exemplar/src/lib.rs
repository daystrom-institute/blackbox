//! Standalone rmcp 3.1 exemplar for the MCP 2026-07-28 migration mechanics.

pub mod client;
pub mod server;

pub use client::{DemoClient, DemoClientHandler};
pub use server::{DemoServer, RunningDemoServer, Scope};
