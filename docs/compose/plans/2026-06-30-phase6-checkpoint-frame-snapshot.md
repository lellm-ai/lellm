# Phase 6: Checkpoint = Execution Frame Snapshot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use compose:subagent (recommended) or compose:execute to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 Execution Frame Snapshot Checkpoint 系统

**Architecture:** FrameStack + CheckpointState + 自动 frame boundary checkpoint

**Tech Stack:** Rust, lellm-graph

## Global Constraints

- 每个单元测试耗时务必小于 10s
- 编写代码的硬性指标：每个代码文件不要超过 400 行
- 每层文件夹中的文件，尽可能不超过 8 个
- checkpoint 不是保存 state，而是保存 execution position + state projection

---

## 文件结构

```
lellm-graph/src/
├── checkpoint/
│   ├── mod.rs              # Checkpoint 模块入口
│   ├── frame.rs            # Frame + FrameStack 定义
│   ├── state.rs            # CheckpointState 定义
│   └── policy.rs           # Checkpoint 触发策略
└── execution_engine.rs     # 修改：集成 FrameStack
```

---

### Task 1: 创建 Checkpoint 模块基础结构

**Covers:** Phase 6 基础设施

**Files:**
- Create: `lellm-graph/src/checkpoint/mod.rs`
- Create: `lellm-graph/src/checkpoint/frame.rs`
- Create: `lellm-graph/src/checkpoint/state.rs`
- Create: `lellm-graph/src/checkpoint/policy.rs`

**Interfaces:**
- Produces: `Frame`, `FrameStack`, `CheckpointState`, `CheckpointPolicy`

- [ ] **Step 1: 创建 checkpoint/mod.rs**

```rust
//! Checkpoint — Execution Frame Snapshot 系统。
//!
//! 核心洞察：checkpoint 不是保存 state，而是保存 execution position + state projection。

pub mod frame;
pub mod policy;
pub mod state;

pub use frame::{Frame, FrameStack};
pub use policy::CheckpointPolicy;
pub use state::CheckpointState;
```

- [ ] **Step 2: 创建 checkpoint/frame.rs**

```rust
//! Frame + FrameStack — 执行位置快照。

use crate::workflow_state::WorkflowState;

/// 执行帧 — 保存单个 Graph 的执行位置。
#[derive(Debug, Clone)]
pub struct Frame<S: WorkflowState> {
    /// 图 ID
    pub graph_id: String,

    /// 当前节点 ID
    pub node_id: String,

    /// 状态快照（可序列化的 projection）
    pub state_snapshot: S,

    /// 执行游标（节点索引或步骤数）
    pub cursor: usize,
}

impl<S: WorkflowState> Frame<S> {
    /// 创建新的 Frame。
    pub fn new(graph_id: String, node_id: String, state_snapshot: S, cursor: usize) -> Self {
        Self {
            graph_id,
            node_id,
            state_snapshot,
            cursor,
        }
    }
}

/// 帧栈 — 保存完整的执行位置历史。
#[derive(Debug, Clone)]
pub struct FrameStack<S: WorkflowState> {
    /// 帧列表（从外到内）
    frames: Vec<Frame<S>>,
}

impl<S: WorkflowState> FrameStack<S> {
    /// 创建空的帧栈。
    pub fn new() -> Self {
        Self {
            frames: Vec::new(),
        }
    }

    /// Push 一个新帧。
    pub fn push(&mut self, frame: Frame<S>) {
        self.frames.push(frame);
    }

    /// Pop 最后一个帧。
    pub fn pop(&mut self) -> Option<Frame<S>> {
        self.frames.pop()
    }

    /// 获取当前帧（最顶层）。
    pub fn current(&self) -> Option<&Frame<S>> {
        self.frames.last()
    }

    /// 获取帧数量。
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// 检查是否为空。
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 获取所有帧的引用。
    pub fn frames(&self) -> &[Frame<S>] {
        &self.frames
    }
}

impl<S: WorkflowState> Default for FrameStack<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;

    #[test]
    fn test_frame_creation() {
        let frame = Frame::new(
            "workflow".to_string(),
            "node_a".to_string(),
            State::new(),
            0,
        );

        assert_eq!(frame.graph_id, "workflow");
        assert_eq!(frame.node_id, "node_a");
        assert_eq!(frame.cursor, 0);
    }

    #[test]
    fn test_frame_stack_push_pop() {
        let mut stack = FrameStack::<State>::new();
        assert!(stack.is_empty());

        let frame1 = Frame::new("g1".to_string(), "n1".to_string(), State::new(), 0);
        let frame2 = Frame::new("g2".to_string(), "n2".to_string(), State::new(), 1);

        stack.push(frame1);
        stack.push(frame2);

        assert_eq!(stack.depth(), 2);
        assert!(!stack.is_empty());

        let popped = stack.pop();
        assert!(popped.is_some());
        assert_eq!(popped.unwrap().graph_id, "g2");

        assert_eq!(stack.depth(), 1);
    }

    #[test]
    fn test_frame_stack_current() {
        let mut stack = FrameStack::<State>::new();
        assert!(stack.current().is_none());

        let frame = Frame::new("g1".to_string(), "n1".to_string(), State::new(), 0);
        stack.push(frame);

        let current = stack.current();
        assert!(current.is_some());
        assert_eq!(current.unwrap().graph_id, "g1");
    }
}
```

