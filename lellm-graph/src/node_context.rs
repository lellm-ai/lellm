//! NodeContext + ExecutionEngine — v0.4 核心类型。
//!
//! 职责分离：
//! - `ExecutionEngine<S>` — Executor 内部拥有，持有 State、Mutation 缓冲、流发射器等
//! - `NodeContext<'a, S>` — 节点能力视图，只暴露 Read State / Record Mutation / Emit Stream
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

use tokio_util::sync::CancellationToken;

use crate::event::FlowEvent;
use crate::state::State;
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
    fn build_node_context(&mut self) -> NodeContext<'_, S>;
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
/// 不对节点开发者公开。节点通过 [`NodeContext`] 能力视图交互。
pub struct ExecutionEngine<S: WorkflowState> {
    /// 类型化状态 — Engine 独占写权限
    state: S,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    stream: Option<Box<dyn StreamSink>>,
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
    pub fn new(
        state: S,
        stream: Option<Box<dyn StreamSink>>,
        cancel: CancellationToken,
    ) -> Self {
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
        self.stream.as_ref().map(|s| s.as_ref())
    }

    /// 取出最终状态。
    pub fn into_state(self) -> S {
        self.state
    }
}

// ─── ExecutorState for ExecutionEngine ────────────────────────

impl<S: WorkflowState> ExecutionView<S> for ExecutionEngine<S> {
    fn state(&self) -> &S {
        &self.state
    }

    fn emit(&self, chunk: StreamChunk) {
        if let Some(stream) = self.stream.as_ref() {
            stream.emit(chunk);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl<S: WorkflowState> ExecutorState<S> for ExecutionEngine<S> {
    fn build_node_context(&mut self) -> NodeContext<'_, S> {
        NodeContext {
            state: &mut self.state,
            stream: self.stream.as_ref().map(|s| s.as_ref()),
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

// ─── NodeContext ──────────────────────────────────────────────

/// 节点能力视图 — 节点能做的三件事：读 State、记录 Mutation、发射 Stream。
///
/// # 设计原则
///
/// NodeContext 是 Runtime 的能力视图，不是 Runtime 的拥有者。
/// - 节点只借用，不拥有。零复制透传给子组件
/// - 禁止放入：RuntimeEventEmitter、TraceId、SpanId、GraphHandle、ExecutorConfig
/// - **不提供 `state_mut()`** — 节点只能通过 `record()` 声明变更意图
/// - 组合节点（如 ParallelNode）使用 `replace_state()` 整体替换状态
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
pub struct NodeContext<'a, S: WorkflowState = State> {
    /// 类型化状态 — 可变引用（仅组合节点如 ParallelNode 需要写权限）
    state: &'a mut S,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    stream: Option<&'a dyn StreamSink>,
    /// 取消令牌
    cancel: &'a CancellationToken,
    /// 控制信号 — 节点写入，Executor 读取
    control: &'a mut ExecutionControl,
    /// 节点元数据 — 节点写入
    metadata: &'a mut NodeMetadata,
    /// Mutation 缓冲 — 节点产生的强类型领域事件
    mutations: &'a mut Vec<S::Mutation>,
    /// FlowEvent 缓冲 — 节点产生的控制面事件
    flow_events: &'a mut Vec<FlowEvent>,
}

impl<S: WorkflowState> NodeContext<'_, S> {
    // ─── 读 State ─────────────────────────────────────────────

    /// 获取类型化状态（只读）。
    pub fn state(&self) -> &S {
        &self.state
    }

    /// 替换整个状态（仅组合节点使用，如 ParallelNode）。
    ///
    /// 这是组合节点合并子分支结果后的 sanctioned API。
    /// 普通节点应使用 `record()` 声明变更意图。
    ///
    /// 替换后，Engine 持有的状态直接变为 `new_state`。
    /// 不会触发 Mutation 记录（因为这是整体替换，不是增量变更）。
    pub fn replace_state(&mut self, new_state: S) {
        *self.state = new_state;
    }

    // ─── 记录 Mutation ────────────────────────────────────────

    /// 记录一个 Mutation（强类型状态变更命令）到缓冲。
    ///
    /// 这是节点变更状态的**唯一入口**。
    /// 零序列化开销 — 直接存储 `S::Mutation`。
    ///
    /// Executor 在节点执行后统一消费并 apply 到 State。
    pub fn record(&mut self, mutation: S::Mutation) {
        self.mutations.push(mutation);
    }

    // ─── 数据面发射 ───────────────────────────────────────────

    /// 发射数据面事件（无 stream 则静默丢弃）。
    pub fn emit(&self, chunk: StreamChunk) {
        if let Some(stream) = self.stream {
            stream.emit(chunk);
        }
    }

    /// 发射控制面 FlowEvent（缓冲到 ExecutionContext，供 Executor 收集转发）。
    pub fn emit_flow_event(&mut self, event: FlowEvent) {
        self.flow_events.push(event);
    }

    // ─── 取消检查 ─────────────────────────────────────────────

    /// 检查是否已取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 获取取消令牌引用。
    pub fn cancel_token(&self) -> &CancellationToken {
        self.cancel
    }

    // ─── 控制信号 ─────────────────────────────────────────────

    /// 跳转到指定节点。
    pub fn goto(&mut self, target: impl Into<String>) {
        self.control.goto(target);
    }

    /// 结束执行。
    pub fn end(&mut self) {
        self.control.end();
    }

    /// Barrier 挂起。
    pub fn pause(
        &mut self,
        barrier_id: crate::event::BarrierId,
        timeout: Option<std::time::Duration>,
    ) {
        self.control.pause(barrier_id, timeout);
    }

    // ─── 元数据 ───────────────────────────────────────────────

    /// 设置 token 成本。
    pub fn set_token_cost(&mut self, cost: f64) {
        self.metadata.token_cost = cost;
    }

    /// 标记有副作用。
    pub fn set_has_side_effects(&mut self) {
        self.metadata.has_side_effects = true;
    }
}
