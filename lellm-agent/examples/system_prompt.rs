//! 系统提示 — 塑造 Agent 的行为
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.agents import create_agent
//!
//! agent = create_agent(
//!     model,
//!     tools,
//!     system_prompt="你是一个有帮助的助手。请简洁准确。"
//! )
//! ```
//!
//! 当未提供 system_prompt 时，智能体将直接从消息中推断其任务。
//!
//! 运行：
//! ```text
//! cargo run --example system_prompt
//! ```

use lellm_agent::{AgentBuilder, ToolUseLoop, create_agent_with_system};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage};
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

/// 使用 AgentBuilder 设置系统提示
fn create_agent_with_builder() -> ToolUseLoop {
    let response = ChatResponse::new(
        vec![ContentBlock::text(
            "根据我的简洁风格：LeLLM 是 Rust 版本的 LangChain，专注于低层编排和精准控制。"
                .to_string(),
        )],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt("你是一个简洁准确的助手。回答不超过两句话。使用技术术语。".to_string())
        .build()
}

/// 使用糖衣 API 设置系统提示
fn create_agent_simple() -> ToolUseLoop {
    let response = ChatResponse::new(
        vec![ContentBlock::text(
            "LeLLM 是 Rust 版本的 LangChain / LangGraph / AutoGen。".to_string(),
        )],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    create_agent_with_system(model, "你是一个专业的技术助手。请用中文回答。".to_string())
}

#[tokio::main]
async fn main() {
    // ─── 示例 1：AgentBuilder 方式 ───
    println!("=== 示例 1: AgentBuilder 设置系统提示 ===\n");

    let agent = create_agent_with_builder();

    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("介绍一下 LeLLM 项目。".to_string()),
        }])
        .await
        .expect("执行失败");

    // 查看系统提示
    for msg in &result.messages {
        if let Message::System { content } = msg {
            println!("[系统提示]");
            for block in content {
                if let ContentBlock::Text(t) = block {
                    println!("  {}", t.text);
                }
            }
            println!();
        }
    }

    // 查看 AI 回复
    println!("[AI 回复]");
    for block in &result.response.content {
        if let ContentBlock::Text(t) = block {
            println!("  {}", t.text);
        }
    }

    // ─── 示例 2：糖衣 API 方式 ───
    println!("\n=== 示例 2: 糖衣 API ===\n");

    let agent2 = create_agent_simple();

    let result2 = agent2
        .execute(vec![Message::User {
            content: lellm_core::text_block("LeLLM 是什么？".to_string()),
        }])
        .await
        .expect("执行失败");

    // 查看系统提示
    for msg in &result2.messages {
        if let Message::System { content } = msg {
            println!("[系统提示]");
            for block in content {
                if let ContentBlock::Text(t) = block {
                    println!("  {}", t.text);
                }
            }
            println!();
        }
    }

    // 查看 AI 回复
    println!("[AI 回复]");
    for block in &result2.response.content {
        if let ContentBlock::Text(t) = block {
            println!("  {}", t.text);
        }
    }

    // ─── 示例 3：无系统提示 ───
    println!("\n=== 示例 3: 无系统提示（从消息推断任务）===\n");

    let response3 = ChatResponse::new(
        vec![ContentBlock::text("好的，我来帮你。".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider3 = Arc::new(MockProvider::reply_with(response3));
    let model3 = ResolvedModel {
        context_window: None,
        provider: provider3,
        model: "test-model".to_string(),
    };

    // 不设置 system_prompt，Agent 直接从用户消息推断任务
    let agent3 = AgentBuilder::new(model3).build();

    let result3 = agent3
        .execute(vec![Message::User {
            content: lellm_core::text_block(
                "你是一个翻译助手。请将以下句子翻译成英文：你好世界".to_string(),
            ),
        }])
        .await
        .expect("执行失败");

    println!("[AI 回复]");
    for block in &result3.response.content {
        if let ContentBlock::Text(t) = block {
            println!("  {}", t.text);
        }
    }
}
