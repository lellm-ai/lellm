//! NodeContext + ExecutionControl + StreamEmitter — v04 核心类型。
//!
//! NodeContext 是 Runtime Handle（运行时句柄），节点只借用，不拥有。
//! 节点通过 NodeContext 读写 State、发射数据面事件、发出控制信号。
//!
//! v0.4+: 泛型化 `NodeContext<'a, S>`，S: WorkflowState。
//! 默认 `S = State`（HashMap）保持向后兼容。

use crate::branch_state::BranchState;
use crate::event::FlowEvent;
use crate::state::State;
use crate::stream_emitter::StreamEmitter;
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

// ─── NodeContext ──────────────────────────────────────────────

/// 节点上下文 — Runtime Handle（运行时句柄）。
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
///
/// # 设计原则
///
/// NodeContext 是 Runtime Handle，不是 Runtime State。
/// - 节点只借用，不拥有。零复制透传给子组件
/// - 禁止放入：RuntimeEventEmitter、TraceId、SpanId、GraphHandle、ExecutorConfig
///
/// # Effects 缓冲（v0.4+）
///
/// `effects` 字段收集节点产生的 Effect（领域事件），
/// 供上层（如 Executor）统一 apply 到 Typed State。
pub struct NodeContext<'a, S: WorkflowState = State> {
    /// 类型化状态 — 直接读写
    state: &'a mut S,
    /// 底层分支状态 — 用于 fork 等操作（backward compat）
    branch: &'a mut BranchState,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    stream: Option<&'a StreamEmitter>,
    /// 控制信号 — 节点写入，Executor 读取
    control: ExecutionControl,
    /// 节点元数据 — 节点写入
    metadata: NodeMetadata,
    /// Effect 缓冲 — 节点产生的强类型领域事件
    effects: Vec<S::Effect>,
    /// FlowEvent 缓冲 — 节点产生的控制面事件
    flow_events: Vec<FlowEvent>,
}

impl<'a, S: WorkflowState> NodeContext<'a, S> {
    /// 创建新的 NodeContext。
    pub fn new(
        state: &'a mut S,
        branch: &'a mut BranchState,
        stream: Option<&'a StreamEmitter>,
    ) -> Self {
        Self {
            state,
            branch,
            stream,
            control: ExecutionControl::new(),
            metadata: NodeMetadata::default(),
            effects: Vec::new(),
            flow_events: Vec::new(),
        }
    }

    /// 获取类型化状态引用。
    pub fn state(&self) -> &S {
        self.state
    }

    /// 获取类型化状态可变引用。
    pub fn state_mut(&mut self) -> &mut S {
        self.state
    }

    /// 获取底层 BranchState 引用（用于 fork 等操作）。
    pub fn branch(&self) -> &BranchState {
        self.branch
    }

    /// 获取底层 BranchState 可变引用。
    pub fn branch_mut(&mut self) -> &mut BranchState {
        self.branch
    }

    // ─── 数据面发射 ─────────────────────────────────────────

    /// 发射数据面事件（无 stream 则静默丢弃）。
    pub fn emit(&self, chunk: crate::stream_chunk::StreamChunk) {
        if let Some(stream) = &self.stream {
            stream.emit(chunk);
        }
    }

    /// 发射控制面 FlowEvent（缓冲到 NodeContext，供 Executor 收集转发）。
    pub fn emit_flow_event(&mut self, event: FlowEvent) {
        self.flow_events.push(event);
    }

    // ─── 控制信号 ─────────────────────────────────────────

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

    // ─── 元数据 ─────────────────────────────────────────

    /// 设置 token 成本。
    pub fn set_token_cost(&mut self, cost: f64) {
        self.metadata.token_cost = cost;
    }

    /// 标记有副作用。
    pub fn set_has_side_effects(&mut self) {
        self.metadata.has_side_effects = true;
    }

    // ─── Effects 缓冲（v0.4+ Typed State）───────────────────

    /// 发射一个 Effect（强类型领域事件）到缓冲。
    ///
    /// 零序列化开销 — 直接存储 `S::Effect`。
    pub fn emit_effect(&mut self, effect: S::Effect) {
        self.effects.push(effect);
    }

    /// 消费 Effect 缓冲（返回所有收集的 Effect）。
    pub fn consume_effects(&mut self) -> Vec<S::Effect> {
        std::mem::take(&mut self.effects)
    }

    /// 获取已收集的 Effect 数量（不消费）。
    pub fn effects_len(&self) -> usize {
        self.effects.len()
    }

    // ─── 内部方法（供 Executor 使用）─────────────────────────

    /// 消费控制信号（Executor 调用）。
    pub fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>) {
        self.control.take()
    }

    /// 获取元数据（Executor 调用）。
    pub fn take_metadata(&mut self) -> NodeMetadata {
        std::mem::take(&mut self.metadata)
    }

    /// 获取数据面发射器引用。
    pub fn stream(&self) -> Option<&'a StreamEmitter> {
        self.stream
    }

    /// 消费 FlowEvent 缓冲（Executor 调用）。
    pub fn take_flow_events(&mut self) -> Vec<FlowEvent> {
        std::mem::take(&mut self.flow_events)
    }
}
