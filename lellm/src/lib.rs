//! LeLLM — Rust LLM orchestration framework.
//!
//! 默认开启 `provider`（core + provider 适配层）。
//!
//! ```toml
//! # 默认：core + provider
//! lellm = "0.1"
//!
//! # 需要 Agent 运行时
//! lellm = { version = "0.1", features = ["agent"] }
//!
//! # 需要 Graph 编排层
//! lellm = { version = "0.1", features = ["graph"] }
//!
//! # 全部启用
//! lellm = { version = "0.1", features = ["full"] }
//! ```

#[cfg(feature = "core")]
pub use lellm_core as core;

#[cfg(feature = "provider")]
pub use lellm_provider as provider;

#[cfg(feature = "agent")]
pub use lellm_agent as agent;

#[cfg(feature = "macros")]
pub use lellm_derive as macros;

#[cfg(feature = "mcp")]
pub use lellm_mcp as mcp;

#[cfg(feature = "graph")]
pub use lellm_graph as graph;
