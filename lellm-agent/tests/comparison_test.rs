//! 对比测试：execute()（新 ReAct Graph 路径）vs execute_stream()（旧手写循环路径）
//!
//! 验证两条路径在相同输入下产生等价的结果。

use async_trait::async_trait;
use futures_util::stream;
use lellm_agent::{AgentBuilder, ExecutableTool};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TokenUsage, ToolCall,
    ToolDefinition,
};
use lellm_provider::{LlmProvider, ProviderEvent, ProviderStream, ResolvedModel};
use std::sync::{Arc, Mutex};

/// 检查消息是否为 ToolResult 变体
fn is_tool_result(msg: &Message) -> bool {
    matches!(msg, Message::ToolResult { .. })
}

// ─── 多轮 MockProvider ──────────────────────────────────────────

/// 支持多轮响应的 Mock Provider。
/// 每次 call/stream 调用返回下一个 response。
///
/// 通过 `Arc<MultiResponseMock>` 共享，内部用 Mutex 保护轮次计数。
/// 为每条路径创建独立实例，避免响应竞争。
struct MultiResponseMock {
    responses: Vec<ChatResponse>,
    round: Mutex<usize>,
    received_requests: Mutex<Vec<ChatRequest>>,
}

impl MultiResponseMock {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            round: Mutex::new(0),
            received_requests: Mutex::new(Vec::new()),
        }
    }

    fn next_response(&self) -> ChatResponse {
        let mut r = self.round.lock().unwrap();
        let idx = *r;
        *r += 1;
        self.responses.get(idx).cloned().unwrap_or_else(|| {
            ChatResponse::new(
                vec![ContentBlock::text("Done.")],
                TokenUsage::default(),
                serde_json::json!(null),
            )
        })
    }

    fn received_requests(&self) -> Vec<ChatRequest> {
        self.received_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for MultiResponseMock {
    async fn call(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.received_requests.lock().unwrap().push(req.clone());
        Ok(self.next_response())
    }

    async fn stream(&self, req: &ChatRequest) -> Result<ProviderStream, LlmError> {
        self.received_requests.lock().unwrap().push(req.clone());
        let resp = self.next_response();
        let tool_calls: Vec<ToolCall> = resp.tool_calls().cloned().collect();
        let text: String = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect();

        let events = vec![
            Ok(ProviderEvent::Start {
                model: "mock".into(),
            }),
            Ok(ProviderEvent::Token { token: text }),
            Ok(ProviderEvent::ResponseComplete {
                tool_calls,
                usage: Some(resp.usage),
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    fn provider_id(&self) -> &str {
        "mock"
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────

/// 从 execute_stream() 收集最终 ToolUseResult。
async fn collect_stream_result(
    mut stream: tokio::sync::mpsc::Receiver<lellm_agent::AgentEvent>,
) -> Option<lellm_agent::ToolUseResult> {
    while let Some(event) = stream.recv().await {
        if let lellm_agent::AgentEvent::LoopEnd { result } = event {
            return Some(result);
        }
    }
    None
}

/// 为两条路径分别创建独立的 ResolvedModel（各自有独立的 Mock provider）。
fn make_models(responses: Vec<ChatResponse>, model_name: &str) -> (ResolvedModel, ResolvedModel) {
    let p1 = Arc::new(MultiResponseMock::new(responses.clone()));
    let p2 = Arc::new(MultiResponseMock::new(responses));
    let m1 = ResolvedModel {
        context_window: None,
        provider: p1,
        model: model_name.to_string(),
    };
    let m2 = ResolvedModel {
        context_window: None,
        provider: p2,
        model: model_name.to_string(),
    };
    (m1, m2)
}

fn build_agent(model: ResolvedModel, tools: Vec<ExecutableTool>) -> lellm_agent::ToolUseLoop {
    let mut builder = AgentBuilder::new(model).max_iterations(10);
    for tool in tools {
        builder = builder.tool(tool);
    }
    builder.compile()
}

// ─── Test 1: 简单文本响应（无工具） ─────────────────────────────

#[tokio::test]
async fn compare_simple_text_response() {
    let (model_new, model_old) = make_models(
        vec![ChatResponse::new(
            vec![ContentBlock::text("Hello, world!")],
            TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
            serde_json::json!(null),
        )],
        "test-model",
    );

    let messages = vec![Message::user_text("say hello")];

    // 新路径: execute()
    let result_new = build_agent(model_new, vec![])
        .invoke(messages.clone())
        .await
        .unwrap();

    // 旧路径: execute_stream()
    let stream_old = build_agent(model_old, vec![]).invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    // 等价性断言
    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(result_new.iterations, 1, "should be 1 iteration");
    assert_eq!(
        result_new.response.has_tool_calls(),
        result_old.response.has_tool_calls(),
        "tool_calls flag mismatch"
    );
    assert!(
        !result_new.response.has_tool_calls(),
        "should have no tool calls"
    );
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );
}

// ─── Test 2: 单轮工具调用 ──────────────────────────────────────

#[tokio::test]
async fn compare_single_tool_call() {
    // echo 工具
    let echo_def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo back".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } }
        }),
        cache_control: None,
    };
    let echo_reg = ExecutableTool::safe(echo_def, |args| {
        let m = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(serde_json::json!(format!("echo: {}", m))) }
    });

    // 第 1 轮: 调用 echo("hello")
    // 第 2 轮: 返回最终文本
    let responses = vec![
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"msg": "hello"}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        ChatResponse::new(
            vec![ContentBlock::text("The echo says: echo: hello")],
            TokenUsage {
                prompt_tokens: 20,
                completion_tokens: 8,
                total_tokens: 28,
            },
            serde_json::json!(null),
        ),
    ];
    let (model_new, model_old) = make_models(responses, "test-model");

    let messages = vec![Message::user_text("echo hello")];

    // 新路径
    let result_new = build_agent(model_new, vec![echo_reg.clone()])
        .invoke(messages.clone())
        .await
        .unwrap();

    // 旧路径
    let stream_old = build_agent(model_old, vec![echo_reg]).invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    // 等价性断言
    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(
        result_new.iterations, 2,
        "should be 2 iterations (LLM → Tool → LLM)"
    );
    assert_eq!(
        result_new.tool_calls_executed, result_old.tool_calls_executed,
        "tool_calls_executed mismatch"
    );
    assert_eq!(
        result_new.tool_calls_executed, 1,
        "should execute 1 tool call"
    );
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );

    // 最终响应都不应有 tool calls
    assert!(
        !result_new.response.has_tool_calls(),
        "new: final response should not have tool calls"
    );
    assert!(
        !result_old.response.has_tool_calls(),
        "old: final response should not have tool calls"
    );

    // 两条路径的消息结构可能略有不同（新路径追踪更细粒度），
    // 但关键消息类型必须都存在：用户输入、至少一个工具结果、最终助手响应
    assert!(
        result_new
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { .. })),
        "new: should have user message"
    );
    assert!(
        result_old
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { .. })),
        "old: should have user message"
    );
    assert!(
        result_new.messages.iter().any(|m| is_tool_result(m)),
        "new: should have tool result message"
    );
    assert!(
        result_old.messages.iter().any(|m| is_tool_result(m)),
        "old: should have tool result message"
    );
}

