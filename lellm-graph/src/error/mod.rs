//! Graph 错误类型。
//!
//! 错误模型：
//! - `Terminal` — 终止执行，stream 关闭
//! - Fallback — 控制流（通过 `StreamNodeResult::Fallback`），非错误
//! - 可观测性 — 通过 `GraphEvent::ObservedError` 事件发送
//!
//! `build()` = 结构正确性校验（纯函数，只产生 BuildError）
//! `analyze()` = 风险诊断（产生 GraphDiagnostics）

use std::fmt;

mod build_error;

pub use build_error::*;

// ─── GraphError ──────────────────────────────────────────────

/// Graph 运行时错误。
///
/// 只有 Terminal 变体 — Fallback 改为控制流（`StreamNodeResult::Fallback`）。
#[derive(Debug)]
pub enum GraphError {
    /// 终止执行 — stream 关闭，不可恢复
    Terminal(TerminalError),
}

/// 终止错误 — Graph 执行不可恢复地停止。
#[derive(Debug)]
pub enum TerminalError {
    /// 图结构无效（构建时校验遗漏的运行时问题）
    InvalidGraph(String),
    /// 节点不存在
    NodeNotFound(String),
    /// Goto 目标缺少对应的边
    MissingEdge { from: String, to: String },
    /// 节点执行失败（不可恢复）
    NodeExecutionFailed {
        node: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// 全局步数超限（运行时熔断）
    StepsExceeded { limit: usize },
    /// 循环超限
    LoopLimitExceeded { limit: usize },
    /// Barrier 超时
    BarrierTimeout {
        node: String,
        timeout: std::time::Duration,
    },
    /// Barrier 被取消
    BarrierCancelled { node: String },
    /// 无匹配边 — 没有任何 outgoing edge 满足条件，且无 fallback
    Unrouted {
        /// 当前节点
        node: String,
        /// 尝试的条件及其结果
        attempted_conditions: Vec<ConditionEval>,
    },
    /// State 操作错误
    StateError(String),
    /// 检查点同步保存失败 — 执行在该边界停止
    CheckpointSaveFailed { error: String },
    /// 持久化/恢复入口遇到不支持的图结构（Parallel/Subgraph/Barrier）
    RestoreUnsupported { node: String, kind: String },
    /// 传入的检查点不是该 trace 的最新检查点 — 拒绝续写原 trace（避免历史分叉）
    RestoreNotLatest { checkpoint: String, latest: String },
    /// 恢复前置条件失败（如加载最新检查点时损坏/缺失）
    RestoreFailed { reason: String },
}

/// 可观测性事件 — 不属于错误体系，通过 GraphEvent 发送。
#[derive(Debug, Clone)]
pub enum ObservedError {
    /// 警告
    Warning { node: String, message: String },
    /// 降级执行
    Degraded { node: String, message: String },
    /// 部分失败
    PartialFailure {
        node: String,
        succeeded: usize,
        failed: usize,
        message: String,
    },
}

impl fmt::Display for ObservedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warning { node, message } => write!(f, "node '{}': {}", node, message),
            Self::Degraded { node, message } => write!(f, "node '{}' degraded: {}", node, message),
            Self::PartialFailure {
                node,
                succeeded,
                failed,
                message,
            } => {
                write!(
                    f,
                    "node '{}' partial: {}/{} ok, {}",
                    node,
                    succeeded,
                    succeeded + failed,
                    message
                )
            }
        }
    }
}

/// 条件评估结果 — 用于 Unrouted 错误报告。
#[derive(Debug, Clone)]
pub struct ConditionEval {
    /// 边描述
    pub edge: String,
    /// 条件描述（None = default edge）
    pub condition: Option<String>,
    /// 评估结果
    pub matched: bool,
}

// ─── Display ─────────────────────────────────────────────────

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Terminal(e) => write!(f, "[terminal] {}", e),
        }
    }
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGraph(msg) => write!(f, "invalid graph: {msg}"),
            Self::NodeNotFound(name) => write!(f, "node not found: {name}"),
            Self::MissingEdge { from, to } => {
                write!(
                    f,
                    "goto '{}' from '{}' failed: no edge {}→{} exists",
                    to, from, from, to
                )
            }
            Self::NodeExecutionFailed { node, source } => {
                write!(f, "node '{node}' execution failed: {source}")
            }
            Self::StepsExceeded { limit } => {
                write!(f, "step limit {limit} exceeded (potential infinite loop)")
            }
            Self::LoopLimitExceeded { limit } => write!(f, "loop limit exceeded: {limit}"),
            Self::BarrierTimeout { node, timeout } => {
                write!(f, "barrier '{node}' timed out after {timeout:?}")
            }
            Self::BarrierCancelled { node } => {
                write!(
                    f,
                    "barrier '{node}' cancelled: consumer dropped the signal channel"
                )
            }
            Self::Unrouted {
                node,
                attempted_conditions,
            } => {
                write!(f, "node '{}' has no matching outgoing edge", node)?;
                if !attempted_conditions.is_empty() {
                    write!(f, ". evaluated: [")?;
                    for (i, ce) in attempted_conditions.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}={}", ce.edge, ce.matched)?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }
            Self::StateError(msg) => write!(f, "state error: {msg}"),
            Self::CheckpointSaveFailed { error } => {
                write!(f, "checkpoint save failed: {error}")
            }
            Self::RestoreUnsupported { node, kind } => write!(
                f,
                "persistence/restore does not support {kind} node '{node}' (phase 1: serial + loops only)"
            ),
            Self::RestoreNotLatest { checkpoint, latest } => write!(
                f,
                "checkpoint {checkpoint} is not the latest of this trace (latest: {latest}); restore requires the latest checkpoint or a new trace"
            ),
            Self::RestoreFailed { reason } => write!(f, "restore precondition failed: {reason}"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(TerminalError::NodeExecutionFailed { source, .. }) => {
                Some(source.as_ref())
            }
            Self::Terminal(_) => None,
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NodeExecutionFailed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}
