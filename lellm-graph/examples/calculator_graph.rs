//! Calculator Graph — LangGraph Tutorial 的 LeLLM 对照实现（真实 Provider）
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! 仅使用 lellm-graph + lellm-core + lellm-provider，不引入 lellm-agent。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run -p lellm-graph --example calculator_graph
//! # 或 Ollama:
//! OPENAI_API_BASE=http://localhost:11434/v1 OPENAI_API_KEY=ollama \
//!   cargo run -p lellm-graph --example calculator_graph
//! ```

use async_trait::async_trait;
use lellm_core::{ChatRequest, ContentBlock, Message, ToolCall, ToolDefinition};
use lellm_graph::{
    GraphBuilder, GraphError, NodeContext, NodeKind, State, StateMerge, StateMutation, TaskNode,
};
use lellm_provider::{CodecProvider, ResolvedModel};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ─── State Key 常量 ───────────────────────────────────────────

const KEY_MESSAGES: &str = "messages";
const KEY_ITERATIONS: &str = "iterations";
const KEY_TOOL_CALLS: &str = "tool_calls";
const KEY_TEXT: &str = "text";

// ─── 工具定义 ──────────────────────────────────────────────────

#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct AddArgs {
    a: f64,
    b: f64,
}

#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct MultiplyArgs {
    a: f64,
    b: f64,
}

fn execute_tool(tc: &ToolCall) -> Value {
    match tc.name.as_str() {
        "add" => {
            let a = tc.arguments.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let b = tc.arguments.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Value::from(a + b)
        }
        "multiply" => {
            let a = tc.arguments.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let b = tc.arguments.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Value::from(a * b)
        }
        _ => Value::String(format!("未知工具: {}", tc.name)),
    }
}

fn get_tool_defs() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "add".to_string(),
            description: "Add two numbers".to_string(),
            parameters: ToolDefinition::compute_and_clean_schema::<AddArgs>(),
            cache_control: None,
        },
        ToolDefinition {
            name: "multiply".to_string(),
            description: "Multiply two numbers".to_string(),
            parameters: ToolDefinition::compute_and_clean_schema::<MultiplyArgs>(),
            cache_control: None,
        },
    ]
}

// ─── 辅助函数 ──────────────────────────────────────────────────

fn get_messages(state: &State) -> Vec<Message> {
    state
        .get(KEY_MESSAGES)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn messages_to_json(msgs: &[Message]) -> Value {
    Value::Array(
        msgs.iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect(),
    )
}

fn get_iterations(state: &State) -> usize {
    state
        .get(KEY_ITERATIONS)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(0)
}

// ─── Graph 节点 ─────────────────────────────────────────────────

fn create_budget_check(max_iterations: usize) -> TaskNode<State> {
    TaskNode::new("budget_chk", move |ctx: &mut NodeContext<'_, State>| {
        let iterations = get_iterations(ctx.state());
        tracing::info!(iteration = iterations, max = max_iterations, "budget_chk");

        if iterations >= max_iterations {
            tracing::warn!("超出最大迭代次数");
            ctx.goto("done");
        }
        Ok(())
    })
}

struct LlmCallNode {
    model: ResolvedModel,
    system: String,
}

impl LlmCallNode {
    fn new(model: ResolvedModel, system: impl Into<String>) -> Self {
        Self {
            model,
            system: system.into(),
        }
    }

