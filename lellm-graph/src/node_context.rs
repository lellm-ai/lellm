//! NodeContext + ExecutionControl + StreamEmitter — v04 核心类型。
//!
//! NodeContext 是 Runtime Handle（运行时句柄），节点只借用，不拥有。
//! 节点通过 NodeContext 读写 State、发射数据面事件、发出控制信号。

use crate::branch_state::BranchState;
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

// ─── NextStep ─────────────────────────────────────────────────

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
    ///
    /// 多次调用的语义：最后一次获胜（与 State 写入的"最后写入者胜"一致）。
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
/// NodeContext 是 Runtime Handle（运行时句柄），不是 Runtime State。
/// - 节点只借用，不拥有。零复制透传给子组件
/// - 禁止放入：RuntimeEventEmitter、TraceId、SpanId、GraphHandle、ExecutorConfig
///
/// # Effects 缓冲（v0.4+）
///
/// `effects` 字段收集节点产生的 Effect（领域事件），
/// 供上层（如 Agent 的 ReAct 循环）统一 apply 到 Typed State。
/// 传统 `ctx.set()` 路径仍然可用，向后兼容。
pub struct NodeContext<'a> {
    /// 执行状态 — 直接写
    state: &'a mut BranchState,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    stream: Option<&'a StreamEmitter>,
    /// 控制信号 — 节点写入，Executor 读取
    control: ExecutionControl,
    /// 节点元数据 — 节点写入
    metadata: NodeMetadata,
    /// Effect 缓冲 — 节点产生的领域事件（v0.4+ Typed State）
    effects: Vec<serde_json::Value>,
}

impl<'a> NodeContext<'a> {
    /// 创建新的 NodeContext。
    pub fn new(state: &'a mut BranchState, stream: Option<&'a StreamEmitter>) -> Self {
        Self {
            state,
            stream,
            control: ExecutionControl::new(),
            metadata: NodeMetadata::default(),
            effects: Vec::new(),
        }
    }

    // ─── State 读取 ─────────────────────────────────────────

    /// 从 State 读取值（返回 clone）。
    pub fn get<T: Clone + serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.state
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// 从 State 读取原始 Value。
    pub fn get_raw(&self, key: &str) -> Option<&serde_json::Value> {
        self.state.get_ref(key)
    }

    // ─── State 写入 ─────────────────────────────────────────

    /// 写入 State。
    pub fn set<T: serde::Serialize>(&mut self, key: impl Into<String>, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.state.set(key.into(), v);
        }
    }

    /// 追加到数组。
    pub fn append(&mut self, key: impl Into<String>, value: serde_json::Value) {
        let key = key.into();
        let current: Vec<serde_json::Value> = self
            .state
            .get_ref(&key)
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let mut new = current;
        new.push(value);
        self.state.set(key, serde_json::Value::Array(new));
    }

    /// 递增数值。
    pub fn increment(&mut self, key: impl Into<String>, delta: u64) {
        let key = key.into();
        let current: u64 = self
            .state
            .get_ref(&key)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        self.state.set(key, serde_json::json!(current + delta));
    }

    /// 删除 State key。
    pub fn remove(&mut self, key: &str) {
        self.state.remove(key);
    }

    // ─── 数据面发射 ─────────────────────────────────────────

    /// 发射数据面事件（无 stream 则静默丢弃）。
    pub fn emit(&self, chunk: crate::stream_chunk::StreamChunk) {
        if let Some(stream) = &self.stream {
            stream.emit(chunk);
        }
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

    /// 发射一个 Effect（领域事件）到缓冲。
    ///
    /// 节点通过此方法产生 Effect，上层统一 apply 到 Typed State。
    /// 传统 `ctx.set()` 路径仍然可用，向后兼容。
    ///
    /// # 示例
    ///
    /// ```rust,ignore
    /// // Agent 节点发射 Effect
    /// ctx.emit_effect(AgentEffect::AppendMessage(msg));
    /// // 或者
    /// ctx.emit_effect_json(serde_json::to_value(effect)?);
    /// ```
    pub fn emit_effect<E: serde::Serialize>(&mut self, effect: E) {
        if let Ok(v) = serde_json::to_value(effect) {
            self.effects.push(v);
        }
    }

    /// 消费 Effect 缓冲（返回所有收集的 Effect）。
    ///
    /// 上层（如 ReAct 循环）调用此方法获取节点产生的 Effect，
    /// 然后 apply 到 Typed State。
    pub fn consume_effects(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.effects)
    }

    /// 获取已收集的 Effect 数量（不消费）。
    pub fn effects_len(&self) -> usize {
        self.effects.len()
    }

    // ─── Typed State 访问（v0.4+）───────────────────────────

    /// 从 State 读取类型化值（WorkflowState）。
    ///
    /// 通过 key 获取存储的类型化状态对象。
    /// 与 `get::<T>()` 的区别：此方法明确用于 WorkflowState 协议。
    pub fn get_state<S: WorkflowState + serde::de::DeserializeOwned>(&self, key: &str) -> Option<S> {
        self.get(key)
    }

    /// 写入类型化值（WorkflowState）到 State。
    ///
    /// 通过 key 存储类型化状态对象。
    pub fn set_state<S: WorkflowState + serde::Serialize>(
        &mut self,
        key: impl Into<String>,
        state: S,
    ) {
        self.set(key, state);
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

    /// 获取状态引用（用于路由解析等）。
    pub fn state(&self) -> &BranchState {
        self.state
    }
}
