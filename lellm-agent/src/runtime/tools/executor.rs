//! 工具执行器 — 统一调度入口。
//!
//! 独立的工具子系统，被 runtime 层使用。
//!
//! **分层：**
//! - 协议层（lellm-core）：`ToolDefinition`, `ParallelSafety`, `ToolCategory`
//! - 可执行描述（lellm-core）：`ExecutableTool`, `ToolFn`
//! - 构造框架（lellm-core/tool feature）：`ToolArgs`, schema 生成
//! - 运行时层（本模块）：`ToolExecutor`, `ToolCatalog`, `ToolSnapshot`
//!
//! **执行链路：**
//! ```text
//! execute_batch()
//!   └── snapshot()                  → Arc<ToolSnapshot>
//!       └── run_batch_internal()    → ParallelSafety 分组
//!           └── dispatch_call()     → lookup + retry + invoke (唯一核心路径)
//!               └── ExecutableTool::execute()
//! ```

use std::sync::Arc;

use lellm_core::{
    Message, ParallelSafety, ToolCall, ToolCategory, ToolError, ToolErrorKind, ToolResult,
};

use super::super::retry::RetryPolicy;
use super::{ToolCatalog, ToolSnapshot};

/// 批量执行结果 — 长度、顺序、完整性三重保证。
///
/// **不变量：**
/// 1. `results.len() == calls.len()` 永远成立
/// 2. `results[i]` 对应 `calls[i]` 的执行结果（原始顺序）
/// 3. panic 永远被转换成 `ToolResult(is_error: true)`，不会丢失
/// 4. `panicked` 仅作为观测信号，不改变结果完整性
#[derive(Debug)]
pub struct BatchExecutionResult {
    /// 按原始调用顺序排列的工具结果，长度等于输入 calls 长度。
    pub results: Vec<Message>,
    /// 是否有任意 spawned task panic（仅作为观测信号）
    pub panicked: bool,
}

/// 工具执行器 — 按名称分派 ToolCall 到实际工具函数。
///
/// 内部持有 `ToolCatalog`，通过 `snapshot()` 获取冻结工具快照。
/// Clone 为 O(1)（Arc 浅拷贝）。
#[derive(Clone)]
pub struct ToolExecutor {
    catalog: Arc<dyn ToolCatalog>,
    retry_policy: RetryPolicy,
}

impl ToolExecutor {
    /// 绑定工具目录。
    pub fn new(catalog: Arc<dyn ToolCatalog>) -> Self {
        Self {
            catalog,
            retry_policy: RetryPolicy::default(),
        }
    }

    /// 绑定工具目录，使用默认重试策略。
    pub fn with_catalog(catalog: Arc<dyn ToolCatalog>) -> Self {
        Self::new(catalog)
    }

    /// 构造时绑定全局重试策略。
    pub fn with_retry_policy(catalog: Arc<dyn ToolCatalog>, policy: RetryPolicy) -> Self {
        Self {
            catalog,
            retry_policy: policy,
        }
    }

    /// 设置/替换重试策略。
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy;
    }

    /// 获取当前重试策略的克隆。
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy.clone()
    }

    /// 获取冻结工具快照。
    ///
    /// 每轮迭代调用一次，固定本轮工具集。
    pub async fn snapshot(&self) -> Arc<ToolSnapshot> {
        self.catalog.snapshot().await
    }

    // ─── 公开执行入口 ──────────────────────────────────────────

    /// 执行单个工具调用，自带 snapshot + retry。
    ///
    /// 唯一的单调用公开入口。
    pub async fn execute_one(&self, call: &ToolCall) -> ToolResult {
        let snapshot = self.snapshot().await;
        self.dispatch_one(call, &snapshot).await
    }

    /// 执行单个工具调用，使用预解析的快照。
    ///
    /// 供测试和需要固定快照的场景使用。
    pub async fn execute_one_with_snapshot(
        &self,
        call: &ToolCall,
        snapshot: &ToolSnapshot,
    ) -> ToolResult {
        self.dispatch_one(call, snapshot).await
    }

    /// 批量执行工具调用 — 唯一批量公开入口。
    ///
    /// 自动获取 snapshot，按 ParallelSafety 分组调度。
    pub async fn execute_batch(&self, calls: &[ToolCall]) -> BatchExecutionResult {
        let snapshot = self.snapshot().await;
        run_batch_internal(calls, snapshot, &self.retry_policy).await
    }

    // ─── 内部核心：dispatch ────────────────────────────────────

    /// 核心 dispatch — lookup + retry + invoke。
    ///
    /// 委托给静态 `dispatch_call`，确保 `execute_one`、`execute_batch`
    /// 以及 batch 内部的 spawn task 都走同一条执行路径。
    async fn dispatch_one(&self, call: &ToolCall, snapshot: &ToolSnapshot) -> ToolResult {
        dispatch_call(call, snapshot, &self.retry_policy).await
    }
}

// ─── 统一 dispatch：所有执行路径共享 ─────────────────────────────

/// 核心 dispatch — lookup + retry + invoke。
///
/// 静态函数，不依赖 `&self`，因此可以被 `dispatch_one`（单调用）、
/// `run_parallel_indexed` / `run_serial_indexed`（batch spawn task）复用。
/// 这是工具执行的唯一核心路径。
///
/// 闭包捕获 `&ExecutableTool` 引用，无需 clone，也无需创建临时 `ToolFn`。
async fn dispatch_call(
    call: &ToolCall,
    snapshot: &ToolSnapshot,
    retry_policy: &RetryPolicy,
) -> ToolResult {
    match snapshot.get(&call.name) {
        Some(tool) => retry_policy.execute(|| tool.execute(&call.arguments)).await,
        None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
    }
}

// ─── Safe group: 全并发 ──────────────────────────────────────────

