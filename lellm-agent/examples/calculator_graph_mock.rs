//! Calculator Graph (Mock) — LangGraph Tutorial 的 LeLLM 对照实现
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! LeLLM 的设计：
//! - `AgentBuilder::build()` 返回 `Graph<AgentState>`
//! - 可以直接用 `graph.run_inline()` 执行
//! - 也可以用 `build_loop().invoke()` 便捷执行
//!
//! ```text
//! cargo run -p lellm-graph --example calculator_graph_mock
//! ```

use futures_util::stream;
use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, ResolvedModel};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TokenUsage, ToolCall,
};
use lellm_derive::Tool;
use lellm_provider::{ProviderEvent, ProviderStream};
use std::sync::{Arc, Mutex};

// ─── 工具定义 ───────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "add", description = "Add two numbers")]
struct AddArgs {
    a: f64,
    b: f64,
}

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "multiply", description = "Multiply two numbers")]
struct MultiplyArgs {
    a: f64,
    b: f64,
}

// ─── 模拟 Provider ──────────────────────────────────────────

struct MockProvider {
    responses: Vec<ChatResponse>,
    round: Mutex<usize>,
}

impl MockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            round: Mutex::new(0),
        }
    }

    fn next_response(&self) -> ChatResponse {
        let mut r = self.round.lock().unwrap();
        let idx = *r;
        *r += 1;
        self.responses.get(idx).cloned().unwrap_or_else(|| {
            ChatResponse::new(
                vec![ContentBlock::text("Done.")],
                TokenUsage::default(),
                serde_json::json!(null),
            )
        })
    }
}

#[::async_trait::async_trait]
impl lellm_provider::LlmProvider for MockProvider {
    async fn call(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(self.next_response())
    }

    async fn stream(&self, _req: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let resp = self.next_response();
        let tool_calls: Vec<ToolCall> = resp.tool_calls().cloned().collect();
        let text: String = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect();

        let events = vec![
            Ok(ProviderEvent::Start {
                model: "mock".into(),
            }),
            Ok(ProviderEvent::Token { token: text }),
            Ok(ProviderEvent::ResponseComplete {
                tool_calls,
                usage: Some(resp.usage),
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    fn provider_id(&self) -> &str {
        "mock"
    }
}

// ─── 构建 Agent ─────────────────────────────────────────────

fn build_agent() -> lellm_agent::ToolUseLoop {
    // 模拟 3 轮 ReAct 循环
    let responses = vec![
        // 第 1 轮：调用 add(3, 4)
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: serde_json::json!({"a": 3.0, "b": 4.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        // 第 2 轮：调用 multiply(7, 2)
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c2".into(),
                name: "multiply".into(),
                arguments: serde_json::json!({"a": 7.0, "b": 2.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        // 第 3 轮：返回最终答案
        ChatResponse::new(
            vec![ContentBlock::text("3 + 4 = 7，7 × 2 = 14。答案是 14。")],
            TokenUsage {
                prompt_tokens: 300,
                completion_tokens: 40,
                total_tokens: 340,
            },
            serde_json::json!(null),
        ),
    ];

    let model = ResolvedModel {
        provider: Arc::new(MockProvider::new(responses)),
        model: "mock".into(),
        context_window: None,
    };

    AgentBuilder::new(model)
        .system("你是一个数学助手。".to_string())
        .tools([
            AddArgs::safe(|args| async move { Ok(serde_json::json!(args.a + args.b)) }),
            MultiplyArgs::safe(|args| async move { Ok(serde_json::json!(args.a * args.b)) }),
        ])
        .max_iterations(10)
        .build_loop()
}

#[tokio::main]
async fn main() {
    let agent = build_agent();

    println!("=== Calculator Agent (Mock) ===\n");

    let start = std::time::Instant::now();

    let messages = vec![Message::user_text("3加4等于多少，然后再乘以2。")];
    let result = agent.invoke(messages).await.expect("Agent 执行失败");

    let duration = start.elapsed();

    println!("\n=== 执行完成 ===");
    println!("总耗时: {}ms", duration.as_millis());

    println!("\n=== 对话历史 ===");
    for msg in &result.messages {
        match msg {
            Message::User { content } => {
                print!("[用户] ");
                for block in content {
                    if let lellm_core::ContentBlock::Text(t) = block {
                        print!("{}", t.text);
                    }
                }
                println!();
            }
            Message::Assistant { content } => {
                print!("[AI] ");
                for block in content {
                    match block {
                        lellm_core::ContentBlock::Text(t) => {
                            print!("{}", t.text);
                        }
                        lellm_core::ContentBlock::ToolCall(tc) => {
                            print!("[调用 {}({})]", tc.name, tc.arguments);
                        }
                        _ => {}
                    }
                }
                println!();
            }
            Message::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                print!("[工具结果 {tool_call_id}] ");
                for block in content {
                    if let lellm_core::ContentBlock::Text(t) = block {
                        print!("{}", t.text);
                    }
                }
                println!();
            }
            _ => {}
        }
    }

    println!("\n=== 执行摘要 ===");
    println!("停止原因: {:?}", result.stop_reason);
    println!("迭代次数: {}", result.iterations);
    println!("工具调用: {}", result.tool_calls_executed);
}
