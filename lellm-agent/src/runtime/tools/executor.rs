//! 工具执行器 — 注册、分派、批量执行、并行安全分级。
//!
//! 通过 `ToolCatalog` 消费工具快照，不持有工具所有权。

use std::borrow::Cow;
use std::sync::Arc;

use lellm_core::{Message, ToolCall, ToolError, ToolErrorKind, ToolResult};

use super::super::event::AgentEvent;
use super::super::retry::RetryPolicy;
use super::{ToolCatalog, ToolFn, ToolSnapshot};
use tokio::sync::mpsc::Sender;

/// 工具安全分级
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelSafety {
    Safe,
    CategoryExclusive,
    Exclusive,
}

/// 工具类别 — 用于 CategoryExclusive 的分组
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCategory(pub Cow<'static, str>);

impl ToolCategory {
    pub const FILE_IO: Self = Self(Cow::Borrowed("file_io"));
    pub const NETWORK: Self = Self(Cow::Borrowed("network"));
    pub const DATABASE: Self = Self(Cow::Borrowed("database"));

    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }
}

/// 工具注册信息 — Schema、安全分级、执行函数合一。
///
/// 用户通过 `ToolRegistration::safe()` 等工厂方法构造。
/// 字段 `pub(crate)` — 外部通过工厂方法访问，内部通过 `ToolSnapshot` 消费。
#[derive(Clone)]
pub struct ToolRegistration {
    pub(crate) definition: lellm_core::ToolDefinition,
    pub(crate) safety: ParallelSafety,
    pub(crate) category: Option<ToolCategory>,
    pub(crate) func: ToolFn,
}

impl ToolRegistration {
    /// 获取工具定义的引用。
    pub fn definition(&self) -> &lellm_core::ToolDefinition {
        &self.definition
    }

    /// 获取并行安全级别。
    pub fn safety(&self) -> &ParallelSafety {
        &self.safety
    }

    /// 获取工具类别（如果有）。
    pub fn category(&self) -> Option<&ToolCategory> {
        self.category.as_ref()
    }

    pub fn safe<F, Fut>(def: lellm_core::ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Safe,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    pub fn category_exclusive<F, Fut>(
        def: lellm_core::ToolDefinition,
        category: ToolCategory,
        f: F,
    ) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::CategoryExclusive,
            category: Some(category),
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    pub fn exclusive<F, Fut>(def: lellm_core::ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Exclusive,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }
}

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

    /// 执行单个工具调用，自带重试。
    ///
    /// 使用预解析的快照执行。
    pub async fn execute_with_snapshot(
        &self,
        call: &ToolCall,
        snapshot: &ToolSnapshot,
    ) -> ToolResult {
        match snapshot.get(&call.name) {
            Some(entry) => {
                self.retry_policy
                    .execute_with_retry(&entry.func, &call.arguments)
                    .await
            }
            None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
        }
    }

    /// 执行单个工具调用，自带重试 + Retry 事件发射。
    pub async fn execute_with_emission(
        &self,
        call: &ToolCall,
        snapshot: &ToolSnapshot,
        tx: &Sender<AgentEvent>,
    ) -> ToolResult {
        match snapshot.get(&call.name) {
            Some(entry) => {
                self.retry_policy
                    .execute_with_retry_and_emission(&entry.func, &call.arguments, tx, &call.id)
                    .await
            }
            None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
        }
    }
}

