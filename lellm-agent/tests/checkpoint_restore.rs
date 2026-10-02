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
    BlobCheckpointStore, Checkpoint, CheckpointBlob, CheckpointCodec, CheckpointConfig,
    CheckpointId, CheckpointStoreError, FileBlobStore, InMemoryBlobStore, NodeId,
    SerdeCheckpointCodec, TraceId,
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

/// 读取 trace 下的磁盘 seq 列表（FileBlobStore 文件名前缀 `{seq}_`）。
fn trace_seqs(store: &FileBlobStore, trace_id: &TraceId) -> Vec<u64> {
    let dir = store.root().join(trace_id.to_string());
    std::fs::read_dir(&dir)
        .expect("trace dir")
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.split('_').next()?.parse::<u64>().ok()
        })
        .collect()
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

// ─── Group 2: 恢复后再次保存及恢复（预算延续 + 磁盘往返）──────────

/// G2: 恢复后续写同一 trace（FileBlobStore 磁盘往返）；Graph 步数预算 + Agent 业务预算均延续（未重置）。
#[tokio::test]
async fn g2_budget_continues_and_disk_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn lellm_graph::BlobCheckpointStore> =
        Arc::new(FileBlobStore::new(tmp.path().to_path_buf()));
    // 检视句柄 — 与 store 共享同一 root，仅用于读磁盘 seq（FileBlobStore 非 Clone）
    let inspect = FileBlobStore::new(tmp.path().to_path_buf());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("final"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();
    let hash = agent.graph().canonical_hash();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=budget_check（tool 后）→ latest = next=tool（steps_used=3）
    let crash_store: Arc<InMemoryBlobStore> = Arc::new(InMemoryBlobStore::new());
    let config_a = crash_config(crash_store.clone(), codec.clone(), hash, "budget_check");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config_a)
        .await;
    assert!(result_a.is_err());

    // 把阶段 A 的检查点搬到磁盘 store（模拟真实持久化路径）
    let blob = crash_store
        .load_latest(&trace_id)
        .await
        .unwrap()
        .expect("latest");
    store.save_with_trace(&trace_id, &blob).await.unwrap();
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("tool".into())));
    let steps_used_a = cp.steps_used;
    assert_eq!(steps_used_a, 3, "阶段 A 最新检查点 steps_used=3");

    // 恢复前记录磁盘 seq（spec §12 组 2：检查点序号延续）
    let seqs_before = trace_seqs(&inspect, &trace_id);
    assert_eq!(seqs_before.len(), 1, "阶段 A 恰好写入 1 个检查点到磁盘");
    let max_seq_before = *seqs_before.iter().max().unwrap();

    // 阶段 B：从磁盘 store 恢复并续写（同一 trace）
    tool_calls.store(0, Ordering::SeqCst);
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent
        .invoke_with_restore(cp, trace_id, config_b)
        .await
        .unwrap();

    // 磁盘往返：最终检查点从磁盘加载
    let final_blob = store
        .load_latest(&trace_id)
        .await
        .unwrap()
        .expect("final on disk");
    let final_cp = codec.deserialize(&final_blob, hash).unwrap();
    assert!(
        final_cp.steps_used > steps_used_a,
        "Graph 步数预算延续未重置 ({} > {})",
        final_cp.steps_used,
        steps_used_a
    );
    assert_eq!(final_cp.next_node, None, "完成态");

    // 磁盘序号延续：同一 trace 严格递增；已有检查点未被覆盖（不要求连续无间隙）
    let seqs_after = trace_seqs(&inspect, &trace_id);
    let max_seq_after = *seqs_after.iter().max().unwrap();
    assert!(
        max_seq_after > max_seq_before,
        "同一 trace 下磁盘序号严格递增 ({max_seq_after} > {max_seq_before})"
    );
    assert!(
        seqs_after.len() > seqs_before.len(),
        "恢复后续写产生了新检查点"
    );
    for s in &seqs_before {
        assert!(seqs_after.contains(s), "已有检查点 seq={s} 未被覆盖");
    }

    // 最新检查点可再次恢复（完成态：零执行，不产生新检查点）
    let config_c = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_c = agent
        .invoke_with_restore(final_cp.clone(), trace_id, config_c)
        .await
        .unwrap();
    assert_eq!(result_c.stop_reason, StopReason::Complete);
    assert_eq!(
        trace_seqs(&inspect, &trace_id),
        seqs_after,
        "完成态恢复零执行，磁盘 seq 集合不变"
    );

    // Agent 业务预算：iterations 延续（未重置为 1）
    assert!(
        result_b.iterations >= 2,
        "iterations 延续, got: {}",
        result_b.iterations
    );
    assert!(ContentBlock::flatten_text(&result_b.response.content).contains("final"));
    // 磁盘上确实有检查点文件
    assert!(tmp.path().exists());
}

