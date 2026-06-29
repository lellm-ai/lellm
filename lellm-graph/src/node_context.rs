//! NodeContext + LeafContext — 节点能力视图。
//!
//! 职责分离：
//! - `LeafContext<'a, S>` — 只读视图，Leaf 节点使用（`&S`，不能修改 State）
//! - `NodeContext<'a, S>` — 可变视图，向后兼容（`&mut S`，可通过 `replace_state()` 修改）
//!
//! ExecutionEngine 和相关 trait 定义在 [`execution_engine`] 模块中。

use tokio_util::sync::CancellationToken;

use crate::event::FlowEvent;
use crate::execution_engine::{ExecutionControl, NodeMetadata};
use crate::state::State;
use crate::stream_chunk::StreamChunk;
use crate::stream_emitter::StreamSink;
use crate::workflow_state::WorkflowState;

// ─── Backward Compat Re-exports ──────────────────────────────

/// 向后兼容 — `ExecutionContext` 已迁移到 [`execution_engine`] 模块。
pub use crate::execution_engine::ExecutionContext;

// ─── LeafContext (Borrowed View) ──────────────────────────────

/// Leaf 节点能力视图 — 纯借用，不拥有任何状态。
///
/// 设计原则：
/// - **只能读 State**（`&S`，不可变引用）
/// - **只能 emit Mutation**（借用在 Engine 的 mutations buffer）
/// - **只能 emit Stream / FlowEvent**
/// - **不能 replace_state / clone_state / fork / merge**
///
/// 与 NodeContext 的区别：
/// - NodeContext 持有 `&mut S`（可变引用），Composite 节点可用 replace_state()
/// - LeafContext 持有 `&S`（只读引用），编译期保证不能修改 State
pub struct LeafContext<'a, S: WorkflowState = State> {
    /// 类型化状态 — 只读引用
    pub(crate) state: &'a S,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    pub(crate) stream: Option<&'a dyn StreamSink>,
    /// 取消令牌
    pub(crate) cancel: &'a CancellationToken,
    /// 控制信号 — 节点写入，Executor 读取
    pub(crate) control: &'a mut ExecutionControl,
    /// 节点元数据 — 节点写入
    pub(crate) metadata: &'a mut NodeMetadata,
    /// Mutation 缓冲 — 借用在 ExecutionEngine 的 buffer
    pub(crate) mutations: &'a mut Vec<S::Mutation>,
    /// FlowEvent 缓冲 — 借用在 ExecutionEngine 的 buffer
    pub(crate) flow_events: &'a mut Vec<FlowEvent>,
}

impl<S: WorkflowState> LeafContext<'_, S> {
    // ─── 读 State ─────────────────────────────────────────────

    /// 获取类型化状态（只读）。
    pub fn state(&self) -> &S {
        self.state
    }

    // ─── 记录 Mutation ────────────────────────────────────────

    /// 记录一个 Mutation（强类型状态变更命令）到缓冲。
    ///
    /// 这是 Leaf 节点变更状态的**唯一入口**。
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

    /// 发射控制面 FlowEvent（缓冲到 ExecutionEngine，供 Executor 收集转发）。
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

// ─── NodeContext ──────────────────────────────────────────────

/// 节点能力视图 — 向后兼容，节点能做的三件事：读 State、记录 Mutation、发射 Stream。
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
    pub(crate) state: &'a mut S,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    pub(crate) stream: Option<&'a dyn StreamSink>,
    /// 取消令牌
    pub(crate) cancel: &'a CancellationToken,
    /// 控制信号 — 节点写入，Executor 读取
    pub(crate) control: &'a mut ExecutionControl,
    /// 节点元数据 — 节点写入
    pub(crate) metadata: &'a mut NodeMetadata,
    /// Mutation 缓冲 — 节点产生的强类型领域事件
    pub(crate) mutations: &'a mut Vec<S::Mutation>,
    /// FlowEvent 缓冲 — 节点产生的控制面事件
    pub(crate) flow_events: &'a mut Vec<FlowEvent>,
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
