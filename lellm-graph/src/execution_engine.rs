//! ExecutionEngine — 执行引擎核心类型。
//!
//! 职责分离：
//! - `ExecutionEngine<S>` — Executor 内部拥有，持有 State、Mutation 缓冲、流发射器等
//! - `NodeContext<'a, S>` / `LeafContext<'a, S>` — 节点能力视图（在 node_context.rs 中）
//!
//! 数据流单向：
//!
//! ```text
//! Node
//!   ↓
//! ctx.record(Mutation)
//!   ↓
//! Mutation Buffer (ExecutionEngine)
//!   ↓
//! Engine: take_mutations()
//!   ↓
//! state.apply_batch(mutations)
//!   ↓
//! State
//! ```
//!
//! 节点只能通过 `record()` 声明变更意图，无法直接修改 State。
//! 这保证了 Mutation Log 是唯一写入口，使 Replay、Trace、Parallel Merge、Undo 全部成立。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::event::FlowEvent;
use crate::stream_chunk::StreamChunk;
use crate::stream_emitter::StreamSink;
use crate::workflow_state::WorkflowState;

// ─── ExecutionSignal ──────────────────────────────────────────

/// 控制信号 — 独立枚举，Barrier 挂起不是路由。
#[derive(Debug, Clone)]
pub enum ExecutionSignal {
    /// Barrier 挂起执行
    Pause {
        barrier_id: crate::event::BarrierId,
        timeout: Option<std::time::Duration>,
    },
}

// ─── NextAction ────────────────────────────────────────────────

/// 节点执行后的下一步路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// 按拓扑顺序走下一步（默认值）
    Next,
    /// 跳转到指定节点
    Goto(String),
    /// 结束执行
    End,
}

// ─── ExecutionControl ─────────────────────────────────────────

/// 控制信号容器 — 节点写入，Executor 读取。
#[derive(Debug, Default)]
pub struct ExecutionControl {
    next: Option<NextAction>,
    signal: Option<ExecutionSignal>,
}

impl ExecutionControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// 跳转到指定节点。
    pub fn goto(&mut self, target: impl Into<String>) {
        self.next = Some(NextAction::Goto(target.into()));
    }

    /// 结束执行。
    pub fn end(&mut self) {
        self.next = Some(NextAction::End);
    }

    /// Barrier 挂起。
    pub fn pause(
        &mut self,
        barrier_id: crate::event::BarrierId,
        timeout: Option<std::time::Duration>,
    ) {
        self.signal = Some(ExecutionSignal::Pause {
            barrier_id,
            timeout,
        });
    }

    /// 获取最终的控制信号。
    pub fn take(&mut self) -> (NextAction, Option<ExecutionSignal>) {
        let next = self.next.take().unwrap_or(NextAction::Next);
        let signal = self.signal.take();
        (next, signal)
    }
}

// ─── NodeMetadata ─────────────────────────────────────────────

/// 节点元数据 — 提供给 Executor 的额外信息。
#[derive(Debug, Clone, Default)]
pub struct NodeMetadata {
    /// Token 消耗成本（0.0 表示无 LLM 调用）
    pub token_cost: f64,
    /// 是否有外部副作用（如部署、发送消息）
    pub has_side_effects: bool,
}

// ─── ExecutionView trait ──────────────────────────────────────

/// 受限视图 — Leaf 节点需要的最小能力。
pub trait ExecutionView<S: WorkflowState>: Send + Sync {
    fn state(&self) -> &S;
    fn emit(&self, chunk: StreamChunk);
    fn is_cancelled(&self) -> bool;
}

// ─── ExecutorState trait ──────────────────────────────────────