/// G2b: 恢复后业务预算（max_iterations）实际触发上限 —
/// 不仅「计数器未重置」，还证明预算机制在恢复后仍强制执行 cap。
///
/// max_iterations=2：阶段 A 消耗 1 轮（tool 后崩溃），阶段 B 恢复后仅再跑 1 轮，
/// 第 2 轮 post_llm_check 命中 MaxIterationsReached。
/// 若恢复后预算被重置，将发起第 3 次 LLM 调用（脚本耗尽 → 报错），本测试即失败。
#[tokio::test]
async fn g2b_max_iterations_triggers_after_restore() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    // 两轮均返回 tool_calls — 循环「想继续」，停止只能来自预算
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        tool_call_response(),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(2)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();
    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=budget_check（tool 后）→ latest = next=tool（已消耗 1 轮）
    let config_a = crash_config(store.clone(), codec.clone(), hash, "budget_check");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config_a)
        .await;
    assert!(result_a.is_err(), "阶段 A 应在 tool 边界崩溃");

    let blob = store.load_latest(&trace_id).await.unwrap().expect("latest");
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("tool".into())));
    assert_eq!(cp.state.iterations, 1, "阶段 A 恰好消耗 1 轮迭代");

    // 阶段 B：恢复续跑 — 预算必须在第 2 轮触发上限
    tool_calls.store(0, Ordering::SeqCst);
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent
        .invoke_with_restore(cp, trace_id, config_b)
        .await
        .unwrap();

    // 预算机制在恢复后仍强制执行 cap（而非获得全新完整预算）
    assert_eq!(
        result_b.stop_reason,
        StopReason::MaxIterationsReached,
        "恢复后第 2 轮命中 max_iterations 上限"
    );
    assert_eq!(result_b.iterations, 2, "iterations 延续至上限（未重置）");
    assert_eq!(
        provider.call_count(),
        2,
        "LLM 恰好两次（阶段 A 1 次 + 阶段 B 1 次），无第 3 次"
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "恢复后工具重放一次");
}

// ─── Group 4: 运行期保存失败接线 ─────────────────────────────────

/// G4: 运行期 checkpoint 保存失败 → 执行在该边界停止，错误映射为 Provider（非 RestoreFailed）。
#[tokio::test]
async fn g4_runtime_save_failure_maps_to_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("x"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();

    // save_fn 总是失败（模拟磁盘满）
    let config = CheckpointConfig::new(
        |_cp: Checkpoint<AgentState>, _tid: TraceId| {
            Box::pin(async { Err(CheckpointStoreError::Storage("disk full".into())) })
        },
        hash,
    );
    let err = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], TraceId::new(), config)
        .await
        .unwrap_err();
    // 运行期保存失败 → Provider（非 RestoreFailed）
    match err {
        LlmError::Provider { provider, .. } => {
            assert_eq!(provider, "react_graph");
        }
        other => panic!("expected Provider, got {other:?}"),
    }
    // spec §12 Group 4：执行在保存失败边界停止 — 边界后的 LLM（llm 节点）从未被调用
    assert_eq!(provider.call_count(), 0, "保存失败边界后的 LLM 未执行");
}

