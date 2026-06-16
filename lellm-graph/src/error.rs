//! Graph 错误类型。
//!
//! 错误二分法：
//! - `Terminal` — 终止执行，stream 关闭
//! - `Recoverable` — 内部重试 / fallback，stream 继续
//!
//! 可观测性（Warning/Diagnostic）不属于错误体系，
//! 通过 `GraphEvent::ObservedError` 事件发送。

use std::fmt;

// ─── BuildError ──────────────────────────────────────────────

/// 构建时结构校验错误。
///
/// 仅验证图的结构性正确性，不检测循环、业务逻辑漏洞、运行时 unreachable。
///
/// `Warning` 变体不阻止 build 成功，仅作为诊断信息返回。
#[derive(Debug, Clone)]
pub enum BuildError {
    /// 节点 ID 重复（后者覆盖前者）
    DuplicateNode { id: String },
    /// 边引用了不存在的节点
    MissingNode { from: String, to: String },
    /// 未指定入口节点
    MissingEntryPoint,
    /// 未指定出口节点
    MissingExitPoint,
    /// 边定义无效
    InvalidEdgeDefinition {
        from: String,
        to: String,
        reason: String,
    },
    /// 非致命警告（build 仍成功，但有潜在问题）
    Warning { message: String },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateNode { id } => write!(f, "duplicate node id: '{}'", id),
            Self::MissingNode { from, to } => {
                write!(
                    f,
                    "edge references non-existent node: '{}' (in {}→{})",
                    to, from, to
                )
            }
            Self::MissingEntryPoint => write!(f, "entry point not set"),
            Self::MissingExitPoint => write!(f, "exit point not set"),
            Self::InvalidEdgeDefinition { from, to, reason } => {
                write!(f, "invalid edge {}→{}: {}", from, to, reason)
            }
            Self::Warning { message } => write!(f, "warning: {}", message),
        }
    }
}

impl BuildError {
    /// 是否为非致命警告。
    pub fn is_warning(&self) -> bool {
        matches!(self, Self::Warning { .. })
    }
}

/// 构建错误集合 — 支持多错误收集。
///
/// `errors` 中可能包含 `Warning`（非致命）和其他致命错误。
/// `has_fatal()` 判断是否存在致命错误，`build()` 应在有致命错误时失败。
#[derive(Debug, Clone)]
pub struct BuildErrors(pub Vec<BuildError>);

impl BuildErrors {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(&mut self, e: BuildError) {
        self.0.push(e);
    }

    /// 是否包含致命错误（非 Warning）。
    pub fn has_fatal(&self) -> bool {
        self.0.iter().any(|e| !e.is_warning())
    }

    /// 提取所有致命错误（过滤掉 Warning）。
    pub fn into_errors(self) -> Vec<BuildError> {
        self.0.into_iter().filter(|e| !e.is_warning()).collect()
    }

    /// 提取所有 Warning。
    pub fn warnings(&self) -> Vec<&BuildError> {
        self.0.iter().filter(|e| e.is_warning()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for BuildErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fatal: Vec<_> = self.0.iter().filter(|e| !e.is_warning()).collect();
        let warnings: Vec<_> = self.0.iter().filter(|e| e.is_warning()).collect();

        if !fatal.is_empty() {
            writeln!(f, "{} error(s):", fatal.len())?;
            for e in &fatal {
                writeln!(f, "  - {}", e)?;
            }
        }
        if !warnings.is_empty() {
            writeln!(f, "{} warning(s):", warnings.len())?;
            for e in &warnings {
                writeln!(f, "  - {}", e)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for BuildError {}
impl std::error::Error for BuildErrors {}

// ─── GraphError ──────────────────────────────────────────────

/// Graph 运行时错误 — 二分法。
#[derive(Debug)]
pub enum GraphError {
    /// 终止执行 — stream 关闭，不可恢复
    Terminal(TerminalError),
    /// 可恢复 — 内部重试 / fallback 触发，stream 继续
    Recoverable(RecoverableError),
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
}

/// 可恢复错误 — 内部重试或 fallback 后继续。
#[derive(Debug)]
pub enum RecoverableError {
    /// 节点执行失败但配置了重试
    Retryable {
        node: String,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    /// 边 fallback 被触发
    FallbackTriggered {
        from: String,
        to: String,
        reason: String,
    },
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
            Self::Recoverable(e) => write!(f, "[recoverable] {}", e),
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
        }
    }
}

impl fmt::Display for RecoverableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retryable {
                node,
                attempt,
                max_attempts,
                reason,
            } => {
                write!(
                    f,
                    "node '{node}' retry {}/{}, reason: {}",
                    attempt, max_attempts, reason
                )
            }
            Self::FallbackTriggered { from, to, reason } => {
                write!(f, "fallback edge {}→{} triggered: {}", from, to, reason)
            }
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(TerminalError::NodeExecutionFailed { source, .. }) => {
                Some(source.as_ref())
            }
            _ => None,
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

impl std::error::Error for RecoverableError {}