    async fn run(&self, ctx: &mut NodeContext<'_, State>) -> Result<(), GraphError> {
        let messages = get_messages(ctx.state());

        let request = ChatRequest {
            model: self.model.model.clone(),
            messages: messages.clone(),
            tools: Some(get_tool_defs()),
            ..Default::default()
        }
        .with_system_prompt(self.system.clone());

        tracing::info!(model = %self.model.model, msg_count = messages.len(), "llm_call");

        let response = self.model.provider.call(&request).await.map_err(|e| {
            GraphError::Terminal(lellm_graph::TerminalError::NodeExecutionFailed {
                node: "llm_call".to_string(),
                source: Box::new(e),
            })
        })?;

        let content = response.content.clone();
        let tool_calls: Vec<ToolCall> = response.tool_calls().cloned().collect();
        let text: Option<String> = content
            .iter()
            .filter_map(|b: &ContentBlock| b.as_text().map(|s| s.to_string()))
            .next();

        tracing::info!(
            has_tool_calls = !tool_calls.is_empty(),
            has_text = text.is_some(),
            "llm_response"
        );

        let assistant_msg = Message::Assistant { content };
        let mut new_messages = messages;
        new_messages.push(assistant_msg);

        let iterations = get_iterations(ctx.state());
        ctx.record(StateMutation::Put(
            KEY_MESSAGES.into(),
            messages_to_json(&new_messages),
        ));
        ctx.record(StateMutation::Put(KEY_ITERATIONS.into(), Value::from(iterations + 1)));

        if !tool_calls.is_empty() {
            ctx.record(StateMutation::Put(
                KEY_TOOL_CALLS.into(),
                Value::Array(
                    tool_calls
                        .iter()
                        .filter_map(|tc| serde_json::to_value(tc).ok())
                        .collect(),
                ),
            ));
        }

        if let Some(ref t) = text {
            ctx.record(StateMutation::Put(KEY_TEXT.into(), serde_json::json!(t)));
        }

        Ok(())
    }
}

#[async_trait]
impl lellm_graph::FlowNode<State> for LlmCallNode {
    async fn execute(&self, ctx: &mut NodeContext<'_, State>) -> Result<(), GraphError> {
        self.run(ctx).await
    }
}

fn create_post_llm_route() -> TaskNode<State> {
    TaskNode::new("post_llm_route", |ctx: &mut NodeContext<'_, State>| {
        let has_tool_calls = ctx
            .state()
            .get(KEY_TOOL_CALLS)
            .and_then(|v| v.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);

        if has_tool_calls {
            tracing::info!("检测到 tool_call → tool_execute");
            ctx.goto("tool_execute");
        } else {
            tracing::info!("无 tool_call → end");
            ctx.end();
        }
        Ok(())
    })
}

fn create_tool_execute() -> TaskNode<State> {
    TaskNode::new("tool_execute", |ctx: &mut NodeContext<'_, State>| {
        let tool_calls: Vec<ToolCall> = ctx
            .state()
            .get(KEY_TOOL_CALLS)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        if tool_calls.is_empty() {
            return Ok(());
        }

        let mut msgs = get_messages(ctx.state());

        for tc in &tool_calls {
            let result = execute_tool(tc);
            let result_str = result.to_string();
            let tool_result: lellm_core::ToolResult = Ok(result);
            let tool_result_msg = Message::tool_result(tc, &tool_result);

            tracing::info!(tool = %tc.name, result = %result_str, "tool_executed");
            msgs.push(tool_result_msg);
        }

        ctx.record(StateMutation::Put(KEY_MESSAGES.into(), messages_to_json(&msgs)));
        ctx.record(StateMutation::Delete(KEY_TOOL_CALLS.into()));
        ctx.goto("budget_chk");
        Ok(())
    })
}

// ─── 构建 Graph ─────────────────────────────────────────────────

