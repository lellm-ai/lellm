//! Calculator Graph — LangGraph Tutorial 的 LeLLM 对照实现（真实 Provider）
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! LangGraph 用 3 个节点手动构建 Agent Loop：
//!   llm_node → tool_node → condition → (llm_node | END)
//!
//! LeLLM 的设计哲学不同：
//! - `ToolUseLoop` 内部完成 LLM ↔ Tools 的 ReAct 循环
//! - `AgentFlowNode` 包装 ToolUseLoop，作为 Graph 的一个节点
//! - Graph 层负责宏观编排（预处理 → Agent → 后处理）
//!
//! 使用 OpenAI 兼容的 LLaMA Provider：
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run -p lellm-graph --example calculator_graph
//! # 或 Ollama:
//! OPENAI_API_BASE=http://localhost:11434/v1 OPENAI_API_KEY=ollama cargo run -p lellm-graph --example calculator_graph
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, AgentFlowNode, ResolvedModel};
use lellm_core::Message;
use lellm_derive::Tool;
use lellm_graph::{
    GraphBuilder, GraphExecutor, NodeContext, NodeKind, State, StateMutation, TaskNode,
};
use lellm_provider::providers::base::CodecProvider;
use lellm_provider::providers::openai_compat::OpenAICompatCodec;
use std::sync::Arc;

// ─── 工具定义 ───────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "add", description = "将两个数字相加")]
struct AddArgs {
    /// 第一个数字
    a: f64,
    /// 第二个数字
    b: f64,
}

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "multiply", description = "将两个数字相乘")]
struct MultiplyArgs {
    /// 第一个数字
    a: f64,
    /// 第二个数字
    b: f64,
}

// ─── 构建 Graph ─────────────────────────────────────────────

fn build_graph() -> lellm_graph::Graph {
    // 创建 Provider（从环境变量读取 API Key）
    let provider =
        CodecProvider::load(OpenAICompatCodec::llama()).expect("请设置 OPENAI_API_KEY 环境变量");

    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "llama3.2".into(),
        context_window: Some(8192),
    };

    let agent = AgentBuilder::new(model)
        .system_prompt("你是一个数学助手。当用户问数学问题时，使用工具计算。".to_string())
        .tools([
            AddArgs::safe(|args| async move { Ok(serde_json::json!(args.a + args.b)) }),
            MultiplyArgs::safe(|args| async move { Ok(serde_json::json!(args.a * args.b)) }),
        ])
        .max_iterations(10)
        .build();

    let mut g = GraphBuilder::new("calculator");

    g.start("init");

    // 初始化：写入用户问题
    g.node(
        "init",
        NodeKind::Task(TaskNode::new("init", |ctx: &mut NodeContext<'_, State>| {
            ctx.record(StateMutation::Put(
                "messages".into(),
                serde_json::json!([Message::user_text("3加4等于多少，然后再乘以2。")]),
            ));
            Ok(())
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
        NodeKind::Task(TaskNode::new(
            "summary",
            |ctx: &mut NodeContext<'_, State>| {
                println!("\n=== 结果 ===");

                let state = ctx.state();
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

                Ok(())
            },
        )),
    );

    g.edge("init", "agent");
    g.edge("agent", "summary");
    g.end("summary");

    g.build().expect("Graph 构建失败")
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=trace,lellm_provider=trace,info".into()),
        )
        .try_init();

    let graph = build_graph();

    println!("=== Calculator Graph (LLaMA) ===\n");
    println!("节点: {:?}", graph.node_names());

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await
        .expect("执行失败");

    println!("\n=== 执行日志 ===");
    for (i, e) in result.execution_log.iter().enumerate() {
        let icon = if e.success { "✅" } else { "❌" };
        println!(
            "  [{}] {} {icon} {}ms",
            i + 1,
            e.node_name,
            e.elapsed().as_millis()
        );
    }
    println!("总耗时: {}ms", result.duration.as_millis());

    println!("\n=== 最终状态 ===");
    for (k, v) in result.state.iter() {
        println!("  {k}: {v}");
    }
}