// ─── Test 3: 多轮 ReAct 循环（2 次工具调用） ────────────────────

#[tokio::test]
async fn compare_multi_round_react() {
    // add 工具
    let add_def = ToolDefinition {
        name: "add".to_string(),
        description: "add two numbers".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            }
        }),
        cache_control: None,
    };
    let add_reg = ExecutableTool::safe(add_def, |args| {
        let a = args.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let b = args.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
        async move { Ok(serde_json::json!(a + b)) }
    });

    // 3 轮: add(3,4) → add(7,2) → "answer is 14"
    let responses = vec![
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: serde_json::json!({"a": 3.0, "b": 4.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c2".into(),
                name: "add".into(),
                arguments: serde_json::json!({"a": 7.0, "b": 2.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        ChatResponse::new(
            vec![ContentBlock::text("3 + 4 = 7, 7 + 2 = 9. The answer is 9.")],
            TokenUsage {
                prompt_tokens: 50,
                completion_tokens: 12,
                total_tokens: 62,
            },
            serde_json::json!(null),
        ),
    ];
    let (model_new, model_old) = make_models(responses, "test-model");

    let messages = vec![Message::user_text("add 3+4 then add 2")];

    // 新路径
    let result_new = build_agent(model_new, vec![add_reg.clone()])
        .invoke(messages.clone())
        .await
        .unwrap();

    // 旧路径
    let stream_old = build_agent(model_old, vec![add_reg]).invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    // 等价性断言
    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(result_new.iterations, 3, "should be 3 iterations");
    assert_eq!(
        result_new.tool_calls_executed, result_old.tool_calls_executed,
        "tool_calls_executed mismatch"
    );
    assert_eq!(
        result_new.tool_calls_executed, 2,
        "should execute 2 tool calls"
    );
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );
    // 关键消息类型必须都存在
    assert!(
        result_new
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { .. })),
        "new: should have user message"
    );
    assert!(
        result_old
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { .. })),
        "old: should have user message"
    );
    // 多轮循环应该有多个工具结果
    let new_tool_results: usize = result_new
        .messages
        .iter()
        .filter(|m| is_tool_result(m))
        .count();
    let old_tool_results: usize = result_old
        .messages
        .iter()
        .filter(|m| is_tool_result(m))
        .count();
    assert_eq!(
        new_tool_results, old_tool_results,
        "tool result count mismatch"
    );
    assert_eq!(new_tool_results, 2, "should have 2 tool results");
}

