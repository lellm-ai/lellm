//! 工具调用 — ReAct 循环示例
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//! from langchain.agents import create_agent
//!
//! @tool
//! def search_products(query: str) -> str:
//!     """搜索产品。"""
//!     return f"找到产品: {query}"
//!
//! @tool
//! def check_inventory(product_id: str) -> str:
//!     """检查库存。"""
//!     return f"{product_id}: 库存 10 件"
//!
//! agent = create_agent(model, tools=[search_products, check_inventory])
//! ```
//!
//! 智能体遵循 ReAct（推理 + 行动）模式，在推理步骤与工具调用之间交替，
//! 直到能够提供最终答案。
//!
//! 运行：
//! ```text
//! cargo run --example tool_use
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, ToolUseLoop};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage, ToolCall};
use lellm_macros::Tool;
use lellm_provider::ResolvedModel;
use std::sync::Arc;

// ─── 定义工具 ───────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(
    name = "search_products",
    description = "搜索产品目录，返回匹配的产品列表"
)]
struct SearchProductsArgs {
    /// 搜索关键词
    query: String,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "check_inventory", description = "检查指定产品的库存数量")]
struct CheckInventoryArgs {
    /// 产品 ID
    product_id: String,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "get_weather", description = "获取指定位置的天气信息")]
struct GetWeatherArgs {
    /// 城市或地点名称
    location: String,
}

// ─── 模拟 ReAct 循环的 Mock Provider ────────────────────────────

/// 模拟多轮 ReAct 循环：
/// 第 1 轮 → 返回 tool_call（search_products）
/// 第 2 轮 → 返回 tool_call（check_inventory）
/// 第 3 轮 → 返回最终答案
struct ReActMockProvider {
    round_responses: Vec<ChatResponse>,
    current_round: std::sync::Mutex<usize>,
}

impl ReActMockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            round_responses: responses,
            current_round: std::sync::Mutex::new(0),
        }
    }
}

#[::async_trait::async_trait]
impl lellm_provider::LlmProvider for ReActMockProvider {
    async fn call(
        &self,
        _request: &lellm_core::ChatRequest,
    ) -> Result<ChatResponse, lellm_core::LlmError> {
        let round = {
            let mut r = self.current_round.lock().unwrap();
            let current = *r;
            *r += 1;
            current
        };

        if let Some(response) = self.round_responses.get(round) {
            Ok(response.clone())
        } else {
            // 兜底：返回最终答案
            Ok(ChatResponse::new(
                vec![ContentBlock::text("已完成。".to_string())],
                TokenUsage::default(),
                serde_json::json!(null),
            ))
        }
    }

    async fn stream(
        &self,
        _request: &lellm_core::ChatRequest,
    ) -> Result<lellm_provider::ProviderStream, lellm_core::LlmError> {
        unimplemented!("stream not needed for this example")
    }

    fn provider_id(&self) -> &str {
        "react-mock"
    }
}

/// 构建模拟 ReAct 循环的 Agent
fn create_react_agent() -> ToolUseLoop {
    // 第 1 轮响应：调用 search_products
    let search_response = ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_abc123".to_string(),
            name: "search_products".to_string(),
            arguments: serde_json::json!({ "query": "wireless headphones" }),
        })],
        TokenUsage::default(),
        serde_json::json!(null),
    );

    // 第 2 轮响应：调用 check_inventory
    let inventory_response = ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_def456".to_string(),
            name: "check_inventory".to_string(),
            arguments: serde_json::json!({ "product_id": "WH-1000XM5" }),
        })],
        TokenUsage::default(),
        serde_json::json!(null),
    );

    // 第 3 轮响应：最终答案
    let final_response = ChatResponse::new(
        vec![ContentBlock::text(
            "我找到了最受欢迎的无线耳机 — Sony WH-1000XM5，当前库存 10 件，可以购买。".to_string(),
        )],
        TokenUsage {
            prompt_tokens: 500,
            completion_tokens: 50,
            total_tokens: 550,
        },
        serde_json::json!(null),
    );

    let provider = Arc::new(ReActMockProvider::new(vec![
        search_response,
        inventory_response,
        final_response,
    ]));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "react-model".to_string(),
    };

    // 注册工具
    let tools = vec![
        SearchProductsArgs::safe(|args| async move {
            Ok(format!(
                "找到 5 个匹配\"{}\"的产品。前 5 个结果：WH-1000XM5, AirPods Pro, QC45, WF-1000XM4, HD600",
                args.query
            ))
        }),
        CheckInventoryArgs::safe(|args| async move {
            Ok(format!("产品 {}：库存 10 件", args.product_id))
        }),
        GetWeatherArgs::safe(|args| async move {
            Ok(format!("{} 的天气：晴朗，25°C", args.location))
        }),
    ];

    AgentBuilder::new(model)
        .tools(tools)
        .max_iterations(10)
        .build()
}

#[tokio::main]
async fn main() {
    let agent = create_react_agent();

    println!("=== ReAct 工具调用循环 ===\n");

    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("找出当前最受欢迎的无线耳机并检查其库存".to_string()),
        }])
        .await
        .expect("Agent 执行失败");

    // 打印完整的对话历史
    println!("--- 对话历史 ---\n");
    for (_i, msg) in result.messages.iter().enumerate() {
        match msg {
            Message::System { content } => {
                println!("[系统]");
                for block in content {
                    if let ContentBlock::Text(t) = block {
                        println!("  {}", t.text);
                    }
                }
            }
            Message::User { content } => {
                println!("[用户]");
                for block in content {
                    if let ContentBlock::Text(t) = block {
                        println!("  {}", t.text);
                    }
                }
            }
            Message::Assistant { content } => {
                println!("[AI]");
                for block in content {
                    match block {
                        ContentBlock::Text(t) => {
                            println!("  文本: {}", t.text);
                        }
                        ContentBlock::ToolCall(tc) => {
                            println!("  工具调用: {}({})", tc.name, tc.arguments);
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult {
                tool_call_id,
                is_error,
                content,
            } => {
                let status = if *is_error {
                    "❌ 错误"
                } else {
                    "✅ 结果"
                };
                println!("[工具 {}] tool_call_id={}", status, tool_call_id);
                for block in content {
                    if let ContentBlock::Text(t) = block {
                        println!("  {}", t.text);
                    }
                }
            }
        }
        println!();
    }

    // 打印最终回复
    println!("[AI 最终回复]");
    for block in &result.response.content {
        if let ContentBlock::Text(t) = block {
            println!("  {}", t.text);
        }
    }

    // 打印执行摘要
    println!("\n--- 执行摘要 ---");
    println!("停止原因: {:?}", result.stop_reason);
    println!("迭代次数: {}", result.iterations);
    println!("工具调用总数: {}", result.tool_calls_executed);
    println!(
        "Token 消耗: prompt={}, completion={}, total={}",
        result.response.usage.prompt_tokens,
        result.response.usage.completion_tokens,
        result.response.usage.total_tokens,
    );
}
