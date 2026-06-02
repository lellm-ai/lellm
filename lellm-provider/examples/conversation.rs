//! 多轮对话 — 携带上下文历史
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain_core.messages import SystemMessage, HumanMessage, AIMessage
//!
//! conversation = [
//!     SystemMessage("你是一个将英语翻译成法语的助手。"),
//!     HumanMessage("翻译: I like programming."),
//!     AIMessage("J'aime la programmation."),
//!     HumanMessage("翻译: I like building apps."),
//! ]
//! response = model.invoke(conversation)
//! ```

#[path = "common/mod.rs"]
mod common;

use lellm_core::{ChatRequest, ContentBlock, LlmError, Message, text_block};
use lellm_provider::LlmProvider;

#[tokio::main]
async fn main() -> Result<(), LlmError> {
    let provider = common::create_openai_provider();

    // ─── 构建对话历史 ───
    let messages: Vec<Message> = vec![
        // 系统提示
        Message::System {
            content: text_block("你是一个将英语翻译成法语的助手。".into()),
        },
        // 第一轮用户
        Message::User {
            content: text_block("翻译: I like programming.".into()),
        },
        // 第一轮助手回复
        Message::Assistant {
            content: text_block("J'aime la programmation.".into()),
        },
        // 第二轮用户
        Message::User {
            content: text_block("翻译: I like building apps.".into()),
        },
    ];

    // ─── 发送请求 ───
    let request = ChatRequest {
        messages,
        ..Default::default()
    };

    //let response = provider.call(&request).await?;
    let response = provider.call(&request).await;
    match response {
        Ok(response) => {
            // ─── 提取并打印响应 ───
            println!("===openai res success");
            for block in &response.content {
                if let ContentBlock::Text(t) = block {
                    println!("{}", t.text);
                }
            }
        }
        Err(e) => println!("{:?}", e),
    }

    println!("\n===create_anthropic_provider===");
    let provider = common::create_anthropic_provider();
    let response = provider.call(&request).await?;
    // ─── 提取并打印响应 ───
    println!("===anthropic res success");
    for block in &response.content {
        if let ContentBlock::Text(t) = block {
            println!("{}", t.text);
        }
    }

    Ok(())
}
