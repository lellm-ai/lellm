//! TraceId / SpanId — 执行追踪标识符。
//!
//! 从 lellm-core 迁移到 lellm-graph，作为执行引擎的一部分。

use uuid::Uuid;

/// Trace ID — 唯一标识一次完整的图执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TraceId(pub Uuid);

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Span ID — 标识一次节点执行的唯一 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SpanId(pub Uuid);

impl Default for SpanId {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
