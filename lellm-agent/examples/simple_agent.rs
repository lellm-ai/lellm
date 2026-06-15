//! 简单 Agent — 无工具，直接 LLM 对话
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.agents import create_agent
//! agent = create_agent(model)  # 无工具
//! result = agent.invoke("你好")
//! ```
//!
//! 运行：
//! ```text
//! cargo run --example simple_agent
//! ```

use lellm_agent::{AgentBuilder, ToolUseLoop};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage};
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

/// 构建一个无工具的简单 Agent
fn create_simple_agent() -> ToolUseLoop {
    // MockProvider — 模拟 LLM 返回纯文本响应
    let response = ChatResponse::new(
        vec![ContentBlock::text(
            "你好！我是 LeLLM Agent，很高兴为你服务。".to_string(),
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

    // 无工具的 Agent — 仅包含一个 LLM 节点，不具备工具调用能力
    AgentBuilder::new(model).build()
}

#[tokio::main]
async fn main() {
    let agent = create_simple_agent();

    // ─── 非流式执行 ───
    println!("=== 非流式执行 ===");
    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("请介绍一下自己。".to_string()),
        }])
        .await
        .expect("Agent 执行失败");

    println!("停止原因: {:?}", result.stop_reason);
    println!("迭代次数: {}", result.iterations);
    println!("工具调用次数: {}", result.tool_calls_executed);
    println!("\n最终回复:");
    for block in &result.response.content {
        if let ContentBlock::Text(t) = block {
            println!("{}", t.text);
        }
    }
}