// ─── Test 4: 系统提示词一致性 ──────────────────────────────────

#[tokio::test]
async fn compare_with_system_prompt() {
    let (model_new, model_old) = make_models(
        vec![ChatResponse::new(
            vec![ContentBlock::text("OK")],
            TokenUsage::default(),
            serde_json::json!(null),
        )],
        "test-model",
    );

    let messages = vec![Message::user_text("hello")];

    // 新路径
    let agent_new = AgentBuilder::new(model_new)
        .system("你是测试助手".to_string())
        .max_iterations(5)
        .compile();
    let result_new = agent_new.invoke(messages.clone()).await.unwrap();

    // 旧路径
    let agent_old = AgentBuilder::new(model_old)
        .system("你是测试助手".to_string())
        .max_iterations(5)
        .compile();
    let stream_old = agent_old.invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );
}

// ─── Test 5: 工具注册但不被调用 ────────────────────────────────

#[tokio::test]
async fn compare_tool_registered_but_not_called() {
    let echo_def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } }
        }),
        cache_control: None,
    };
    let echo_reg = ExecutableTool::safe(echo_def, |args| {
        let m = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(serde_json::json!(format!("echo: {}", m))) }
    });

    // LLM 不返回 tool call，直接返回文本
    let responses = vec![ChatResponse::new(
        vec![ContentBlock::text("no tool needed")],
        TokenUsage::default(),
        serde_json::json!(null),
    )];
    let (model_new, model_old) = make_models(responses, "test-model");

    let messages = vec![Message::user_text("hello")];

    // 新路径
    let result_new = build_agent(model_new, vec![echo_reg.clone()])
        .invoke(messages.clone())
        .await
        .unwrap();

    // 旧路径
    let stream_old = build_agent(model_old, vec![echo_reg]).invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(
        result_new.tool_calls_executed, result_old.tool_calls_executed,
        "tool_calls_executed mismatch"
    );
    assert_eq!(
        result_new.tool_calls_executed, 0,
        "should execute 0 tool calls"
    );
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );
}

// ─── Test 6: Provider 收到的请求内容一致 ───────────────────────

