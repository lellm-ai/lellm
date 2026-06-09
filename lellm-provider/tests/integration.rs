use lellm_core::{ChatRequest, ChatResponse, ContentBlock, TokenUsage};
use lellm_provider::{LlmProvider, MockProvider, ProviderEvent, StreamOptions};

#[tokio::test]
async fn test_mock_provider_call() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hello".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = MockProvider::reply_with(response);

    let request = ChatRequest::user_prompt("test".to_string());
    let result = provider.call(&request).await.unwrap();

    assert_eq!(result.content.len(), 1);
    assert!(!result.has_tool_calls());
}

#[tokio::test]
async fn test_mock_provider_stream() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hello world".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = MockProvider::reply_with(response);

    let request = ChatRequest::user_prompt("test".to_string());
    let mut stream = provider.stream(&request, &StreamOptions::default()).await.unwrap();

    // 应该收到 Start, Token, ResponseComplete 事件
    let mut events = Vec::new();
    use futures_util::StreamExt;
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }

    assert!(events.len() >= 3);
    assert!(matches!(events[0], ProviderEvent::Start { .. }));
    assert!(matches!(
        events[events.len() - 1],
        ProviderEvent::ResponseComplete { .. }
    ));
}

#[tokio::test]
async fn test_mock_provider_received_requests() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = MockProvider::reply_with(response);

    let request = ChatRequest::user_prompt("test".to_string());
    let _ = provider.call(&request).await.unwrap();

    assert_eq!(provider.received_requests().len(), 1);
}