/// G4b: 恢复入口运行期保存失败同样映射为 Provider。
#[tokio::test]
async fn g4b_restore_runtime_save_failure_maps_to_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();
    let state = AgentState::from_messages(vec![Message::user_text("q")]);
    // 合法检查点（next=budget_check，last_response=None 允许）
    let cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
    // save_fn 总是失败
    let config = CheckpointConfig::new(
        |_cp: Checkpoint<AgentState>, _tid: TraceId| {
            Box::pin(async { Err(CheckpointStoreError::Storage("disk full".into())) })
        },
        hash,
    );
    let err = agent
        .invoke_with_restore(cp, TraceId::new(), config)
        .await
        .unwrap_err();
    match err {
        LlmError::Provider { provider, .. } => {
            assert_eq!(provider, "react_graph");
        }
        other => panic!("expected Provider, got {other:?}"),
    }
    // spec §12 Group 4：恢复入口同样在保存失败边界停止 — 边界后的 LLM 从未被调用
    assert_eq!(
        provider.call_count(),
        0,
        "恢复入口保存失败边界后的 LLM 未执行"
    );
}

// ─── 收尾项：恢复失败原因类型化（按类型映射，非字符串分类）──────

/// 无 store config（跳过最新性检查）— 直接构造检查点做校验层测试。
fn no_store_config(hash: u64) -> CheckpointConfig<AgentState> {
    CheckpointConfig::new(
        |_cp: Checkpoint<AgentState>, _tid: TraceId| {
            Box::pin(async { Ok::<(), CheckpointStoreError>(()) })
        },
        hash,
    )
}

/// 收尾项 1：恢复失败原因按类型映射 — `GraphMismatch` / `UnsupportedFormat`
/// 各得对应 reason（不是 `Other` 兜底），可供程序识别。
#[tokio::test]
async fn restore_validation_reasons_are_typed_not_other() {
    // (a) graph_hash 不匹配 → GraphMismatch
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let mut cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
        cp.graph_hash = hash ^ 0xff; // 模拟图结构已变更
        let err = agent
            .invoke_with_restore(cp, TraceId::new(), no_store_config(hash))
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => assert_eq!(
                reason,
                RestoreFailureReason::GraphMismatch,
                "hash 不匹配必须得 GraphMismatch（而非 Other）"
            ),
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }

    // (b) format_version 不支持 → UnsupportedFormat
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let mut cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
        cp.format_version = 999; // legacy / 未来版本
        let err = agent
            .invoke_with_restore(cp, TraceId::new(), no_store_config(hash))
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => assert_eq!(
                reason,
                RestoreFailureReason::UnsupportedFormat,
                "format_version 不支持必须得 UnsupportedFormat（而非 Other）"
            ),
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }
}

// ─── 条件核对：assert_fresh_trace 错误语义 ─────────────────────

/// 条件核对：trace 已有检查点 → `InvalidRequest`（调用方用错入口，属用户输入错误）。
#[tokio::test]
async fn fresh_trace_existing_checkpoint_maps_to_invalid_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();
    // trace 已有检查点
    let state = AgentState::from_messages(vec![Message::user_text("q")]);
    let cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
    let blob = codec.serialize(&cp, hash).unwrap();
    store.save_with_trace(&trace_id, &blob).await.unwrap();

    let config = CheckpointConfig::for_store(store, codec, hash);
    let err = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config)
        .await
        .unwrap_err();
    match err {
        LlmError::InvalidRequest { .. } => {}
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    // 新鲜度校验在 LLM 调用之前 — 未执行任何 LLM
    assert_eq!(provider.call_count(), 0, "新鲜度校验失败后 LLM 未执行");
}