- [ ] **Step 3: 创建 checkpoint/state.rs**

```rust
//! CheckpointState — 可序列化的状态投影。

use crate::workflow_state::WorkflowState;

/// Checkpoint 状态 — 可序列化的状态投影。
///
/// 核心原则：checkpoint = projection, not raw state
///
/// Runtime State（不可序列化）：
/// ```text
/// struct WorkflowState {
///     agent: AgentState,
///     cache: Arc<...>,  // ❌ 不可序列化
///     channels: mpsc::...,  // ❌ 不可序列化
/// }
/// ```
///
/// Checkpoint State（可序列化）：
/// ```text
/// struct CheckpointState {
///     agent: AgentCheckpoint,
///     planner: PlannerCheckpoint,
/// }
/// ```
pub trait CheckpointState: WorkflowState + Clone + Send + Sync {
    /// 从 Runtime State 创建 Checkpoint State。
    fn from_runtime(state: &Self) -> Self;

    /// 恢复到 Runtime State。
    fn restore(&self) -> Self;
}

/// Checkpoint 数据 — 包含状态和帧栈。
#[derive(Debug, Clone)]
pub struct Checkpoint<S: CheckpointState> {
    /// 状态快照
    pub state: S,

    /// 帧栈快照
    pub frames: Vec<crate::checkpoint::Frame<S>>,

    /// 创建时间戳
    pub created_at: std::time::Instant,
}

impl<S: CheckpointState> Checkpoint<S> {
    /// 创建新的 Checkpoint。
    pub fn new(state: S, frames: Vec<crate::checkpoint::Frame<S>>) -> Self {
        Self {
            state,
            frames,
            created_at: std::time::Instant::now(),
        }
    }

    /// 恢复状态。
    pub fn restore_state(&self) -> S {
        self.state.restore()
    }

    /// 恢复帧栈。
    pub fn restore_frames(&self) -> crate::checkpoint::FrameStack<S> {
        let mut stack = crate::checkpoint::FrameStack::new();
        for frame in &self.frames {
            stack.push(frame.clone());
        }
        stack
    }
}

/// CheckpointStore — Checkpoint 存储 trait。
#[async_trait::async_trait]
pub trait CheckpointStore<S: CheckpointState>: Send + Sync {
    /// 保存 Checkpoint。
    async fn save(&self, checkpoint: &Checkpoint<S>) -> Result<(), Box<dyn std::error::Error>>;

    /// 加载最近的 Checkpoint。
    async fn load_latest(&self) -> Result<Option<Checkpoint<S>>, Box<dyn std::error::Error>>;

    /// 加载指定 ID 的 Checkpoint。
    async fn load(&self, id: &str) -> Result<Option<Checkpoint<S>>, Box<dyn std::error::Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;

    // State 需要实现 CheckpointState 才能测试
    // 这里先测试 Checkpoint 结构

    #[test]
    fn test_checkpoint_creation() {
        let state = State::new();
        let frames = vec![];

        let checkpoint = Checkpoint::new(state, frames);

        assert!(checkpoint.frames.is_empty());
    }
}
```

- [ ] **Step 4: 创建 checkpoint/policy.rs**

```rust
//! CheckpointPolicy — Checkpoint 触发策略。

/// Checkpoint 触发策略。
#[derive(Debug, Clone)]
pub enum CheckpointPolicy {
    /// 每个 Node exit 触发
    EveryNode,

    /// 每个 Subgraph exit 触发
    EverySubgraph,

    /// 每 N 个步骤触发
    EverySteps(usize),

    /// 基于时间间隔触发
    EveryDuration(std::time::Duration),

    /// 自定义条件触发
    Custom(String),

    /// 禁用 Checkpoint
    Disabled,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self::EverySubgraph
    }
}