/// 完整能力 — Composite 节点 + LeafAdapter 使用。
///
/// # 注意
///
/// 此 trait **不是 dyn compatible**（`build_node_context` 返回带生命周期的 `NodeContext`，
/// `apply_batch` 使用泛型）。仅用于静态分发（`T: ExecutorState<S>`），不用于 `dyn ExecutorState<S>`。
pub trait ExecutorState<S: WorkflowState>: ExecutionView<S> {
    fn build_node_context(&mut self) -> crate::node_context::NodeContext<'_, S>;
    fn build_leaf_context(&mut self) -> crate::node_context::LeafContext<'_, S>;
    fn clone_state(&self) -> S;
    fn replace_state(&mut self, state: S);
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>);
    fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>);
    fn take_metadata(&mut self) -> NodeMetadata;
    fn take_flow_events(&mut self) -> Vec<FlowEvent>;
    /// 发射控制面 FlowEvent（Composite 节点如 ParallelNode 需要）。
    fn emit_flow_event(&mut self, event: FlowEvent);
}

// ─── ExecutionEngine ──────────────────────────────────────────

/// 执行引擎 — 拥有所有可变状态，替代 ExecutionContext。
///
/// 不对节点开发者公开。节点通过 [`NodeContext`](crate::node_context::NodeContext)
/// 或 [`LeafContext`](crate::node_context::LeafContext) 能力视图交互。
///
/// # 三层 API
///
/// - **Leaf Execution API**: `build_leaf_context()` — 构建只读 + emit 视图
/// - **Composite Execution API**: `clone_state()`, `replace_state()` — 执行控制
/// - **Runtime Control Plane**: `stream`, `cancel`, `commit()` — 运行时管理
pub struct ExecutionEngine<S: WorkflowState> {
    /// 类型化状态 — Engine 独占写权限
    state: S,
    /// 数据面发射器 — 可选（阻塞模式 = None）。使用 Arc 以便 Parallel 子分支 clone。
    stream: Option<Arc<dyn StreamSink>>,
    /// 取消令牌 — 消费者断开时触发
    cancel: CancellationToken,
    /// 控制信号 — 节点写入，Executor 读取
    control: ExecutionControl,
    /// 节点元数据 — 节点写入
    metadata: NodeMetadata,
    /// Mutation 缓冲 — 节点产生的强类型领域事件
    mutations: Vec<S::Mutation>,
    /// FlowEvent 缓冲 — 节点产生的控制面事件
    flow_events: Vec<FlowEvent>,
}

impl<S: WorkflowState> ExecutionEngine<S> {
    /// 创建新的 ExecutionEngine。
    pub fn new(state: S, stream: Option<Arc<dyn StreamSink>>, cancel: CancellationToken) -> Self {
        Self {
            state,
            stream,
            cancel,
            control: ExecutionControl::new(),
            metadata: NodeMetadata::default(),
            mutations: Vec::new(),
            flow_events: Vec::new(),
        }
    }

    // ─── Executor API ─────────────────────────────────────────

    /// 消费 Mutation 缓冲（Executor 调用）。
    pub fn take_mutations(&mut self) -> Vec<S::Mutation> {
        std::mem::take(&mut self.mutations)
    }

