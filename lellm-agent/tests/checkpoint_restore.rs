//! Agent Checkpoint Phase 2 — 四组必测场景。
//!
//! 崩溃模拟：save_fn 在 `next_node` 命中目标时拒绝保存并报错，
//! 执行在该边界停止（不越过），前一检查点成为 latest（复刻设计 §6 崩溃窗口）。
//! 工具重放被接受（不承诺 exactly-once）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::stream;
use lellm_agent::{AgentBuilder, AgentState, ExecutableTool, ResolvedModel, StopReason};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, RestoreFailureReason, TokenUsage,
    ToolCall, ToolDefinition, ToolSchema,
};
use lellm_graph::{
    BlobCheckpointStore, Checkpoint, CheckpointCodec, CheckpointConfig, CheckpointStoreError,
    FileBlobStore, InMemoryBlobStore, NodeId, SerdeCheckpointCodec, TraceId,
};
use lellm_provider::{LlmProvider, ProviderEvent, ProviderStream};

// ─── 脚本化 Provider（MockProvider 只能返回首个；此处按序弹出）──────────────

struct ScriptedProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            call_count: AtomicUsize::new(0),
        }
    }
    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
    fn next(&self) -> Result<ChatResponse, LlmError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(LlmError::Provider {
                provider: "scripted".into(),
                status: Some(500),
                code: None,
                message: "no scripted response left".into(),
            })
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn call(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.next()
    }
    async fn stream(&self, _request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let response = self.next()?;
        let events: Vec<Result<ProviderEvent, LlmError>> = vec![
            Ok(ProviderEvent::Start {
                model: String::new(),
            }),
            Ok(ProviderEvent::Token {
                token: response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            }),
            Ok(ProviderEvent::ResponseComplete {
                tool_calls: response.tool_calls().cloned().collect(),
                usage: Some(response.usage),
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
    fn provider_id(&self) -> &str {
        "scripted"
    }
}

// ─── 测试工具 ─────────────────────────────────────────────────────

fn tool_call_response() -> ChatResponse {
    ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "calc".into(),
            arguments: serde_json::json!({"expr": "6*7"}),
        })],
        TokenUsage::default(),
        serde_json::Value::Null,
    )
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse::new(
        lellm_core::text_block(text),
        TokenUsage::default(),
        serde_json::Value::Null,
    )
}

fn make_tool(counter: Arc<AtomicUsize>) -> ExecutableTool {
    let def = ToolDefinition {
        name: "calc".to_string(),
        description: "calculator".to_string(),
        parameters: ToolSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "expr": { "type": "string" } }
        })),
        cache_control: None,
    };
    ExecutableTool::safe(def, move |_args: &serde_json::Value| {
        let c = counter.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!("42"))
        }
    })
}

/// 构造「在 next_node 命中 target 时拒绝保存」的 config（崩溃模拟）。
fn crash_config(
    store: Arc<InMemoryBlobStore>,
    codec: SerdeCheckpointCodec<AgentState>,
    hash: u64,
    target: &str,
) -> CheckpointConfig<AgentState> {
    let t = target.to_string();
    let config_store = store.clone();
    CheckpointConfig::new(
        move |cp: Checkpoint<AgentState>, tid: TraceId| {
            let s = store.clone();
            let c = codec.clone();
            let h = hash;
            let t = t.clone();
            Box::pin(async move {
                if cp.next_node.as_ref().map(|n| n.0.as_str()) == Some(t.as_str()) {
                    return Err(CheckpointStoreError::Storage("simulated crash".into()));
                }
                let blob = c.serialize(&cp, h)?;
                s.save_with_trace(&tid, &blob).await
            })
        },
        hash,
    )
    .with_store(config_store)
}

// ─── Group 1: 恢复正确性 ─────────────────────────────────────────