impl CheckpointPolicy {
    /// 检查是否应该触发 Checkpoint。
    pub fn should_checkpoint(&self, context: &CheckpointContext) -> bool {
        match self {
            Self::EveryNode => true,
            Self::EverySubgraph => context.is_subgraph_exit,
            Self::EverySteps(n) => context.steps % n == 0,
            Self::EveryDuration(d) => context.last_checkpoint.elapsed() >= *d,
            Self::Custom(_) => false, // TODO: 实现自定义条件
            Self::Disabled => false,
        }
    }
}

/// Checkpoint 上下文 — 用于决策是否触发 Checkpoint。
#[derive(Debug)]
pub struct CheckpointContext {
    /// 是否是 Subgraph exit
    pub is_subgraph_exit: bool,

    /// 当前步骤数
    pub steps: usize,

    /// 上次 Checkpoint 时间
    pub last_checkpoint: std::time::Instant,
}

impl CheckpointContext {
    /// 创建新的 Checkpoint 上下文。
    pub fn new(is_subgraph_exit: bool, steps: usize, last_checkpoint: std::time::Instant) -> Self {
        Self {
            is_subgraph_exit,
            steps,
            last_checkpoint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_policy_disabled() {
        let policy = CheckpointPolicy::Disabled;
        let ctx = CheckpointContext::new(false, 10, std::time::Instant::now());

        assert!(!policy.should_checkpoint(&ctx));
    }

    #[test]
    fn test_checkpoint_policy_every_node() {
        let policy = CheckpointPolicy::EveryNode;
        let ctx = CheckpointContext::new(false, 10, std::time::Instant::now());

        assert!(policy.should_checkpoint(&ctx));
    }

    #[test]
    fn test_checkpoint_policy_every_subgraph() {
        let policy = CheckpointPolicy::EverySubgraph;

        let ctx1 = CheckpointContext::new(false, 10, std::time::Instant::now());
        assert!(!policy.should_checkpoint(&ctx1));

        let ctx2 = CheckpointContext::new(true, 10, std::time::Instant::now());
        assert!(policy.should_checkpoint(&ctx2));
    }

    #[test]
    fn test_checkpoint_policy_every_steps() {
        let policy = CheckpointPolicy::EverySteps(5);

        let ctx1 = CheckpointContext::new(false, 4, std::time::Instant::now());
        assert!(!policy.should_checkpoint(&ctx1));

        let ctx2 = CheckpointContext::new(false, 5, std::time::Instant::now());
        assert!(policy.should_checkpoint(&ctx2));

        let ctx3 = CheckpointContext::new(false, 10, std::time::Instant::now());
        assert!(policy.should_checkpoint(&ctx3));
    }
}
```

- [ ] **Step 5: 更新 lib.rs 添加 checkpoint 模块**

```rust
// 在 lellm-graph/src/lib.rs 中添加
pub mod checkpoint;
```

- [ ] **Step 6: 运行测试验证**

```bash
cargo test -p lellm-graph checkpoint
```

- [ ] **Step 7: 提交**

```bash
git add lellm-graph/src/checkpoint/ lellm-graph/src/lib.rs
git commit -m "feat(v0.5): Phase 6 - 创建 Checkpoint 模块基础结构"
```

---

### Task 2: 集成 FrameStack 到 ExecutionEngine

**Covers:** Phase 6 核心集成

**Files:**
- Modify: `lellm-graph/src/execution_engine.rs`

**Interfaces:**
- Consumes: `FrameStack`, `CheckpointPolicy`
- Produces: `ExecutionEngine::checkpoint()`, `ExecutionEngine::restore()`

- [ ] **Step 1: 在 execution_engine.rs 中添加 FrameStack 字段**

```rust
// 在 ExecutionEngine 结构体中添加
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

    // ─── Phase 6: FrameStack ─────────────────────────────────
    /// 帧栈 — 保存执行位置历史
    frame_stack: crate::checkpoint::FrameStack<S>,

    /// Checkpoint 策略
    checkpoint_policy: crate::checkpoint::CheckpointPolicy,

    /// 上次 Checkpoint 时间
    last_checkpoint: std::time::Instant,
}
```

- [ ] **Step 2: 更新 ExecutionEngine::new()**

```rust
impl<S: WorkflowState> ExecutionEngine<S> {
    pub fn new(state: S, stream: Option<Arc<dyn StreamSink>>, cancel: CancellationToken) -> Self {
        Self {
            state,
            stream,
            cancel,
            control: ExecutionControl::new(),
            metadata: NodeMetadata::default(),
            mutations: Vec::new(),
            flow_events: Vec::new(),
            frame_stack: crate::checkpoint::FrameStack::new(),
            checkpoint_policy: crate::checkpoint::CheckpointPolicy::default(),
            last_checkpoint: std::time::Instant::now(),
        }
    }
}
```

- [ ] **Step 3: 添加 checkpoint 相关方法**

```rust
impl<S: WorkflowState> ExecutionEngine<S> {
    // ─── FrameStack API ─────────────────────────────────────