#[tokio::test]
async fn compare_provider_requests_equivalent() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("done")],
        TokenUsage::default(),
        serde_json::json!(null),
    );

    // 两条路径各用一个独立 provider
    let provider_new = Arc::new(MultiResponseMock::new(vec![response.clone()]));
    let provider_old = Arc::new(MultiResponseMock::new(vec![response]));

    let model_new = ResolvedModel {
        context_window: None,
        provider: Arc::clone(&provider_new) as Arc<dyn LlmProvider>,
        model: "test-model".to_string(),
    };
    let model_old = ResolvedModel {
        context_window: None,
        provider: Arc::clone(&provider_old) as Arc<dyn LlmProvider>,
        model: "test-model".to_string(),
    };

    let messages = vec![Message::user_text("test request")];

    // 新路径
    let agent_new = build_agent(model_new, vec![]);
    let _result_new = agent_new.invoke(messages.clone()).await.unwrap();

    // 旧路径
    let agent_old = build_agent(model_old, vec![]);
    let stream_old = agent_old.invoke_stream(messages.clone());
    let _ = collect_stream_result(stream_old).await;

    // 两条路径各调用了一次 provider
    let reqs_new = provider_new.received_requests();
    let reqs_old = provider_old.received_requests();
    assert_eq!(reqs_new.len(), 1, "new path should call provider once");
    assert_eq!(reqs_old.len(), 1, "old path should call provider once");

    // 两个请求的 messages 数量应该一致
    assert_eq!(
        reqs_new[0].messages.len(),
        reqs_old[0].messages.len(),
        "request message count should be equivalent"
    );
}

// ─── Test 7: 最大迭代次数截断 ─────────────────────────────────

/// 验证两条路径在持续工具调用时，都正确返回 MaxIterationsReached。
///
/// 新路径 `execute()` 使用 `max_steps = max_iterations * 4 + 1` 作为 graph step 上限，
/// 确保 PostLLMGuard 的 `reached_max()` 检测优先于 step limit 触发。
/// 两条路径的 StopReason 完全一致。
#[tokio::test]
async fn compare_max_iterations_reached() {
    // 20 个 tool call responses — 足够让循环跑满 max_iterations
    let responses: Vec<ChatResponse> = (0..20)
        .map(|i| {
            ChatResponse::new(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("c{}", i),
                    name: "loop".into(),
                    arguments: serde_json::json!({}),
                })],
                TokenUsage::default(),
                serde_json::json!(null),
            )
        })
        .collect();

    let loop_def = ToolDefinition {
        name: "loop".to_string(),
        description: "loop tool".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        cache_control: None,
    };
    let loop_reg =
        ExecutableTool::safe(loop_def, |_| async move { Ok(serde_json::json!("looped")) });

    let (model_new, model_old) = make_models(responses.clone(), "test-model");

    let messages = vec![Message::user_text("loop forever")];

    // 新路径 — max_steps = 3*4+1 = 13, PostLLMGuard 在第 3 次 LLM 后设置 MaxIterationsReached
    let agent_new = AgentBuilder::new(model_new)
        .tool(loop_reg.clone())
        .max_iterations(3)
        .compile();
    let result_new = agent_new.invoke(messages.clone()).await.unwrap();

    // 旧路径 — 明确返回 MaxIterationsReached
    let agent_old = AgentBuilder::new(model_old)
        .tool(loop_reg)
        .max_iterations(3)
        .compile();
    let stream_old = agent_old.invoke_stream(messages.clone());
    let result_old = collect_stream_result(stream_old)
        .await
        .expect("stream ended without LoopEnd");

    // ✅ 核心等价：两条路径都返回 MaxIterationsReached
    assert_eq!(
        result_new.stop_reason, result_old.stop_reason,
        "stop_reason mismatch"
    );
    assert_eq!(
        result_new.stop_reason,
        lellm_agent::StopReason::MaxIterationsReached,
        "both should return MaxIterationsReached"
    );

    // iterations 一致
    assert_eq!(
        result_new.iterations, result_old.iterations,
        "iterations mismatch"
    );
    assert_eq!(result_new.iterations, 3, "should be exactly 3 iterations");

    // tool_calls 一致且有限
    assert_eq!(
        result_new.tool_calls_executed, result_old.tool_calls_executed,
        "tool_calls_executed mismatch"
    );
    assert!(
        result_new.tool_calls_executed >= 1 && result_new.tool_calls_executed < 10,
        "new: should execute limited tool calls, got {}",
        result_new.tool_calls_executed
    );
}
