//! wavo — an AI agent for music processing, reachable over Telegram.
//!
//! The binary is a thin wiring layer over these modules; they are a library so
//! the guards that matter (schema conversion, the path guard, the tool-calling
//! loop) can be tested without a bot token, an API key or a network.

pub mod config;
pub mod error;
pub mod health;
pub mod jobs;
pub mod llm;
pub mod mcp;
pub mod session;
pub mod telegram;
pub mod tools;