/// G1a: 崩溃在 tool 后（latest = next=tool）→ 恢复执行工具、不重调已完成的 LLM。
#[tokio::test]
async fn g1a_restore_at_tool_executes_tool_no_rellm() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("answer: 42"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();

    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=budget_check（tool 执行后）→ latest = next=tool
    let config_a = crash_config(store.clone(), codec.clone(), hash, "budget_check");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("what is 6*7?")], trace_id, config_a)
        .await;
    assert!(result_a.is_err(), "phase A 应在 tool 边界崩溃");

    // 最新检查点 = next=tool，last_response 已入 checkpoint
    let blob = store.load_latest(&trace_id).await.unwrap().expect("latest");
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("tool".into())));
    assert!(
        cp.state.last_response.is_some(),
        "last_response 必须已入 checkpoint"
    );

    // 阶段 B：恢复（正常 config）
    tool_calls.store(0, Ordering::SeqCst); // 只统计恢复后的工具执行
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent
        .invoke_with_restore(cp, trace_id, config_b)
        .await
        .unwrap();

    let answer = ContentBlock::flatten_text(&result_b.response.content);
    assert!(answer.contains("42"), "最终回答存在, got: {answer}");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "恢复后工具执行一次");
    assert_eq!(
        provider.call_count(),
        2,
        "LLM 恰好两次（恢复后未重调已完成的 LLM）"
    );
    assert_eq!(result_b.stop_reason, StopReason::Complete);
}

/// G1b: 崩溃在 post_llm_check 前（latest = next=post_llm_check）→ PostLLMGuard 正确读取 last_response 路由到 tool（而非误判 Complete）。
#[tokio::test]
async fn g1b_restore_at_post_llm_check_guard_reads_last_response() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("done"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();

    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=tool → latest = next=post_llm_check（post_llm_check 未执行）
    let config_a = crash_config(store.clone(), codec.clone(), hash, "tool");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config_a)
        .await;
    assert!(result_a.is_err());

    let blob = store.load_latest(&trace_id).await.unwrap().expect("latest");
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("post_llm_check".into())));
    assert!(cp.state.last_response.is_some());

    // 阶段 B：恢复 — 若 last_response 丢失，PostLLMGuard 会误判 Complete（tool 不执行）
    tool_calls.store(0, Ordering::SeqCst);
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent
        .invoke_with_restore(cp, trace_id, config_b)
        .await
        .unwrap();

    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        1,
        "PostLLMGuard 读取 last_response 路由到 tool（非 Complete）"
    );
    assert!(ContentBlock::flatten_text(&result_b.response.content).contains("done"));
    assert_eq!(provider.call_count(), 2);
}

// ─── Group 3: MissingExecutionContext ───────────────────────────

/// G3: 恢复目标节点需要 last_response 但检查点缺失 → MissingExecutionContext。
#[tokio::test]
async fn g3_missing_last_response_rejected() {
    // (a) 直接构造（store=None，跳过最新性）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        // last_response 默认 None
        let cp = Checkpoint::new(Some(NodeId("tool".into())), &state, hash, 3);
        let config = CheckpointConfig::new(
            |_cp: Checkpoint<AgentState>, _tid: TraceId| {
                Box::pin(async { Ok::<(), CheckpointStoreError>(()) })
            },
            hash,
        );
        let err = agent
            .invoke_with_restore(cp, TraceId::new(), config)
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => {
                assert_eq!(reason, RestoreFailureReason::MissingExecutionContext);
            }
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }

    // (b) 存储往返（store 存在，最新性通过，last_response 校验失败）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<AgentState>::new();
        let trace_id = TraceId::new();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let cp = Checkpoint::new(Some(NodeId("tool".into())), &state, hash, 3);
        let blob = codec.serialize(&cp, hash).unwrap();
        store.save_with_trace(&trace_id, &blob).await.unwrap();
        let loaded = store.load_latest(&trace_id).await.unwrap().unwrap();
        let cp_loaded = codec.deserialize(&loaded, hash).unwrap();
        let config = CheckpointConfig::for_store(store, codec, hash);
        let err = agent
            .invoke_with_restore(cp_loaded, trace_id, config)
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => {
                assert_eq!(reason, RestoreFailureReason::MissingExecutionContext);
            }
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }

    // (c) 对照：next=budget_check + last_response=None → 正常恢复（不误报）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
        let config = CheckpointConfig::new(
            |_cp: Checkpoint<AgentState>, _tid: TraceId| {
                Box::pin(async { Ok::<(), CheckpointStoreError>(()) })
            },
            hash,
        );
        let result = agent
            .invoke_with_restore(cp, TraceId::new(), config)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Complete);
    }
}
