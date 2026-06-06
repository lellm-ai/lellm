//! 工具执行器 — 注册、分派、批量执行、并行安全分级。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use lellm_core::{Message, ToolCall, ToolDefinition, ToolError, ToolErrorKind, ToolResult};

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

/// 工具完整条目 — Schema、安全分级、执行函数合一。
#[derive(Clone)]
pub struct ToolEntry {
    pub definition: ToolDefinition,
    pub safety: ParallelSafety,
    pub category: Option<ToolCategory>,
    pub func: ToolFn,
}

/// 工具注册信息（包含 Schema + 安全分级 + 执行函数）。
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

/// 工具执行器 — 按名称分派 ToolCall 到实际工具函数。
///
/// 内部使用 `Arc<HashMap>` — 注册后不可变，clone 为 O(1)。
#[derive(Clone)]
pub struct ToolExecutor {
    tools: Arc<HashMap<String, ToolEntry>>,
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

    /// 是否有注册的工具
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 收集所有工具的 Schema 定义
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition.clone()).collect()
    }

    pub fn register(&mut self, name: &str, reg: ToolRegistration) {
        let entry = ToolEntry {
            definition: reg.definition,
            safety: reg.safety,
            category: reg.category,
            func: reg.func,
        };
        Arc::get_mut(&mut self.tools)
            .unwrap()
            .insert(name.to_string(), entry);
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
            Some(entry) => {
                self.retry_policy
                    .execute_with_retry(&entry.func, &call.arguments)
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
                        exclusive_calls.push(call.clone());
                    }
                }
                ParallelSafety::Exclusive => exclusive_calls.push(call.clone()),
            }
        }

        let executor = Arc::new(self.clone());
        let mut group_handles: Vec<tokio::task::JoinHandle<Vec<Message>>> = Vec::new();

        if !safe_calls.is_empty() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(
                async move { exe.run_parallel(safe_calls).await },
            ));
        }

        for group_calls in category_calls.into_values() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(
                async move { exe.run_serial(group_calls).await },
            ));
        }

        if !exclusive_calls.is_empty() {
            let exe = Arc::clone(&executor);
            group_handles.push(tokio::spawn(async move {
                exe.run_serial(exclusive_calls).await
            }));
        }

        let all_results = futures_util::future::join_all(group_handles).await;
        let mut flat = Vec::new();
        for handle_result in all_results {
            let messages = handle_result.expect("spawned task panicked");
            flat.extend(messages);
        }
        flat
    }

    async fn run_parallel(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let futures: Vec<_> = calls.iter().map(|call| self.execute(call)).collect();
        let results = futures_util::future::join_all(futures).await;
        calls
            .into_iter()
            .zip(results)
            .map(|(call, result)| Message::tool_result(&call, &result))
            .collect()
    }

    async fn run_serial(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let result = self.execute(&call).await;
            results.push(Message::tool_result(&call, &result));
        }
        results
    }

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