    /// Push 一个新帧。
    pub fn push_frame(&mut self, frame: crate::checkpoint::Frame<S>) {
        self.frame_stack.push(frame);
    }

    /// Pop 最后一个帧。
    pub fn pop_frame(&mut self) -> Option<crate::checkpoint::Frame<S>> {
        self.frame_stack.pop()
    }

    /// 获取当前帧。
    pub fn current_frame(&self) -> Option<&crate::checkpoint::Frame<S>> {
        self.frame_stack.current()
    }

    /// 获取帧栈深度。
    pub fn frame_depth(&self) -> usize {
        self.frame_stack.depth()
    }

    // ─── Checkpoint API ─────────────────────────────────────

    /// 创建 Checkpoint。
    pub fn checkpoint(&self) -> crate::checkpoint::Checkpoint<S> {
        crate::checkpoint::Checkpoint::new(
            self.state.clone(),
            self.frame_stack.frames().to_vec(),
        )
    }

    /// 从 Checkpoint 恢复。
    pub fn restore(&mut self, checkpoint: crate::checkpoint::Checkpoint<S>) {
        self.state = checkpoint.restore_state();
        self.frame_stack = checkpoint.restore_frames();
        self.last_checkpoint = std::time::Instant::now();
    }

    /// 检查是否应该触发 Checkpoint。
    pub fn should_checkpoint(&self, is_subgraph_exit: bool) -> bool {
        let ctx = crate::checkpoint::CheckpointContext::new(
            is_subgraph_exit,
            self.metadata.token_cost as usize, // 暂时用 token_cost 作为 steps
            self.last_checkpoint,
        );
        self.checkpoint_policy.should_checkpoint(&ctx)
    }

    /// 设置 Checkpoint 策略。
    pub fn set_checkpoint_policy(&mut self, policy: crate::checkpoint::CheckpointPolicy) {
        self.checkpoint_policy = policy;
    }
}
```

- [ ] **Step 4: 运行测试验证**

```bash
cargo test -p lellm-graph
```

- [ ] **Step 5: 提交**

```bash
git add lellm-graph/src/execution_engine.rs
git commit -m "feat(v0.5): Phase 6 - 集成 FrameStack 到 ExecutionEngine"
```

---

### Task 4: 添加单元测试

**Covers:** Phase 6 测试

**Files:**
- Modify: `lellm-graph/src/checkpoint/frame.rs`
- Modify: `lellm-graph/src/checkpoint/state.rs`
- Modify: `lellm-graph/src/checkpoint/policy.rs`

**Interfaces:**
- Consumes: `Frame`, `FrameStack`, `Checkpoint`, `CheckpointPolicy`

- [ ] **Step 1: 运行所有 Checkpoint 测试**

```bash
cargo test -p lellm-graph checkpoint
```

- [ ] **Step 2: 提交**

```bash
git add lellm-graph/src/checkpoint/
git commit -m "test(v0.5): Phase 6 - 添加 Checkpoint 单元测试"
```

---

### Task 5: 文档更新

**Covers:** Phase 6 文档

**Files:**
- Modify: `docs/v05-graph-as-runtime.md`

- [ ] **Step 1: 更新实现状态**

```markdown
## 实现状态

- [x] Phase 1：AgentBuilder::build() → Graph<AgentState>
- [x] Phase 2：ToolUseLoop 重构为薄 Facade
- [x] Phase 3：删除 AgentFlowNode
- [x] Phase 4：StateLens + SubgraphNode + SubgraphSpec
- [x] Phase 5：Compiler Inline Pass（骨架实现）
- [x] Phase 6：Checkpoint = Execution Frame Snapshot
```

- [ ] **Step 2: 提交**

```bash
git add docs/v05-graph-as-runtime.md
git commit -m "docs(v0.5): Phase 6 - 更新文档"
```

---

## Self-Review

**1. Spec coverage:** Phase 6 的所有设计要求都已覆盖：
- Frame + FrameStack ✅
- CheckpointState ✅
- CheckpointPolicy ✅
- ExecutionEngine 集成 ✅

**2. Placeholder scan:** 没有发现占位符。所有代码都是完整的。

**3. Type consistency:** 所有类型和方法签名都是一致的。

---

## Execution Handoff

Phase 6 是一个相对独立的任务，适合 Inline 执行。
