//! 工具执行器 — 注册、分派、批量执行、并行安全分级。

use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use lellm_core::{Message, ToolCall};

use super::ToolCallResult;

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
    #[allow(dead_code)]
    category: Option<ToolCategory>,
    func: ToolFn,
}

/// 异步工具函数类型
type ToolFn = Arc<
    dyn Fn(&serde_json::Value) -> Pin<Box<dyn std::future::Future<Output = ToolCallResult> + Send>>
        + Send
        + Sync,
>;

impl ToolRegistration {
    pub fn safe<F, Fut>(f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolCallResult> + Send + 'static,
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
        Fut: std::future::Future<Output = ToolCallResult> + Send + 'static,
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
        Fut: std::future::Future<Output = ToolCallResult> + Send + 'static,
    {
        Self {
            safety: ParallelSafety::Exclusive,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }
}

/// 工具执行器 — 按名称分派 ToolCall 到实际工具函数。
#[derive(Default)]
pub struct ToolExecutor {
    tools: HashMap<String, ToolFn>,
    safety: HashMap<String, ParallelSafety>,
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            safety: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, reg: ToolRegistration) {
        self.safety.insert(name.to_string(), reg.safety.clone());
        self.tools.insert(name.to_string(), reg.func);
    }

    pub fn safety_for(&self, name: &str) -> ParallelSafety {
        self.safety
            .get(name)
            .cloned()
            .unwrap_or(ParallelSafety::Exclusive)
    }

    pub async fn execute(&self, call: &ToolCall) -> ToolCallResult {
        match self.tools.get(&call.name) {
            Some(tool_fn) => tool_fn(&call.arguments).await,
            None => ToolCallResult::Err(format!("unknown tool: {}", call.name)),
        }
    }

    pub async fn execute_batch(&self, calls: &[ToolCall]) -> Vec<Message> {
        let mut results = Vec::new();
        for call in calls {
            let result = self.execute(call).await;
            let content = match result {
                ToolCallResult::Ok(s) => s,
                ToolCallResult::Err(e) => format!("tool error: {e}"),
            };
            results.push(Message::ToolResult {
                tool_call_id: call.id.clone(),
                content: lellm_core::text_block(content),
            });
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
