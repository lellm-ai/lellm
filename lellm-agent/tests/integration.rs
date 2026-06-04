use lellm_agent::{ToolCategory, ToolExecutor, ToolRegistration, ToolUseLoop};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage, ToolCall};
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

#[tokio::test]
async fn test_tool_use_loop_no_tool_calls() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hello".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        provider,
        model: "test-model".to_string(),
    };

    let executor = ToolExecutor::new();
    let messages = vec![Message::User {
        content: lellm_core::text_block("test".to_string()),
    }];

    let result = ToolUseLoop::new(model, executor)
        .set_max_iterations(5)
        .execute(messages)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(!result.response.has_tool_calls());
}

#[tokio::test]
async fn test_tool_executor_register_and_execute() {
    let mut executor = ToolExecutor::new();
    executor.register(
        "echo",
        ToolRegistration::safe(|args: &serde_json::Value| {
            let args_clone = args.clone();
            async move {
                let text = args_clone
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(format!("echo: {}", text))
            }
        }),
    );

    let call = ToolCall {
        id: "1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::json!({"text": "hello"}),
    };

    let result = executor.execute(&call).await;
    assert!(matches!(result, Ok(ref s) if s == "echo: hello"));
}

#[test]
fn test_tool_category() {
    assert_eq!(ToolCategory::FILE_IO.0, "file_io");
    assert_eq!(ToolCategory::NETWORK.0, "network");
    assert_eq!(ToolCategory::DATABASE.0, "database");
}