/// load_latest 总是失败的 store（其余操作委托内存 store）— 模拟存储读取故障。
struct LoadFailingStore {
    inner: InMemoryBlobStore,
}

#[async_trait]
impl BlobCheckpointStore for LoadFailingStore {
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        blob: &CheckpointBlob,
    ) -> Result<(), CheckpointStoreError> {
        self.inner.save_with_trace(trace_id, blob).await
    }

    async fn load(
        &self,
        id: &CheckpointId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        self.inner.load(id).await
    }

    async fn load_latest(
        &self,
        _trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        Err(CheckpointStoreError::Storage(
            "simulated load failure".into(),
        ))
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
        self.inner.list(trace_id).await
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        self.inner.delete(id).await
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
        self.inner.prune(trace_id, keep).await
    }
}

/// 条件核对：assert_fresh_trace 存储读取失败 → `Provider`（存储故障语义），
/// 不是 `InvalidRequest`（用户输入错误）；也不会被当作「没有检查点」继续执行。
#[tokio::test]
async fn fresh_trace_storage_failure_maps_to_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();
    let store: Arc<dyn BlobCheckpointStore> = Arc::new(LoadFailingStore {
        inner: InMemoryBlobStore::new(),
    });
    let codec = SerdeCheckpointCodec::<AgentState>::new();

    let config = CheckpointConfig::for_store(store, codec, hash);
    let err = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], TraceId::new(), config)
        .await
        .unwrap_err();
    match err {
        LlmError::Provider { provider, .. } => {
            assert_eq!(provider, "react_graph");
        }
        other => panic!("expected Provider（存储故障语义）, got {other:?}"),
    }
    assert_eq!(
        provider.call_count(),
        0,
        "存储读取失败后 LLM 未执行（未当作「没有检查点」继续）"
    );
}

// ─── 完成态重建：旧格式检查点（无 last_response，§5.4）──────────

/// 完成态重建：恢复**完成态**检查点（next_node=None）且无 last_response（旧格式）时，
/// 最终 response 必须从 messages 的最后一条 Assistant 消息重建 — 最终答案不丢失。
///
/// 四组必测场景均以真实 LLM 调用收尾（last_response=Some），未覆盖此分支；
/// 本测试直接构造旧格式完成态检查点并经存储往返后恢复。
#[tokio::test]
async fn completed_checkpoint_reconstructs_response_from_messages() {
    // 零执行 — provider 绝不应被调用（空脚本，误调即报错）
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();

    // 旧格式完成态检查点：next_node=None、last_response=None，
    // messages 最后一条为 Assistant 文本消息
    let mut state = AgentState::from_messages(vec![
        Message::user_text("q"),
        Message::assistant_text("final answer text"),
    ]);
    state.iterations = 1;
    state.stop_reason = Some(StopReason::Complete);
    let cp = Checkpoint::new(None, &state, hash, 3);

    // 存储往返 — 证明 last_response=None（旧格式）可序列化/反序列化
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();
    let blob = codec.serialize(&cp, hash).unwrap();
    store.save_with_trace(&trace_id, &blob).await.unwrap();
    let loaded = store.load_latest(&trace_id).await.unwrap().unwrap();
    let cp_loaded = codec.deserialize(&loaded, hash).unwrap();
    assert!(
        cp_loaded.state.last_response.is_none(),
        "旧格式检查点无 last_response"
    );

    let config = CheckpointConfig::for_store(store, codec, hash);
    let result = agent
        .invoke_with_restore(cp_loaded, trace_id, config)
        .await
        .unwrap();

    // 完成态零执行：provider 从未被调用
    assert_eq!(provider.call_count(), 0, "完成态恢复零执行");
    assert_eq!(result.stop_reason, StopReason::Complete);
    assert_eq!(result.iterations, 1);
    // §5.4 契约：最终 response 从最后一条 Assistant 消息重建
    let text = ContentBlock::flatten_text(&result.response.content);
    assert_eq!(text, "final answer text", "最终答案未丢失");
}
