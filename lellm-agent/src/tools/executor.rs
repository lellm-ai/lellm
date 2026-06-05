//! 工具执行器 — 注册、分派、批量执行、并行安全分级。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use lellm_core::{Message, ToolCall, ToolError, ToolErrorKind, ToolResult};

use super::{RetryPolicy, ToolFn};

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

/// 工具注册信息（包含安全分级 + 执行函数）。
pub struct ToolRegistration {
    safety: ParallelSafety,
    category: Option<ToolCategory>,
    func: ToolFn,
}

impl ToolRegistration {
    pub fn safe<F, Fut>(f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            safety: ParallelSafety::Safe,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    pub fn category_exclusive<F, Fut>(category: ToolCategory, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            safety: ParallelSafety::CategoryExclusive,
            category: Some(category),
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    pub fn exclusive<F, Fut>(f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            safety: ParallelSafety::Exclusive,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }
}

/// 工具执行器 — 按名称分派 ToolCall 到实际工具函数。
///
/// 内部使用 `Arc<HashMap>` — 注册后不可变，clone 为 O(1)。
pub struct ToolExecutor {
    tools: Arc<HashMap<String, ToolFn>>,
    safety: Arc<HashMap<String, ParallelSafety>>,
    categories: Arc<HashMap<String, ToolCategory>>,
    retry_policy: RetryPolicy,
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self {
            tools: Arc::new(HashMap::new()),
            safety: Arc::new(HashMap::new()),
            categories: Arc::new(HashMap::new()),
            retry_policy: RetryPolicy::default(),
        }
    }
}

impl Clone for ToolExecutor {
    fn clone(&self) -> Self {
        Self {
            tools: Arc::clone(&self.tools),
            safety: Arc::clone(&self.safety),
            categories: Arc::clone(&self.categories),
            retry_policy: self.retry_policy.clone(),
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

    pub fn register(&mut self, name: &str, reg: ToolRegistration) {
        Arc::get_mut(&mut self.safety)
            .unwrap()
            .insert(name.to_string(), reg.safety.clone());
        if let Some(cat) = reg.category {
            Arc::get_mut(&mut self.categories)
                .unwrap()
                .insert(name.to_string(), cat);
        }
        Arc::get_mut(&mut self.tools)
            .unwrap()
            .insert(name.to_string(), reg.func);
    }

    pub fn safety_for(&self, name: &str) -> ParallelSafety {
        self.safety
            .get(name)
            .cloned()
            .unwrap_or(ParallelSafety::Exclusive)
    }

    /// 获取工具所属的 category（仅 CategoryExclusive 工具有意义）
    fn category_for(&self, name: &str) -> Option<ToolCategory> {
        self.categories.get(name).cloned()
    }

    /// 执行单个工具调用，自带重试。
    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(tool_fn) => {
                self.retry_policy
                    .execute_with_retry(tool_fn, &call.arguments)
                    .await
            }
            None => Err(ToolError {
                kind: ToolErrorKind::NotFound,
                message: format!("unknown tool: {}", call.name),
            }),
        }
    }

    /// 批量执行 tool_calls。
    ///
    /// 执行策略：
    /// - `Safe` → 全部并发（`join_all`）
    /// - `CategoryExclusive` → 按 category 分组，组内串行、组间并发
    /// - `Exclusive` → 全部串行
    pub async fn execute_batch(&self, calls: &[ToolCall]) -> Vec<Message> {
        // 按安全分级分组
        let mut safe_calls = Vec::new();
        let mut category_calls: HashMap<ToolCategory, Vec<ToolCall>> = HashMap::new();
        let mut exclusive_calls = Vec::new();

        for call in calls {
            match self.safety_for(&call.name) {
                ParallelSafety::Safe => safe_calls.push(call.clone()),
                ParallelSafety::CategoryExclusive => {
                    if let Some(cat) = self.category_for(&call.name) {
                        category_calls.entry(cat).or_default().push(call.clone());
                    } else {
                        // 没有 category 的 CategoryExclusive 降级为 Exclusive
                        exclusive_calls.push(call.clone());
                    }
                }
                ParallelSafety::Exclusive => exclusive_calls.push(call.clone()),
            }
        }

        // 使用 Arc 共享 executor，避免每个 spawn 克隆整个 HashMap
        let executor = Arc::new(self.clone());

        let mut group_handles: Vec<tokio::task::JoinHandle<Vec<Message>>> = Vec::new();

        // Safe 组 — 并发执行
        if !safe_calls.is_empty() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(
                async move { exe.run_parallel(safe_calls).await },
            ));
        }

        // CategoryExclusive 组 — 每组内串行，组间并发
        for group_calls in category_calls.into_values() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(
                async move { exe.run_serial(group_calls).await },
            ));
        }

        // Exclusive 组 — 串行执行
        if !exclusive_calls.is_empty() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(async move {
                exe.run_serial(exclusive_calls).await
            }));
        }

        // 等待所有组完成，合并结果
        let all_results = futures_util::future::join_all(group_handles).await;
        let mut flat = Vec::new();
        for handle_result in all_results {
            let messages = handle_result.expect("spawned task panicked");
            flat.extend(messages);
        }
        flat
    }

    /// 并发执行一组 Safe 工具
    async fn run_parallel(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let futures: Vec<_> = calls.iter().map(|call| self.execute(call)).collect();
        let results = futures_util::future::join_all(futures).await;
        calls
            .into_iter()
            .zip(results)
            .map(|(call, result)| Message::tool_result(&call, &result))
            .collect()
    }

    /// 串行执行一组互斥工具
    async fn run_serial(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let result = self.execute(&call).await;
            results.push(Message::tool_result(&call, &result));
        }
        results
    }

    /// 按安全分级将 tool_calls 分为可并行和需串行两组
    pub fn partition_calls(&self, calls: &[ToolCall]) -> (Vec<ToolCall>, Vec<ToolCall>) {
        let mut safe = Vec::new();
        let mut exclusive = Vec::new();
        for call in calls {
            let safety = self.safety_for(&call.name);
            match safety {
                ParallelSafety::Safe => safe.push(call.clone()),
                ParallelSafety::CategoryExclusive | ParallelSafety::Exclusive => {
                    exclusive.push(call.clone());
                }
            }
        }
        (safe, exclusive)
    }
}
