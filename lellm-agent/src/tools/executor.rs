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
#[derive(Clone, Default)]
pub struct ToolExecutor {
    tools: HashMap<String, ToolFn>,
    safety: HashMap<String, ParallelSafety>,
    categories: HashMap<String, ToolCategory>,
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            safety: HashMap::new(),
            categories: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, reg: ToolRegistration) {
        self.safety.insert(name.to_string(), reg.safety.clone());
        if let Some(cat) = reg.category {
            self.categories.insert(name.to_string(), cat);
        }
        self.tools.insert(name.to_string(), reg.func);
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
                let policy = RetryPolicy::default();
                policy.execute_with_retry(tool_fn, &call.arguments).await
            }
            None => Err(ToolError {
                kind: ToolErrorKind::Internal,
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

        // 构建所有并行任务组：Safe 组 + 每个 category 组 + Exclusive 组
        // 使用 tokio::spawn 绕过 impl Future 类型不一致的问题
        let mut group_handles: Vec<tokio::task::JoinHandle<Vec<Message>>> = Vec::new();

        // Safe 组 — 并发执行
        if !safe_calls.is_empty() {
            let executor = self.clone();
            group_handles.push(tokio::spawn(async move {
                executor.run_parallel(safe_calls).await
            }));
        }

        // CategoryExclusive 组 — 每组内串行，组间并发
        for group_calls in category_calls.into_values() {
            let executor = self.clone();
            group_handles.push(tokio::spawn(async move {
                executor.run_serial(group_calls).await
            }));
        }

        // Exclusive 组 — 串行执行
        if !exclusive_calls.is_empty() {
            let executor = self.clone();
            group_handles.push(tokio::spawn(async move {
                executor.run_serial(exclusive_calls).await
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
            .map(|(call, result)| self.to_tool_result_message(&call, result))
            .collect()
    }

    /// 串行执行一组互斥工具
    async fn run_serial(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let result = self.execute(&call).await;
            results.push(self.to_tool_result_message(&call, result));
        }
        results
    }

    /// 将 ToolResult 转为 Message::ToolResult
    fn to_tool_result_message(&self, call: &ToolCall, result: ToolResult) -> Message {
        let content = match result {
            Ok(s) => s,
            Err(e) => format!("tool error: {e}"),
        };
        Message::ToolResult {
            tool_call_id: call.id.clone(),
            content: lellm_core::text_block(content),
        }
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