/// 使用预解析的快照批量执行 tool_calls。
///
/// 这是动态目录模式的核心执行函数。
///
/// # 用法
///
/// ```ignore
/// let snapshot = executor.snapshot().await;
/// let result = execute_batch_with(&tool_calls, &snapshot, &executor.retry_policy()).await;
/// ```
///
/// # ParallelSafety 契约
///
/// - `Safe`: 全并发（每个 tool 独立 spawn）
/// - `CategoryExclusive(cat)`: 组内串行，组间并发
/// - `Exclusive`: 全串行
///
/// # 一致性保证
///
/// `snapshot` 快照在函数执行期间固定不变，不会因目录刷新而漂移。
pub async fn execute_batch_with(
    calls: &[ToolCall],
    snapshot: &ToolSnapshot,
    retry_policy: &RetryPolicy,
) -> BatchExecutionResult {
    if calls.is_empty() {
        return BatchExecutionResult {
            results: Vec::new(),
            panicked: false,
        };
    }

    // 分组时保留原始索引
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

    // 构建 group handles
    let mut group_handles: Vec<tokio::task::JoinHandle<Vec<(usize, Message)>>> = Vec::new();
    let mut group_indices: Vec<Vec<usize>> = Vec::new();

    let snapshot = Arc::new(snapshot.clone_for_spawn());
    let retry_policy = retry_policy.clone();

    // Safe: 每个 tool 独立 spawn（全并发）
    if !safe_calls.is_empty() {
        let s = Arc::clone(&snapshot);
        let rp = retry_policy.clone();
        let indices: Vec<usize> = safe_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(async move {
            run_parallel_indexed_with(&s, &rp, safe_calls).await
        }));
        group_indices.push(indices);
    }

    // CategoryExclusive: 按 category 分组，组内串行、组间并发
    for group_calls in category_calls.into_values() {
        let s = Arc::clone(&snapshot);
        let rp = retry_policy.clone();
        let indices: Vec<usize> = group_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(async move {
            run_serial_indexed_with(&s, &rp, group_calls).await
        }));
        group_indices.push(indices);
    }

    // Exclusive: 全部串行，一个 task
    if !exclusive_calls.is_empty() {
        let s = Arc::clone(&snapshot);
        let rp = retry_policy.clone();
        let indices: Vec<usize> = exclusive_calls.iter().map(|(i, _)| *i).collect();
        group_handles.push(tokio::spawn(async move {
            run_serial_indexed_with(&s, &rp, exclusive_calls).await
        }));
        group_indices.push(indices);
    }

    // 按原始索引回填结果；panic 的 group 按索引列表精准回填错误
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

// ─── 内部辅助：快照克隆 ──────────────────────────────────────────

impl ToolSnapshot {
    /// 克隆内部工具映射，供 spawn 使用。
    fn clone_for_spawn(&self) -> Arc<indexmap::IndexMap<String, ToolRegistration>> {
        self.tools.clone()
    }
}

// ─── Safe group: 全并发 ──────────────────────────────────────────

async fn run_parallel_indexed_with(
    tools: &Arc<indexmap::IndexMap<String, ToolRegistration>>,
    retry_policy: &RetryPolicy,
    calls: Vec<(usize, ToolCall)>,
) -> Vec<(usize, Message)> {
    let handles: Vec<_> = calls
        .iter()
        .map(|(idx, call)| {
            let tools = Arc::clone(tools);
            let rp = retry_policy.clone();
            let call = call.clone();
            let idx = *idx;
            tokio::spawn(async move {
                let result = match tools.get(&call.name) {
                    Some(entry) => rp.execute_with_retry(&entry.func, &call.arguments).await,
                    None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
                };
                (idx, Message::tool_result(&call, &result))
            })
        })
        .collect();

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

async fn run_serial_indexed_with(
    tools: &Arc<indexmap::IndexMap<String, ToolRegistration>>,
    retry_policy: &RetryPolicy,
    calls: Vec<(usize, ToolCall)>,
) -> Vec<(usize, Message)> {
    let mut results = Vec::with_capacity(calls.len());
    for (idx, call) in calls {
        let exec_result = match tools.get(&call.name) {
            Some(entry) => {
                retry_policy
                    .execute_with_retry(&entry.func, &call.arguments)
                    .await
            }
            None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
        };
        results.push((idx, Message::tool_result(&call, &exec_result)));
    }
    results
}
