//! Graph 错误类型。

use std::fmt;

/// Graph 执行错误。
#[derive(Debug)]
pub enum GraphError {
    /// 图结构无效（构建时校验）
    InvalidGraph(String),
    /// 节点不存在
    NodeNotFound(String),
    /// 节点执行失败
    NodeExecutionFailed {
        node: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// 循环超限
    LoopLimitExceeded { limit: usize },
    /// 全局步数超限（运行时熔断）
    StepsExceeded { limit: usize },
    /// Barrier 超时
    BarrierTimeout {
        node: String,
        timeout: std::time::Duration,
    },
    /// Barrier 被取消（消费者丢弃了 channel）
    BarrierCancelled { node: String },
    /// State 操作错误
    StateError(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGraph(msg) => write!(f, "invalid graph: {msg}"),
            Self::NodeNotFound(name) => write!(f, "node not found: {name}"),
            Self::NodeExecutionFailed { node, source } => {
                write!(f, "node '{node}' execution failed: {source}")
            }
            Self::LoopLimitExceeded { limit } => {
                write!(f, "loop limit exceeded: {limit}")
            }
            Self::StepsExceeded { limit } => {
                write!(
                    f,
                    "graph execution halted: step limit {limit} exceeded (potential infinite loop)"
                )
            }
            Self::StateError(msg) => write!(f, "state error: {msg}"),
            Self::BarrierTimeout { node, timeout } => {
                write!(f, "barrier '{node}' timed out after {timeout:?}")
            }
            Self::BarrierCancelled { node } => {
                write!(
                    f,
                    "barrier '{node}' cancelled: consumer dropped the signal channel"
                )
            }
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NodeExecutionFailed { source, .. } => Some(source.as_ref()),
            Self::BarrierCancelled { .. } => None,
            Self::BarrierTimeout { .. } => None,
            _ => None,
        }
    }
}
