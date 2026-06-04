//! Mock 测试 — 无需 API Key 即可验证调用链路
//!
//! 编译：
//! ```text
//! cargo run --example mock_test --features mock
//! ```

#[cfg(feature = "mock")]
fn main() {
    use lellm_core::{ChatRequest, ChatResponse, ContentBlock, TokenUsage, text_block};
    use lellm_provider::LlmProvider;
    use lellm_provider::providers::mock::MockProvider;

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        // ─── 1. 构造预设响应 ───
        let response = ChatResponse::new(
            text_block("Hello from MockProvider!".into()),
            TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 8,
                total_tokens: 18,
            },
            serde_json::json!(null),
        );

        let provider = MockProvider::reply_with(response);

        // ─── 2. 非流式调用 ───
        let request = ChatRequest::user_prompt("你好".into());
        let resp: lellm_core::ChatResponse = provider.call(&request).await.expect("call failed");

        println!("provider_id = {}", provider.provider_id());
        for block in &resp.content {
            if let ContentBlock::Text(t) = block {
                println!("content   = {}", t.text);
            }
        }
        println!(
            "usage     = prompt={}, completion={}",
            resp.usage.prompt_tokens, resp.usage.completion_tokens,
        );

        // ─── 3. 验证请求被正确接收 ───
        let received = provider.received_requests();
        println!("received  = {} request(s)", received.len());
        let first_block = &received[0].messages[0].content()[0];
        if let Some(text) = first_block.as_text() {
            println!("first msg   = {}", text);
        }
    });
}

#[cfg(not(feature = "mock"))]
fn main() {
    println!(
        "This example requires the `mock` feature: cargo run --example mock_test --features mock"
    );
}
