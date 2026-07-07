//! FastMCP — 类似 Python FastMCP 的简洁 API。
//!
//! ```rust,ignore
//! use lellm_mcp::server::SimpleMcp;
//!
//! let mut mcp = SimpleMcp::new("My Server");
//!
//! mcp.tool("add", "Add two numbers", |args: serde_json::Value| async move {
//!     let a = args["a"].as_i64().unwrap_or(0);
//!     let b = args["b"].as_i64().unwrap_or(0);
//!     Ok(serde_json::json!({ "result": a + b }))
//! });
//!
//! mcp.run_stdio().await;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::protocol::{CallToolResult, ContentBlock, ToolInfo};

/// 工具执行函数类型。
pub type ToolFn = Arc<
    dyn Fn(
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>
        + Send
        + Sync,
>;

/// 工具定义。
struct Tool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    executor: ToolFn,
}

/// FastMCP 服务器。
///
/// 提供简洁的 API 来定义工具并运行 MCP 服务器。
pub struct SimpleMcp {
    name: String,
    tools: HashMap<String, Tool>,
}

impl SimpleMcp {
    /// 创建新的 SimpleMCP 服务器。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tools: HashMap::new(),
        }
    }

    /// 获取服务器名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 注册工具（使用默认空 schema）。
    ///
    /// - `name`: 工具名称
    /// - `description`: 工具描述
    /// - `handler`: 异步处理函数，接收 JSON 参数，返回 JSON 结果
    pub fn tool<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: F,
    ) where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, String>> + Send + 'static,
    {
        let default_schema = serde_json::json!({
            "type": "object",
            "properties": {}
        });
        self.tool_with_schema(name, description, default_schema, handler);
    }

    /// 注册工具（自定义 input_schema）。
    ///
    /// - `name`: 工具名称
    /// - `description`: 工具描述
    /// - `input_schema`: JSON Schema 描述参数结构
    /// - `handler`: 异步处理函数，接收 JSON 参数，返回 JSON 结果
    pub fn tool_with_schema<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        handler: F,
    ) where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value, String>> + Send + 'static,
    {
        let name = name.into();
        let description = description.into();

        let executor: ToolFn = Arc::new(move |args| {
            let fut = handler(args);
            Box::pin(fut)
        });

        self.tools.insert(
            name.clone(),
            Tool {
                name,
                description,
                input_schema,
                executor,
            },
        );
    }

    /// 获取工具信息列表。
    pub fn tool_list(&self) -> Vec<ToolInfo> {
        self.tools
            .values()
            .map(|t| ToolInfo {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    /// 调用工具。
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, super::ServerError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| super::ServerError::ToolNotFound(name.to_string()))?;

        match (tool.executor)(args).await {
            Ok(result) => Ok(CallToolResult {
                content: vec![ContentBlock::Text {
                    text: result.to_string(),
                }],
                is_error: false,
            }),
            Err(e) => Ok(CallToolResult {
                content: vec![ContentBlock::Text { text: e }],
                is_error: true,
            }),
        }
    }

    /// 以 stdio 模式运行服务器。
    pub async fn run_stdio(&self) -> Result<(), super::ServerError> {
        super::handler::run_stdio(self).await
    }

    /// 以 HTTP 模式运行服务器。
    ///
    /// - `port`: 监听端口
    pub async fn run_http(self, port: u16) -> Result<(), super::ServerError> {
        let server = std::sync::Arc::new(self);
        super::handler::run_http(server, port).await
    }

    /// 以 SSE 模式运行服务器。
    ///
    /// - `port`: 监听端口
    pub async fn run_sse(self, port: u16) -> Result<(), super::ServerError> {
        let server = std::sync::Arc::new(self);
        super::handler::run_sse(server, port).await
    }
}
