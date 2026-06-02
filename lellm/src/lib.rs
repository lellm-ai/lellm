//! LeLLM — Rust LLM orchestration framework.
//!
//! ```toml
//! # 只用 Provider
//! lellm = { version = "0.1", default-features = false, features = ["provider"] }
//!
//! # 启用 Agent
//! lellm = { version = "0.1", features = ["agent"] }
//!
//! # 全部启用
//! lellm = { version = "0.1", features = ["full"] }
//! ```

#[cfg(feature = "provider")]
pub use lellm_provider as provider;

#[cfg(any(feature = "provider", feature = "agent"))]
pub use lellm_core as core;

#[cfg(feature = "agent")]
pub use lellm_agent as agent;

#[cfg(feature = "macros")]
pub use lellm_macros as macros;
