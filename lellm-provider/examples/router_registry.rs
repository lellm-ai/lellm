//! 模型路由 — 多 Provider 注册与任务分级
//!
//! 对应 LangChain 用法：
//! ```python
//! # 根据任务复杂度切换模型
//! fast_model  = init_chat_model("openai:gpt-4o-mini")
//! smart_model = init_chat_model("anthropic:claude-sonnet-4-5")
//! ```
//!
//! LeLLM 通过 ModelRouter + ProviderRegistry 实现三级路由：
//! - Flash    → 快速/便宜（如简单问答）
//! - Standard → 默认（如一般对话）
//! - Pro      → 复杂推理（如代码生成）

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use lellm_core::{ChatRequest, LlmError};
use lellm_provider::{ModelRouter, ProviderRegistry, ResolvedModel, TaskLevel};

#[tokio::main]
async fn main() -> Result<(), LlmError> {
    // ─── 1. 注册 Provider ───
    let mut registry = ProviderRegistry::new();

    // OpenAI
    registry.register("openai", Arc::new(common::create_openai_provider()));

    // Anthropic（从环境变量读取）
    registry.register("anthropic", Arc::new(common::create_anthropic_provider()));

    // ─── 2. 配置路由表 ───
    let mut router = ModelRouter::new();
    router.add_route(
        TaskLevel::Flash,
        lellm_provider::RouteEntry {
            provider_id: "openai".into(),
            model: "gpt-4o-mini".into(),
        },
    );
    router.add_route(
        TaskLevel::Standard,
        lellm_provider::RouteEntry {
            provider_id: "openai".into(),
            model: "gpt-4.1".into(),
        },
    );
    router.add_route(
        TaskLevel::Pro,
        lellm_provider::RouteEntry {
            provider_id: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
        },
    );

    // ─── 3. 根据任务级别解析并调用 ───
    let tasks = [
        (TaskLevel::Flash, "1+1 等于几？"),
        (TaskLevel::Standard, "简述光合作用的过程。"),
        (TaskLevel::Pro, "用 Rust 实现一个线程安全的 LRU Cache。"),
    ];

    for (level, prompt) in &tasks {
        let route = router.resolve(*level).expect("route not configured");
        let resolved: ResolvedModel = registry.resolve(route)?;

        eprintln!(
            "[{:?}] provider={}, model={}",
            level,
            resolved.provider.provider_id(),
            resolved.model,
        );

        let request = ChatRequest {
            model: resolved.model.clone(),
            messages: vec![lellm_core::Message::user_text(prompt)],
            ..Default::default()
        };

        let response = resolved.provider.call(&request).await?;
        for block in &response.content {
            if let lellm_core::ContentBlock::Text(t) = block {
                println!("{}\n", t.text);
            }
        }
    }

    Ok(())
}
