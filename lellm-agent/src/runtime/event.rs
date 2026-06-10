//! Agent 层流式事件与停止原因。

/// Agent 层流式事件 — 封闭、强类型、exhaustive match
///
/// 终态契约：
/// - 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
/// - 异常结束：`LoopError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug)]
pub enum AgentEvent {
    /// Provider 层事件
    Provider(lellm_provider::ProviderEvent),
    /// 工具开始执行
    ToolStart { tool_call_id: String, name: String },
    /// 工具执行完成
    ToolEnd {
        tool_call_id: String,
        result: super::ToolResult,
    },
    /// 工具重试（v0.2 RetryPolicy 事件化）
    #[cfg(feature = "v02-preview")]
    Retry {
        #[allow(dead_code)]
        tool_call_id: String,
        #[allow(dead_code)]
        attempt: usize,
        #[allow(dead_code)]
        max_attempts: usize,
        #[allow(dead_code)]
        reason: String,
    },
    /// 上下文压缩完成（可观测性事件）
    ContextCompacted {
        before_tokens: usize,
        after_tokens: usize,
        removed_messages: usize,
    },
    /// Agent loop 正常结束（恰好一次，后不再发送）
    LoopEnd { result: super::ToolUseResult },
    /// Agent loop 异常结束（恰好一次，后不再发送）— 不含 messages，消费者自行维护
    LoopError {
        error: lellm_core::LlmError,
        iterations: usize,
    },
}

/// Agent loop 停止原因 — 描述"为什么停止"，而非"响应长什么样"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Agent 已获得最终答案并正常结束
    Complete,
    /// 达到最大轮次
    MaxIterationsReached,
    /// 外部取消（消费者断开、task 终止等）
    Cancelled,
    /// 输出预算超限（单轮或总输出 token 超过限制）
    OutputBudgetExceeded,
    /// 推理预算超限（thinking token 超过限制）
    ReasoningBudgetExceeded,
    // NOTE: LoopDetected 将在 v0.2 LoopDetector 实现时加回
}

/// Agent 层流式事件通道类型别名
pub type AgentStream = tokio::sync::mpsc::Receiver<AgentEvent>;
