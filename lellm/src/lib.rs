//! LeLLM — Rust LLM orchestration framework.
//!
//! ```toml
//! # 默认启用 provider
//! lellm = "0.1"
//!
//! # 或按需加载
//! lellm = { version = "0.1", features = ["provider", "agent"] }
//! ```

#[cfg(feature = "provider")]
pub use lellm_provider as provider;

#[cfg(any(feature = "provider", feature = "agent"))]
pub use lellm_core as core;

#[cfg(feature = "agent")]
pub use lellm_agent as agent;

#[cfg(feature = "macros")]
pub use lellm_macros as macros;