    /// 消费控制信号（Executor 调用）。
    pub fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>) {
        self.control.take()
    }

    /// 获取元数据（Executor 调用）。
    pub fn take_metadata(&mut self) -> NodeMetadata {
        std::mem::take(&mut self.metadata)
    }

    /// 消费 FlowEvent 缓冲（Executor 调用）。
    pub fn take_flow_events(&mut self) -> Vec<FlowEvent> {
        std::mem::take(&mut self.flow_events)
    }

    /// 获取状态引用。
    pub fn state(&self) -> &S {
        &self.state
    }

    /// 获取状态可变引用（Executor 内部使用）。
    ///
    /// ⚠️ 仅限 crate 内部调用。外部代码应通过 `ExecutorState::apply_batch()` 或
    /// `NodeContext::record()` 间接操作状态。
    pub(crate) fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// 获取数据面发射器引用。
    pub fn stream(&self) -> Option<&dyn StreamSink> {
        self.stream.as_deref()
    }

    /// 获取取消令牌引用（Composite 节点用于 child_token）。
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// 获取 stream 的 Arc 引用（Parallel 子分支 clone 用）。
    pub fn stream_sink(&self) -> Option<Arc<dyn StreamSink>> {
        self.stream.clone()
    }

    /// 取出最终状态。
    pub fn into_state(self) -> S {
        self.state
    }

    // ─── commit() — Unit of Work 流水线 ───────────────────────

    /// 取出 mutation batch（Executor 调用）。
    ///
    /// 这是 commit 流水线的第一段：
    /// ```text
    /// take_commit_batch() → TraceSink/MutationLog 消费 → apply_batch_to_state()
    /// ```
    ///
    /// 调用方可以在此处插入 Trace 记录、MutationLog 持久化等扩展点。
    pub fn take_commit_batch(&mut self) -> Vec<S::Mutation> {
        std::mem::take(&mut self.mutations)
    }

    /// 将 mutation batch 应用到状态（Executor 调用）。
    ///
    /// commit 流水线的最后一段。与 `take_commit_batch()` 配合使用。
    pub fn apply_batch_to_state(&mut self, mutations: Vec<S::Mutation>) {
        if !mutations.is_empty() {
            self.state.apply_batch(mutations);
        }
    }

    /// 完整的 commit 流水线（便捷方法）。
    ///
    /// 等价于 `apply_batch_to_state(take_commit_batch())`。
    /// 内部调用使用此方法即可；需要扩展 Trace/MutationLog 的场景
    /// 应手动调用 `take_commit_batch()` + 扩展点 + `apply_batch_to_state()`。
    pub fn commit(&mut self) {
        let batch = self.take_commit_batch();
        self.apply_batch_to_state(batch);
    }
}

// ─── ExecutionView for ExecutionEngine ───────────────────────

impl<S: WorkflowState> ExecutionView<S> for ExecutionEngine<S> {
    fn state(&self) -> &S {
        &self.state
    }

    fn emit(&self, chunk: StreamChunk) {
        if let Some(ref stream) = self.stream {
            stream.emit(chunk);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

// ─── ExecutorState for ExecutionEngine ────────────────────────

impl<S: WorkflowState> ExecutorState<S> for ExecutionEngine<S> {
    fn build_node_context(&mut self) -> crate::node_context::NodeContext<'_, S> {
        crate::node_context::NodeContext {
            state: &mut self.state,
            stream: self.stream.as_deref(),
            cancel: &self.cancel,
            control: &mut self.control,
            metadata: &mut self.metadata,
            mutations: &mut self.mutations,
            flow_events: &mut self.flow_events,
        }
    }

    fn build_leaf_context(&mut self) -> crate::node_context::LeafContext<'_, S> {
        crate::node_context::LeafContext {
            state: &self.state,
            stream: self.stream.as_deref(),
            cancel: &self.cancel,
            control: &mut self.control,
            metadata: &mut self.metadata,
            mutations: &mut self.mutations,
            flow_events: &mut self.flow_events,
        }
    }

    fn clone_state(&self) -> S {
        self.state.clone()
    }

    fn replace_state(&mut self, state: S) {
        self.state = state;
    }

    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>) {
        self.state.apply_batch(mutations);
    }

    fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>) {
        self.control.take()
    }

    fn take_metadata(&mut self) -> NodeMetadata {
        std::mem::take(&mut self.metadata)
    }

    fn take_flow_events(&mut self) -> Vec<FlowEvent> {
        std::mem::take(&mut self.flow_events)
    }

    fn emit_flow_event(&mut self, event: FlowEvent) {
        self.flow_events.push(event);
    }
}

// ─── Backward Compat Alias ────────────────────────────────────

/// 向后兼容别名 — `ExecutionContext` → `ExecutionEngine`。
pub type ExecutionContext<S> = ExecutionEngine<S>;
