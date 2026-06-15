# LeLLM v0.2 Graph/Node/Edge 编排层设计

> 版本：v0.2 | 日期：2026-06-15 | 状态：设计阶段

## [S1] 设计目标

**Workflow DAG + Loop Node** — 图结构必须 Acyclic，循环通过 NodeKind::Loop 表达。

### 核心约束

| 约束 | 决策 |
|------|------|
| DAG 类型 | 严格 Acyclic，不允许 A→B→C→A 任意环 |
| 循环表达 | NodeKind::Loop 节点，不是边形成环 |
| 控制流 | Sequence, Condition, Parallel, Loop |
| Node 种类 | 严格 5 种：Task, Agent, Tool, Condition, Loop |
| 数据传递 | 共享 State（HashMap<String, Value>）|
| 执行模式 | 宏观串行，Parallel Node 内部并发 |

## [S2] Node 类型定义

```rust
pub enum NodeKind {
    Task(TaskNode),           // 自定义逻辑
    Agent(AgentNode),         // 包装 ToolUseLoop
    Tool(ToolNode),           // 调用工具
    Condition(ConditionNode), // 条件分支
    Loop(LoopNode),           // 循环容器
}

pub struct TaskNode {
    pub name: String,
    pub func: Box<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>,
}

pub struct AgentNode {
    pub name: String,
    pub agent: ToolUseLoop,  // 直接复用 v0.1
}

pub struct ToolNode {
    pub name: String,
    pub tool_name: String,   // ToolExecutor 中的工具名
}

pub struct ConditionNode {
    pub name: String,
    pub branches: Vec<(String, Box<dyn Fn(&State) -> bool + Send + Sync>)>,
}

pub struct LoopNode {
    pub name: String,
    pub body: SubGraph,
    pub continue_condition: Box<dyn Fn(&State) -> bool + Send + Sync>,
    pub max_iterations: usize,
}
```

## [S3] Graph 结构

```rust
pub struct Graph {
    nodes: IndexMap<String, NodeKind>,
    edges: Vec<Edge>,
    start: String,
    end: String,
}

pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<Box<dyn Fn(&State) -> bool + Send + Sync>>,
}
```

## [S4] State 设计

```rust
pub type State = HashMap<String, serde_json::Value>;

pub struct GraphResult {
    pub state: State,
    pub execution_log: Vec<ExecutionEntry>,
    pub duration: Duration,
}

pub struct ExecutionEntry {
    pub node_name: String,
    pub start_time: Instant,
    pub end_time: Instant,
    pub success: bool,
}
```

## [S5] 执行语义

### 宏观串行

节点按拓扑顺序依次执行，控制流确定，易于 Debug 和 Tracing。

### Parallel Node

- 对外是单入单出的串行节点
- 内部是封闭的、支持并发调度的沙盒
- 使用 `futures::future::join_all` 实施局部并发
- Reducer 聚合所有子图结果到主 State

```rust
impl GraphNode for ParallelNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        let mut futures = Vec::new();
        
        for sub_graph in &self.sub_graphs {
            let local_state = state.clone();
            futures.push(async move {
                sub_graph.execute(local_state).await
            });
        }
        
        let results = futures::future::join_all(futures).await;
        
        let mut successful_results = Vec::new();
        for res in results {
            successful_results.push(res?);
        }
        
        (self.reducer)(state, successful_results)?;
        
        Ok(NextStep::GoToNext)
    }
}
```

### Loop Node

- while 循环（条件在后）+ max_iterations 熔断
- 每次执行完 body 后求值条件
- 条件满足时 break，否则继续循环
- 达到 max_iterations 时强制熔断

```rust
impl GraphNode for LoopNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        let mut current_iteration = 0;
        
        loop {
            current_iteration += 1;
            
            if current_iteration > self.max_iterations {
                return Err(GraphError::LoopLimitExceeded {
                    limit: self.max_iterations,
                });
            }
            
            self.body.execute(state).await?;
            
            let should_continue = (self.continue_condition)(state);
            
            if !should_continue {
                break;
            }
        }
        
        Ok(NextStep::GoToNext)
    }
}
```

### 错误处理

**Fail-Fast**：节点失败立即停止，返回错误。

## [S6] Builder API

```rust
let graph = GraphBuilder::new("my_workflow")
    .start("planner")
    .node("planner", NodeKind::Agent(AgentNode {
        name: "planner".into(),
        agent: planner_agent,
    }))
    .node("executor", NodeKind::Task(TaskNode {
        name: "executor".into(),
        func: Box::new(|state| {
            // 执行逻辑
            Ok(())
        }),
    }))
    .node("validator", NodeKind::Condition(ConditionNode {
        name: "validator".into(),
        branches: vec![
            ("success".into(), Box::new(|s| s["valid"] == true)),
            ("retry".into(), Box::new(|s| s["valid"] == false)),
        ],
    }))
    .edge("planner", "executor", None)
    .edge("executor", "validator", None)
    .edge("validator", "end", Some(Box::new(|s| s["valid"] == true)))
    .edge("validator", "planner", Some(Box::new(|s| s["valid"] == false)))
    .end("end")
    .build()?;  // 构建时验证
```

## [S7] 验证规则

```rust
impl Graph {
    pub fn build(self) -> Result<Graph, GraphError> {
        // 1. 单起点、单终点
        // 2. 所有节点可达
        // 3. 无环（Loop Node 除外）
        // 4. Edge 条件覆盖完整
    }
}
```

## [S8] 错误类型

```rust
pub enum GraphError {
    InvalidGraph(String),           // 图结构无效
    NodeNotFound(String),           // 节点不存在
    NodeExecutionFailed {
        node: String,
        source: Box<dyn std::error::Error>,
    },
    LoopLimitExceeded { limit: usize },  // 循环超限
    StateError(String),             // State 操作错误
}
```

## [S9] Crate 结构

```
lellm/
├── lellm-graph/        # 新建
│   ├── src/
│   │   ├── lib.rs
│   │   ├── graph.rs    # Graph + Builder
│   │   ├── node.rs     # NodeKind 定义
│   │   ├── state.rs    # State + GraphResult
│   │   └── error.rs    # GraphError
│   └── Cargo.toml      # 依赖 lellm-agent
```

## [S10] 与 v0.1 集成

- AgentNode 直接持有 `ToolUseLoop`
- LoopDetector/SignalVoter 作为 feature gate 集成
- 复用 `ToolExecutor`, `RetryPolicy`, `FallbackStrategy`

## [S11] 测试策略

- 单元测试各组件
- 集成测试完整 Graph 执行
- 测试场景：
  - 简单线性流水线
  - 条件分支
  - Loop 循环 + 熔断
  - 错误处理
  - Parallel Node 并发

## [S12] 版本路线图

| 版本 | 范围 |
|------|------|
| v0.2 | Graph/Node/Edge + LoopDetector/SignalVoter |
| v0.3 | StateGraph（LangGraph 风格任意环）|
| v0.4 | Checkpoint + 持久化 |
