//! CompiledSubgraph — 编译后的 Subgraph 描述符。
//!
//! # 设计理念
//!
//! ```text
//! Builder 阶段：
//!   SubgraphSpec<Outer, Inner, M, Lens>  (强类型)
//!
//! 编译阶段：
//!   CompiledSubgraph<Outer>  (类型擦除 Inner/Lens/M)
//!
//! Engine 执行：
//!   match node.kind {
//!       NodeKind::Subgraph(spec) => self.execute_subgraph(spec).await,
//!   }
//! ```
//!
//! # 类型擦除
//!
//! SubgraphSpec 有 4 个泛型参数，NodeKind 只有 2 个。
//! CompiledSubgraph 通过 `StateProjector` trait 擦除 Inner/Lens/M，
//! 只保留 Outer（外层 State 类型）。
//!
//! # 与 SubgraphSpec 的区别
//!
//! - SubgraphSpec：Builder 阶段，强类型，包含 Graph + Lens
//! - CompiledSubgraph：编译后，类型擦除，可存入 NodeKind

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::GraphError;
use crate::stream_emitter::StreamSink;
use crate::workflow_state::WorkflowState;
use tokio_util::sync::CancellationToken;

// ─── StateProjector ────────────────────────────────────────────

/// 状态投影器 — 类型擦除的 Outer → Inner 投影 + 执行。
///
/// 这是 Subgraph 执行的核心 trait。它擦除了 Inner、Lens、Merge 类型，
/// 只暴露 Outer State 类型。
///
/// # 设计原则
///
/// - 最小接口：只有 `execute()` + 元数据方法
/// - 类型擦除：Inner/Lens/M 全部隐藏在实现内部
/// - 可 introspection：提供 `graph_name()` 和 `node_count()`
pub trait StateProjector<S: WorkflowState>: Send + Sync {
    /// 执行 Subgraph — 投影状态 + 递归执行内层 Graph。
    fn execute<'a>(
        &'a self,
        outer: &'a mut S,
        stream: Option<Arc<dyn StreamSink>>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphError>> + Send + 'a>>;

    /// 内层 Graph 的名称。
    fn graph_name(&self) -> &str;

    /// 内层 Graph 的节点数（用于评估是否值得内联）。
    fn node_count(&self) -> usize;
}

// ─── CompiledSubgraph ──────────────────────────────────────────

/// 编译后的 Subgraph 描述符 — 可存入 NodeKind。
///
/// # 类型参数
///
/// - `S` — 外层 State 类型（与 NodeKind 一致）
///
/// # 内容
///
/// - `projector` — 类型擦除的执行器（包含 Graph + Lens + Merge）
/// - `max_steps` — 最大执行步数
///
/// # 使用方式
///
/// ```text
/// NodeKind::Subgraph(CompiledSubgraph {
///     projector: Arc::new(spec),  // SubgraphSpec implements StateProjector
///     max_steps: 1000,
/// })
/// ```
#[derive(Clone)]
pub struct CompiledSubgraph<S: WorkflowState> {
    /// 类型擦除的执行器
    pub projector: Arc<dyn StateProjector<S>>,
    /// 最大执行步数
    pub max_steps: usize,
}

impl<S: WorkflowState> CompiledSubgraph<S> {
    /// 创建新的 CompiledSubgraph。
    pub fn new(projector: Arc<dyn StateProjector<S>>, max_steps: usize) -> Self {
        Self {
            projector,
            max_steps,
        }
    }

    /// 执行 Subgraph。
    pub async fn execute(
        &self,
        outer: &mut S,
        stream: Option<Arc<dyn StreamSink>>,
        cancel: CancellationToken,
    ) -> Result<(), GraphError> {
        self.projector.execute(outer, stream, cancel).await
    }

    /// 内层 Graph 的名称。
    pub fn graph_name(&self) -> &str {
        self.projector.graph_name()
    }

    /// 内层 Graph 的节点数。
    pub fn node_count(&self) -> usize {
        self.projector.node_count()
    }
}

impl<S: WorkflowState> std::fmt::Debug for CompiledSubgraph<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledSubgraph")
            .field("graph_name", &self.projector.graph_name())
            .field("node_count", &self.projector.node_count())
            .field("max_steps", &self.max_steps)
            .finish()
    }
}
