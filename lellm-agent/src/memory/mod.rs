//! 记忆系统 — Agent Runtime 的组成部分。
//!
//! v0.1 仅提供 ShortTermMemory（基于 VecDeque 的对话历史窗口）。

pub mod short_term;

pub use short_term::ShortTermMemory;
