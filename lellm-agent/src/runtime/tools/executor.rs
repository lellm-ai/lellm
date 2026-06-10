//! 工具执行器 — 注册、分派、批量执行、并行安全分级。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use lellm_core::{Message, ToolCall, ToolDefinition, ToolError, ToolErrorKind, ToolResult};

use super::super::retry::RetryPolicy;
use super::ToolFn;

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
/// 用户通过 `ToolRegistration::safe()` 等工厂方法构造，
/// `ToolExecutor` 内部直接持有。消除 `ToolEntry` 的无意义拷贝。
#[derive(Clone)]
pub struct ToolRegistration {
    pub definition: ToolDefinition,
    pub safety: ParallelSafety,
    pub category: Option<ToolCategory>,
    pub func: ToolFn,
}

impl ToolRegistration {
    pub fn safe<F, Fut>(def: ToolDefinition, f: F) -> Self
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

    pub fn category_exclusive<F, Fut>(def: ToolDefinition, category: ToolCategory, f: F) -> Self
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

    pub fn exclusive<F, Fut>(def: ToolDefinition, f: F) -> Self
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
    /// 执行失败的工具调用会被填充为 `ToolResult { is_error: true }`。
    pub results: Vec<Message>,
    /// 是否有任意 spawned task panic（仅作为观测信号）
    pub panicked: bool,
}

