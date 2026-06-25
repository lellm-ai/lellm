//! 快速开始 — 最简 Provider 调用
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.chat_models import init_chat_model
//! model = init_chat_model("openai:gpt-4.1")
//! response = model.invoke("你好")
//! ```
//!
//! 运行（需实现 Adapter 后，当前 Adapter 为 Stub）：
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example quickstart
//! ```

#[path = "common/mod.rs"]
mod common;

use lellm_core::{ChatRequest, ContentBlock, LlmError};
use lellm_provider::LlmProvider;

#[tokio::main]
async fn main() -> Result<(), LlmError> {
    // ─── 1. 初始化 Provider ───
    let provider = common::create_openai_provider();

    // ─── 2. 单条消息调用 ───
    let request = ChatRequest::user_prompt("为什么鹦鹉有五颜六色的羽毛？").with_temperature(0.7);

    let response = provider.call(&request).await?;
    println!("--- 响应 ---");
    for block in &response.content {
        if let ContentBlock::Text(t) = block {
            print!("{}", t.text);
        }
    }
    println!();

    // ─── 3. 打印 token 消耗 ───
    println!(
        "Token: prompt={}, completion={}, total={}",
        response.usage.prompt_tokens, response.usage.completion_tokens, response.usage.total_tokens,
    );

    Ok(())
}
