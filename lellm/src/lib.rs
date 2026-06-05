//! LeLLM — Rust LLM orchestration framework.
//!
//! 所有 feature 均需显式开启（`default = []`）：
//!
//! ```toml
//! # 仅协议对象（零运行时依赖）
//! lellm = { version = "0.1", features = ["core"] }
//!
//! # 协议 + Provider 适配层
//! lellm = { version = "0.1", features = ["provider"] }
//!
//! # 协议 + Provider + Agent 运行时
//! lellm = { version = "0.1", features = ["agent"] }
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
pub use lellm_macros as macros;