async fn run_parallel_indexed(
    snapshot: Arc<ToolSnapshot>,
    retry_policy: RetryPolicy,
    calls: Vec<(usize, ToolCall)>,
) -> Vec<(usize, Message)> {
    let handles: Vec<_> = calls
        .iter()
        .map(|(idx, call)| {
            let snap = Arc::clone(&snapshot);
            let rp = retry_policy.clone();
            let call = call.clone();
            let idx = *idx;
            tokio::spawn(async move {
                let result = dispatch_call(&call, &snap, &rp).await;
                (idx, Message::tool_result(&call, &result))
            })
        })
        .collect();

    // snap: Arc<ToolSnapshot>, &snap → &Arc<ToolSnapshot>
    // dispatch_call 接收 &ToolSnapshot，Rust auto-deref 会处理

    let raw = futures_util::future::join_all(handles).await;
    raw.into_iter()
        .zip(calls.into_iter())
        .map(|(h, (idx, call))| match h {
            Ok((_, msg)) => (idx, msg),
            Err(join_err) => (
                idx,
                Message::tool_result(
                    &call,
                    &Err(ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("tool '{}' task panicked: {join_err}", call.name),
                    }),
                ),
            ),
        })
        .collect()
}

// ─── 组内串行 ────────────────────────────────────────────────────

async fn run_serial_indexed(
    snapshot: Arc<ToolSnapshot>,
    retry_policy: RetryPolicy,
    calls: Vec<(usize, ToolCall)>,
) -> Vec<(usize, Message)> {
    let mut results = Vec::with_capacity(calls.len());
    for (idx, call) in calls {
        let exec_result = dispatch_call(&call, &snapshot, &retry_policy).await;
        results.push((idx, Message::tool_result(&call, &exec_result)));
    }
    results
}

// ─── 内部：批量调度核心 ──────────────────────────────────────────

async fn run_batch_internal(
    calls: &[ToolCall],
    snapshot: Arc<ToolSnapshot>,
    retry_policy: &RetryPolicy,
) -> BatchExecutionResult {
    if calls.is_empty() {
        return BatchExecutionResult {
            results: Vec::new(),
            panicked: false,
        };
    }

    let mut safe_calls: Vec<(usize, ToolCall)> = Vec::new();
    let mut category_calls: std::collections::HashMap<ToolCategory, Vec<(usize, ToolCall)>> =
        std::collections::HashMap::new();
    let mut exclusive_calls: Vec<(usize, ToolCall)> = Vec::new();

    for (idx, call) in calls.iter().enumerate() {
        let safety = snapshot
            .get(&call.name)
            .map(|t| t.safety.clone())
            .unwrap_or(ParallelSafety::Exclusive);

        match safety {
            ParallelSafety::Safe => safe_calls.push((idx, call.clone())),
            ParallelSafety::CategoryExclusive => {
                if let Some(cat) = snapshot.get(&call.name).and_then(|t| t.category.clone()) {
                    category_calls
                        .entry(cat)
                        .or_default()
                        .push((idx, call.clone()));
                } else {
                    exclusive_calls.push((idx, call.clone()));
                }
            }
            ParallelSafety::Exclusive => exclusive_calls.push((idx, call.clone())),
        }
    }

    let mut group_handles: Vec<tokio::task::JoinHandle<Vec<(usize, Message)>>> = Vec::new();
    let mut group_indices: Vec<Vec<usize>> = Vec::new();

    if !safe_calls.is_empty() {
        let rp = retry_policy.clone();
        let indices: Vec<usize> = safe_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(run_parallel_indexed(
            Arc::clone(&snapshot),
            rp,
            safe_calls,
        )));
        group_indices.push(indices);
    }

    for group_calls in category_calls.into_values() {
        let rp = retry_policy.clone();
        let indices: Vec<usize> = group_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(run_serial_indexed(
            Arc::clone(&snapshot),
            rp,
            group_calls,
        )));
        group_indices.push(indices);
    }

    if !exclusive_calls.is_empty() {
        let rp = retry_policy.clone();
        let indices: Vec<usize> = exclusive_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(run_serial_indexed(
            Arc::clone(&snapshot),
            rp,
            exclusive_calls,
        )));
        group_indices.push(indices);
    }

    let mut results: Vec<Option<Message>> = vec![None; calls.len()];
    let mut panicked = false;
    let all_handles = futures_util::future::join_all(group_handles).await;

    for (handle_result, indices) in all_handles.into_iter().zip(group_indices.into_iter()) {
        match handle_result {
            Ok(indexed_messages) => {
                for (idx, msg) in indexed_messages {
                    results[idx] = Some(msg);
                }
            }
            Err(join_err) => {
                panicked = true;
                for idx in indices {
                    let call = &calls[idx];
                    results[idx] = Some(Message::tool_result(
                        call,
                        &Err(ToolError {
                            kind: ToolErrorKind::Internal,
                            message: format!("tool group task panicked: {join_err}"),
                        }),
                    ));
                }
            }
        }
    }

    BatchExecutionResult {
        results: results.into_iter().flatten().collect(),
        panicked,
    }
}

// ─── 向后兼容：自由函数别名 ──────────────────────────────────────

/// 向后兼容 — 等价于 `executor.execute_batch()`。
///
/// **已弃用。** 请直接调用 `ToolExecutor::execute_batch()`。
#[deprecated(since = "0.4.10", note = "Use ToolExecutor::execute_batch() instead")]
#[allow(deprecated)]
pub async fn execute_batch_with(
    calls: &[ToolCall],
    snapshot: &ToolSnapshot,
    retry_policy: &RetryPolicy,
) -> BatchExecutionResult {
    run_batch_internal(calls, Arc::new(snapshot.clone()), retry_policy).await
}