/// 工具执行器 — 按名称分派 ToolCall 到实际工具函数。
///
/// 内部使用 `Arc<HashMap>` — 注册后不可变，clone 为 O(1)。
#[derive(Clone)]
pub struct ToolExecutor {
    tools: Arc<HashMap<String, ToolRegistration>>,
    retry_policy: RetryPolicy,
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self {
            tools: Arc::new(HashMap::new()),
            retry_policy: RetryPolicy::default(),
        }
    }
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造时绑定全局重试策略。
    pub fn with_retry_policy(policy: RetryPolicy) -> Self {
        Self {
            retry_policy: policy,
            ..Default::default()
        }
    }

    /// 设置/替换重试策略（不丢失已注册的工具）。
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy;
    }

    /// 是否有注册的工具
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 收集所有工具的 Schema 定义
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition.clone()).collect()
    }

    pub fn register(&mut self, name: &str, reg: ToolRegistration) {
        Arc::get_mut(&mut self.tools)
            .expect("ToolExecutor already cloned, cannot register more tools")
            .insert(name.to_string(), reg);
    }

    pub fn safety_for(&self, name: &str) -> ParallelSafety {
        self.tools
            .get(name)
            .map(|t| t.safety.clone())
            .unwrap_or(ParallelSafety::Exclusive)
    }

    /// 获取工具所属的 category（仅 CategoryExclusive 工具有意义）
    fn category_for(&self, name: &str) -> Option<ToolCategory> {
        self.tools.get(name).and_then(|t| t.category.clone())
    }

    /// 执行单个工具调用，自带重试。
    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(entry) => self
                .retry_policy
                .execute_with_retry(&entry.func, &call.arguments)
                .await,
            None => Err(ToolError {
                kind: ToolErrorKind::NotFound,
                message: format!("unknown tool: {}", call.name),
            }),
        }
    }

  /// 批量执行 tool_calls。
    ///
    /// # ParallelSafety 契约
    ///
    /// - `Safe`: 全并发（每个 tool 独立 spawn）
    /// - `CategoryExclusive(cat)`: 组内串行，组间并发（同 category 串行，不同 category 并发）
    /// - `Exclusive`: 全串行（全局一个 task，tool 依次执行）
    ///
    /// # Panic 隔离
    ///
    /// panic 被转换为 `ToolResult(error)`，永远不会中止兄弟 tool 调用。
    ///
    /// # 不变量
    ///
    /// - `results.len() == calls.len()` 永远成立
    /// - `results[i]` 对应 `calls[i]`（原始顺序）
    /// - `panicked` 仅作为观测信号
    pub async fn execute_batch(&self, calls: &[ToolCall]) -> BatchExecutionResult {
        if calls.is_empty() {
            return BatchExecutionResult {
                results: Vec::new(),
                panicked: false,
            };
        }

        // 分组时保留原始索引
        let mut safe_calls: Vec<(usize, ToolCall)> = Vec::new();
        let mut category_calls: HashMap<ToolCategory, Vec<(usize, ToolCall)>> = HashMap::new();
        let mut exclusive_calls: Vec<(usize, ToolCall)> = Vec::new();

        for (idx, call) in calls.iter().enumerate() {
            match self.safety_for(&call.name) {
                ParallelSafety::Safe => safe_calls.push((idx, call.clone())),
                ParallelSafety::CategoryExclusive => {
                    if let Some(cat) = self.category_for(&call.name) {
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

        // 构建 group handles — 每个 group 是一个 spawned task
        let mut group_handles: Vec<tokio::task::JoinHandle<Vec<(usize, Message)>>> = Vec::new();
        // 记录每个 group 的索引列表，用于 panic 恢复时精准回填
        let mut group_indices: Vec<Vec<usize>> = Vec::new();

        let executor = Arc::new(self.clone());

        // Safe: 每个 tool 独立 spawn（全并发）
        if !safe_calls.is_empty() {
            let exe = Arc::clone(&executor);
            let indices: Vec<usize> = safe_calls.iter().map(|(i, _)| *i).collect();
            group_handles.push(tokio::spawn(async move {
                exe.run_parallel_indexed(safe_calls).await
            }));
            group_indices.push(indices);
        }

        // CategoryExclusive: 按 category 分组，组内串行、组间并发
        for group_calls in category_calls.into_values() {
            let exe = Arc::clone(&executor);
            let indices: Vec<usize> = group_calls.iter().map(|(i, _)| *i).collect();
            group_handles.push(tokio::spawn(async move {
                exe.run_serial_indexed(group_calls).await
            }));
            group_indices.push(indices);
        }

        // Exclusive: 全部串行，一个 task
        if !exclusive_calls.is_empty() {
            let exe = Arc::clone(&executor);
            let indices: Vec<usize> = exclusive_calls.iter().map(|(i, _)| *i).collect();
            group_handles.push(tokio::spawn(async move {
                exe.run_serial_indexed(exclusive_calls).await
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
                    // panic 只影响该 group 的索引
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

    /// Safe group: 每个 tool 独立 spawn，全并发。
    /// 如果某个 tool panic，被 JoinHandle 捕获并转为 ToolResult(error)。
    async fn run_parallel_indexed(
        &self,
        calls: Vec<(usize, ToolCall)>,
    ) -> Vec<(usize, Message)> {
        // 每个 spawn 携带 idx — 当前 join_all 保证顺序一致，idx 是冗余的。
        // 但为未来切换到 JoinSet/FuturesUnordered 预留正确性。
        let handles: Vec<_> = calls
            .iter()
            .map(|(idx, call)| {
                let exe = self.clone();
                let call = call.clone();
                let idx = *idx;
                tokio::spawn(async move {
                    let result = exe.execute(&call).await;
                    (idx, Message::tool_result(&call, &result))
                })
            })
            .collect();

        let raw = futures_util::future::join_all(handles).await;
        // zip 回原始 calls，确保 panic 时也能构造错误结果
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

   /// 组内串行执行，每个 tool spawn+await 实现 panic 隔离。
    ///
    /// 所有路径（CategoryExclusive, Exclusive）统一 tool 级隔离：
    /// 一个 tool panic 不会丢失同组中已完成的 tool 结果。
    async fn run_serial_indexed(
        &self,
        calls: Vec<(usize, ToolCall)>,
    ) -> Vec<(usize, Message)> {
        let mut results = Vec::with_capacity(calls.len());
        for (idx, call) in calls {
            let exe = self.clone();
            let call_clone = call.clone();
            let name = call_clone.name.clone();
            let exec_result =
                match tokio::spawn(async move { exe.execute(&call_clone).await }).await {
                    Ok(tool_result) => tool_result,
                    Err(join_err) => Err(ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("tool '{name}' panicked: {join_err}"),
                    }),
                };
            results.push((idx, Message::tool_result(&call, &exec_result)));
        }
        results
    }
}
