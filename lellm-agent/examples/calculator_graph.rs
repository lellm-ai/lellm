//! Calculator Graph — LangGraph Tutorial 的 LeLLM 对照实现（真实 Provider）
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! LeLLM v0.5 的设计：
//! - `AgentBuilder::build()` 返回 `Arc<Graph<AgentState>>`
//! - 可以直接用 `graph.run_inline()` 执行
//! - 也可以用 `build_loop().invoke()` 便捷执行
//!
//! 使用 OpenAI 兼容的 LLaMA Provider：
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run -p lellm-graph --example calculator_graph
//! # 或 Ollama:
//! OPENAI_API_BASE=http://localhost:11434/v1 OPENAI_API_KEY=ollama cargo run -p lellm-graph --example calculator_graph
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, ResolvedModel};
use lellm_core::Message;
use lellm_derive::Tool;
use lellm_provider::providers::base::CodecProvider;
use lellm_provider::providers::openai_compat::OpenAICompatCodec;
use std::sync::Arc;

// ─── 工具定义 ───────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "add", description = "将两个数字相加")]
struct AddArgs {
    /// 第一个数字
    a: f64,
    /// 第二个数字
    b: f64,
}

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "multiply", description = "将两个数字相乘")]
struct MultiplyArgs {
    /// 第一个数字
    a: f64,
    /// 第二个数字
    b: f64,
}

// ─── 构建 Agent ─────────────────────────────────────────────

fn build_agent() -> lellm_agent::ToolUseLoop {
    // 创建 Provider（从环境变量读取 API Key）
    let provider =
        CodecProvider::load(OpenAICompatCodec::llama()).expect("请设置 OPENAI_API_KEY 环境变量");

    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "llama3.2".into(),
        context_window: Some(8192),
    };

    AgentBuilder::new(model)
        .system("你是一个数学助手。当用户问数学问题时，使用工具计算。".to_string())
        .tools([
            AddArgs::safe(|args| async move { Ok(serde_json::json!(args.a + args.b)) }),
            MultiplyArgs::safe(|args| async move { Ok(serde_json::json!(args.a * args.b)) }),
        ])
        .max_iterations(10)
        .build_loop()
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=trace,lellm_provider=trace,info".into()),
        )
        .try_init();

    let agent = build_agent();

    println!("=== Calculator Agent (LLaMA) ===\n");

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
