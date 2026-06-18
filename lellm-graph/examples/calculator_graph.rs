//! Calculator Graph — LeLLM 入门示例
//!
//! 展示 Graph + Agent 的基本用法：
//! - 定义工具
//! - 构建 Agent（内含 ReAct 循环）
//! - 构建 Graph（预处理 → Agent → 后处理）
//!
//! 运行：`cargo run -p lellm-graph --example calculator_graph`

use futures_util::stream;
use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, AgentFlowNode, ResolvedModel, ToolUseLoop};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TokenUsage, ToolCall,
};
use lellm_derive::Tool;
use lellm_graph::{GraphBuilder, GraphExecutor, NodeKind, StateDelta, TaskNode};
use lellm_provider::{ProviderEvent, ProviderStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ─── 1. 定义工具 ─────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "add", description = "Add two numbers")]
struct AddArgs {
    a: f64,
    b: f64,
}

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "multiply", description = "Multiply two numbers")]
struct MultiplyArgs {
    a: f64,
    b: f64,
}

// ─── 2. 模拟 Provider ────────────────────────────────────────

/// 模拟多轮 ReAct 循环的 Provider。
///
/// 第 1 轮 → tool_call(add(3, 4))
/// 第 2 轮 → tool_call(multiply(7, 2))
/// 第 3 轮 → 最终答案
struct MockProvider {
    responses: Vec<ChatResponse>,
    round: Mutex<usize>,
}

impl MockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            round: Mutex::new(0),
        }
    }

    fn next_response(&self) -> ChatResponse {
        let mut r = self.round.lock().unwrap();
        let idx = *r;
        *r += 1;
        self.responses
            .get(idx)
            .cloned()
            .unwrap_or_else(|| {
                ChatResponse::new(
                    vec![ContentBlock::text("Done.".into())],
                    TokenUsage::default(),
                    serde_json::json!(null),
                )
            })
    }
}

#[::async_trait::async_trait]
impl lellm_provider::LlmProvider for MockProvider {
    async fn call(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(self.next_response())
    }

    async fn stream(&self, _req: &ChatRequest) -> Result<ProviderStream, LlmError> {
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

// ─── 3. 构建 Agent ───────────────────────────────────────────

fn build_agent() -> ToolUseLoop {
    // 模拟 3 轮 ReAct 循环
    let responses = vec![
        // 第 1 轮：调用 add(3, 4)
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: serde_json::json!({"a": 3.0, "b": 4.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        // 第 2 轮：调用 multiply(7, 2)
        ChatResponse::new(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c2".into(),
                name: "multiply".into(),
                arguments: serde_json::json!({"a": 7.0, "b": 2.0}),
            })],
            TokenUsage::default(),
            serde_json::json!(null),
        ),
        // 第 3 轮：返回最终答案
        ChatResponse::new(
            vec![ContentBlock::text("3 + 4 = 7，7 × 2 = 14。答案是 14。".into())],
            TokenUsage {
                prompt_tokens: 300,
                completion_tokens: 40,
                total_tokens: 340,
            },
            serde_json::json!(null),
        ),
    ];

    let model = ResolvedModel {
        provider: Arc::new(MockProvider::new(responses)),
        model: "mock".into(),
        context_window: None,
    };

    AgentBuilder::new(model)
        .system_prompt("你是一个数学助手。".into())
        .tools([
            AddArgs::safe(|args| async move { Ok(serde_json::json!(args.a + args.b)) }),
            MultiplyArgs::safe(|args| async move { Ok(serde_json::json!(args.a * args.b)) }),
        ])
        .max_iterations(10)
        .build()
}

// ─── 4. 构建 Graph ───────────────────────────────────────────

fn build_graph(agent: ToolUseLoop) -> lellm_graph::Graph {
    let mut g = GraphBuilder::new("calculator");

    g.start("init");

    // 初始化：写入用户问题
    g.node(
        "init",
        NodeKind::Task(TaskNode::new("init", |_| {
            Ok(vec![StateDelta::put(
                "messages",
                serde_json::json!([Message::User {
                    content: lellm_core::text_block("3加4等于多少，然后再乘以2。".into()),
                }]),
            )])
        })),
    );

    // Agent 节点：执行 ReAct 循环
    g.node(
        "agent",
        NodeKind::External(Arc::new(AgentFlowNode::new("agent", agent))),
    );

    // 后处理：打印结果
    g.node(
        "summary",
        NodeKind::Task(TaskNode::new("summary", |state| {
            println!("\n=== 结果 ===");

            if let Some(msgs) = state.get("messages") {
                let count = msgs.as_array().map_or(0, |a| a.len());
                println!("消息数: {count}");
            }

            let reason = state
                .get("agent_stop_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let iters = state
                .get("agent_iterations")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let calls = state
                .get("agent_tool_calls")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!("停止原因: {reason}");
            println!("迭代次数: {iters}");
            println!("工具调用: {calls}");

            Ok(vec![])
        })),
    );

    g.edge("init", "agent");
    g.edge("agent", "summary");
    g.end("summary");

    g.build().expect("Graph 构建失败")
}

// ─── 5. 执行 ─────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let agent = build_agent();
    let graph = build_graph(agent);

    println!("=== LeLLM Calculator Graph ===\n");
    println!("节点: {:?}", graph.node_names());
    println!("起始: {}", graph.start_node());

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("执行失败");

    println!("\n=== 执行日志 ===");
    for (i, e) in result.execution_log.iter().enumerate() {
        let icon = if e.success { "✅" } else { "❌" };
        println!("  [{}] {} {icon} {}ms", i + 1, e.node_name, e.elapsed().as_millis());
    }
    println!("总耗时: {}ms", result.duration.as_millis());

    println!("\n=== 最终状态 ===");
    for (k, v) in &result.state {
        println!("  {k}: {v}");
    }
}
