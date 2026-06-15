//! Graph 层流式事件。
//!
//! P2: 流式事件穿透 — Graph 执行过程中实时发射事件，
//! 消费者可通过 channel 观察节点执行与 Agent 内部状态。

use std::time::Duration;

use crate::error::GraphError;
use crate::state::GraphResult;

/// Graph 层流式事件 — 封闭、强类型、exhaustive match。
///
/// 事件流生命周期：
/// - 正常结束：`GraphComplete` 恰好一次，然后 channel 关闭
/// - 异常结束：`GraphError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug)]
pub enum GraphEvent {
    /// 节点开始执行
    NodeStart {
        /// 节点名称
        node_name: String,
    },
    /// 节点执行完成
    NodeEnd {
        /// 节点名称
        node_name: String,
        /// 是否成功
        success: bool,
        /// 执行耗时
        duration: Duration,
    },
    /// Agent 节点内部事件（来自 ToolUseLoop）
    Agent {
        /// AgentNode 名称
        node_name: String,
        /// Agent 层事件
        event: lellm_agent::AgentEvent,
    },
    /// Graph 执行完成（恰好一次）
    GraphComplete {
        /// 执行结果
        result: GraphResult,
    },
    /// Graph 执行出错（恰好一次）
    GraphError {
        /// 错误信息
        error: GraphError,
    },
}

/// Graph 事件通道类型别名
pub type GraphStream = tokio::sync::mpsc::Receiver<GraphEvent>;
