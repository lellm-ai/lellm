//! 记忆系统 — Agent Runtime 的组成部分。
//!
//! 提供 ShortTermMemory（环形缓冲）和 LongTermMemory（SQLite 持久化）。

pub mod long_term;
pub mod short_term;
pub mod store;

pub use long_term::LongTermMemory;
pub use short_term::ShortTermMemory;
pub use store::MemoryStore;

use chrono::{DateTime, Utc};

/// 记忆条目的数据类型
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryType {
    ToolCall,
    ToolResult,
    LlmResponse,
    UserInput,
    Decision,
    Summary,
}

impl MemoryType {
    /// 类型权重评分（用于记忆过滤）
    /// Decision(0.9) > UserInput(0.8) > Summary(0.75) > LlmResponse(0.5) > ToolResult(0.4) > ToolCall(0.2)
    pub fn default_score(&self) -> f32 {
        match self {
            MemoryType::Decision => 0.9,
            MemoryType::UserInput => 0.8,
            MemoryType::Summary => 0.75,
            MemoryType::LlmResponse => 0.5,
            MemoryType::ToolResult => 0.4,
            MemoryType::ToolCall => 0.2,
        }
    }
}

/// 记忆条目
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: u64,
    pub content: String,
    pub r#type: MemoryType,
    pub timestamp: DateTime<Utc>,
    pub score: f32,
}
