//! 流式调用 — 实时接收 Token
//!
//! 对应 LangChain 用法：
//! ```python
//! for chunk in model.stream("Hello"):
//!     print(chunk.content, end="", flush=True)
//! ```

#[path = "common/mod.rs"]
mod common;

use futures_util::StreamExt;
use lellm_core::{ChatRequest, LlmError, ToolCall};
use lellm_provider::{LlmProvider, ProviderEvent, StreamOptions};

#[tokio::main]
async fn main() -> Result<(), LlmError> {
    let provider = common::create_openai_provider();

    let request = ChatRequest::user_prompt("用三句话介绍 Rust 编程语言。".into());

    // ─── 流式调用 ───
    let mut stream = provider.stream(&request, &StreamOptions::default()).await?;

    let mut tool_calls: Vec<ToolCall> = Vec::new();

    while let Some(event) = stream.next().await {
        match event? {
            ProviderEvent::Start { model } => {
                eprintln!("[开始] model={}", model);
            }
            ProviderEvent::Token { token } => {
                // 实时打印每个 token
                print!("{}", token);
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
            ProviderEvent::ThinkingDelta { thinking: _, .. } => {
                // Thinking 内容暂不输出（v0.1）
            }
            ProviderEvent::ResponseComplete {
                tool_calls: tc,
                usage,
            } => {
                println!();
                tool_calls = tc;
                if let Some(u) = usage {
                    eprintln!(
                        "[完成] tokens={}, usage=prompt={},completion={}",
                        u.total_tokens, u.prompt_tokens, u.completion_tokens,
                    );
                }
            }
        }
    }

    // ─── 检查是否有 tool_calls ───
    if !tool_calls.is_empty() {
        eprintln!("[Tool Calls] {} 个工具调用", tool_calls.len());
        for tc in &tool_calls {
            eprintln!("  - {}({})", tc.name, tc.arguments);
        }
    }

    Ok(())
}