fn build_graph(
    model: ResolvedModel,
    max_iterations: usize,
) -> Result<lellm_graph::Graph<State, StateMerge>, lellm_graph::BuildErrors> {
    let mut builder = GraphBuilder::<State, StateMerge>::new("calculator_graph");

    builder.start("budget_chk");

    builder.node(
        "budget_chk",
        NodeKind::Task(create_budget_check(max_iterations)),
    );
    builder.node(
        "llm_call",
        NodeKind::External(Arc::new(LlmCallNode::new(
            model,
            "You are a math assistant. Always use the calculator tools to compute results. \
             The user speaks Chinese but you can respond in either language.",
        ))),
    );
    builder.node(
        "post_llm_route",
        NodeKind::Task(create_post_llm_route()),
    );
    builder.node("tool_execute", NodeKind::Task(create_tool_execute()));
    builder.node(
        "done",
        NodeKind::Task(TaskNode::new("done", |_ctx: &mut NodeContext<'_, State>| {
            tracing::info!("done");
            Ok(())
        })),
    );

    builder.edge("budget_chk", "llm_call");
    builder.edge("llm_call", "post_llm_route");
    builder.edge("tool_execute", "budget_chk");
    builder.end("done");

    builder.compile()
}

// ─── StepCallback ────────────────────────────────────────────────

struct LoggingStepCallback;

impl<'e> lellm_graph::StepCallback<'e> for LoggingStepCallback {
    fn on_step(&mut self, node_name: &str, step: usize, duration: std::time::Duration) {
        tracing::info!(
            step = step,
            node = node_name,
            duration_ms = duration.as_millis(),
            "step"
        );
    }
}

// ─── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_graph=trace,lellm_provider=trace,info".into()),
        )
        .try_init();

    println!("=== Calculator Graph (纯 graph, 无 lellm-agent) ===\n");

    let provider = CodecProvider::load(
        lellm_provider::providers::openai_compat::OpenAICompatCodec::openai(),
    )
    .expect("请设置 OPENAI_API_KEY 环境变量");

    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "llama3.2".into(),
        context_window: Some(8192),
    };

    let graph = build_graph(model, 10).expect("Graph 构建失败");
    println!("Graph: {} | 节点: {:?}", graph.name(), graph.node_names());
    println!();

    let user_question = "3加4等于多少，然后再乘以2。";
    let mut state = State::new();
    state.insert(
        KEY_MESSAGES.into(),
        messages_to_json(&[Message::user_text(user_question)]),
    );
    state.insert(KEY_ITERATIONS.into(), Value::from(0));

    println!("用户: {}\n", user_question);

    let start = std::time::Instant::now();

    let mut exec_ctx = lellm_graph::ExecutionEngine::new(
        &mut state,
        None,
        CancellationToken::new(),
        None,
        None,
    );

    match graph
        .run_inline(&mut exec_ctx, 50, &mut LoggingStepCallback)
        .await
    {
        Ok(()) => {
            println!("\n=== 执行完成 ({}ms) ===", start.elapsed().as_millis());
            println!("\n=== 对话历史 ===");
            for msg in get_messages(&state) {
                print_message(&msg);
            }
            println!("\n=== 摘要 ===");
            println!("迭代次数: {}", get_iterations(&state));
            if let Some(text) = state.get(KEY_TEXT).and_then(|v| v.as_str()) {
                println!("AI 回复: {}", text);
            }
        }
        Err(e) => {
            println!("\n执行失败: {:?}", e);
        }
    }
}

fn print_message(msg: &Message) {
    match msg {
        Message::User { content } => {
            print!("[用户] ");
            for block in content {
                if let ContentBlock::Text(t) = block {
                    print!("{}", t.text);
                }
            }
            println!();
        }
        Message::Assistant { content } => {
            print!("[AI] ");
            for block in content {
                match block {
                    ContentBlock::Text(t) => print!("{}", t.text),
                    ContentBlock::ToolCall(tc) => print!("[调用 {}({})]", tc.name, tc.arguments),
                    _ => {}
                }
            }
            println!();
        }
        Message::ToolResult {
            tool_call_id,
            content,
            is_error,
            ..
        } => {
            print!("[工具结果 {}]", tool_call_id);
            if *is_error {
                print!("(错误) ");
            }
            for block in content {
                if let ContentBlock::Text(t) = block {
                    print!("{}", t.text);
                }
            }
            println!();
        }
        _ => {}
    }
}
