//! 流式 Agent — 实时接收事件
//!
//! 展示如何使用 execute_stream() 获取 AgentEvent 流，
//! 实时处理 Token、工具调用开始/结束、循环结束等事件。
//!
//! 运行：
//! ```text
//! cargo run --example streaming_agent
//! ```

use lellm_agent::{AgentBuilder, AgentEvent};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage};
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // MockProvider — 模拟 LLM 响应
    let response = ChatResponse::new(
        vec![ContentBlock::text(
            "LeLLM 是 Rust 版本的 LangChain，提供 LLM 抽象层、Agent 编排和工具调用系统。"
                .to_string(),
        )],
        TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 40,
            total_tokens: 140,
        },
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "streaming-model".to_string(),
    };

    let agent = AgentBuilder::new(model).build_loop();

    println!("=== 流式 Agent 执行 ===\n");

    // ─── 流式执行 ───
    let messages = vec![Message::user_text("介绍一下 LeLLM。")];
    let mut stream = agent.invoke_stream(messages);

    let mut has_tool_calls = false;

    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv())
        .await
        .ok()
        .flatten()
    {
        match event {
            AgentEvent::Provider(provider_event) => {
                match provider_event {
                    lellm_provider::ProviderEvent::Start { model } => {
                        println!("[开始] model={}", model);
                    }
                    lellm_provider::ProviderEvent::Token { token } => {
                        // 实时打印每个 token
                        print!("{}", token);
                        std::io::Write::flush(&mut std::io::stdout()).ok();
                    }
                    lellm_provider::ProviderEvent::ThinkingDelta { .. } => {
                        // Thinking 内容暂不输出
                    }
                    lellm_provider::ProviderEvent::ResponseComplete { tool_calls, usage } => {
                        println!();
                        if !tool_calls.is_empty() {
                            has_tool_calls = true;
                            println!("[Tool Calls] {} 个工具调用", tool_calls.len());
                        }
                        if let Some(u) = usage {
                            println!(
                                "[Token] prompt={}, completion={}, total={}",
                                u.prompt_tokens, u.completion_tokens, u.total_tokens
                            );
                        }
                    }
                }
            }
            AgentEvent::ToolStart { tool_call_id, name } => {
                println!("[工具开始] id={}, name={}", tool_call_id, name);
            }
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                duration,
            } => {
                println!(
                    "[工具结束] id={}, result={:?}, duration={:.2?}",
                    tool_call_id,
                    match &result {
                        Ok(v) => v.as_str().unwrap_or_else(|| v.to_string().leak()),
                        Err(e) => &e.message,
                    },
                    duration
                );
            }
            AgentEvent::Retry {
                tool_call_id,
                attempt,
                max_attempts,
                reason,
            } => {
                println!(
                    "[重试] id={}, 尝试 {}/{} ({})",
                    tool_call_id, attempt, max_attempts, reason
                );
            }
            AgentEvent::ContextCompacted {
                before_tokens,
                after_tokens,
                removed_messages,
            } => {
                println!(
                    "[上下文压缩] {before_tokens} → {after_tokens} tokens (移除 {removed_messages} 条消息)"
                );
            }
            AgentEvent::LoopEnd { result } => {
                println!("\n[循环结束]");
                println!("  停止原因: {:?}", result.stop_reason);
                println!("  迭代次数: {}", result.iterations);
                println!("  工具调用: {}", result.tool_calls_executed);
            }
            AgentEvent::LoopError { error, iterations } => {
                eprintln!("\n[循环错误] iterations={}, error={}", iterations, error);
            }
        }
    }

    if !has_tool_calls {
        println!("\n[无工具调用 — Agent 直接返回了最终答案]");
    }
}
