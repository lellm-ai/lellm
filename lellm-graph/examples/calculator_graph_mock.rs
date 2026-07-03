//! Calculator Graph (Mock) — LangGraph Tutorial 的 LeLLM 对照实现
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! 仅使用 lellm-graph + lellm-core + lellm-provider，不引入 lellm-agent。
//! 使用 MockProvider 模拟 LLM 响应，无需 API Key。
//!
//! ```text
//! cargo run -p lellm-graph --example calculator_graph_mock
//! ```

use async_trait::async_trait;
use futures_util::stream;
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, ExecutableTool, LlmError, Message, TokenUsage,
    ToolCall, ToolDefinition,
};
use lellm_derive::tool;
use lellm_graph::{
    GraphBuilder, GraphError, NodeContext, NodeKind, State, StateMerge, StateMutation, TaskNode,
};
use lellm_provider::{ProviderEvent, ProviderStream, ResolvedModel};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// ─── State Key 常量 ───────────────────────────────────────────

const KEY_MESSAGES: &str = "messages";
const KEY_ITERATIONS: &str = "iterations";
const KEY_TOOL_CALLS: &str = "tool_calls";

// ─── 工具定义（#[tool] 宏自动生成 Schema + ExecutableTool）─────

#[tool(name = "add", description = "Add two numbers")]
async fn add(a: f64, b: f64) -> lellm_core::ToolResult {
    Ok(Value::from(a + b))
}

#[tool(name = "multiply", description = "Multiply two numbers")]
async fn multiply(a: f64, b: f64) -> lellm_core::ToolResult {
    Ok(Value::from(a * b))
}

fn get_tools() -> Vec<ExecutableTool> {
    vec![add_tool(), multiply_tool()]
}

fn get_tool_defs() -> Vec<ToolDefinition> {
    get_tools()
        .into_iter()
        .map(|t| t.definition.clone())
        .collect()
}

/// 根据工具名称查找对应的 ExecutableTool
fn find_tool<'a>(name: &str, tools: &'a [ExecutableTool]) -> Option<&'a ExecutableTool> {
    tools.iter().find(|t| t.definition.name == name)
}

// ─── Mock Provider ──────────────────────────────────────────────

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
        self.responses.get(idx).cloned().unwrap_or_else(|| {
            ChatResponse::new(
                vec![ContentBlock::text("Done.")],
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
        if iterations >= max_iterations {
            ctx.goto("done");
        }
        Ok(())
    })
}

struct LlmCallNode {
    model: ResolvedModel,
}

impl LlmCallNode {
    fn new(model: ResolvedModel) -> Self {
        Self { model }
    }

    async fn run(&self, ctx: &mut NodeContext<'_, State>) -> Result<(), GraphError> {
        let messages = get_messages(ctx.state());

        let request = ChatRequest {
            model: self.model.model.clone(),
            messages: messages.clone(),
            tools: Some(get_tool_defs()),
            ..Default::default()
        };

        let response = self.model.provider.call(&request).await.map_err(|e| {
            GraphError::Terminal(lellm_graph::TerminalError::NodeExecutionFailed {
                node: "llm_call".to_string(),
                source: Box::new(e),
            })
        })?;

        let content = response.content.clone();
        let tool_calls: Vec<ToolCall> = response.tool_calls().cloned().collect();

        let assistant_msg = Message::Assistant { content };
        let mut new_messages = messages;
        new_messages.push(assistant_msg);

        let iterations = get_iterations(ctx.state());
        ctx.record(StateMutation::Put(
            KEY_MESSAGES.into(),
            messages_to_json(&new_messages),
        ));
        ctx.record(StateMutation::Put(
            KEY_ITERATIONS.into(),
            Value::from(iterations + 1),
        ));

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
            ctx.goto("tool_execute");
        } else {
            ctx.end();
        }
        Ok(())
    })
}

fn create_tool_execute(tools: Arc<Vec<ExecutableTool>>) -> TaskNode<State> {
    TaskNode::new("tool_execute", move |ctx: &mut NodeContext<'_, State>| {
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
            let tool =
                find_tool(&tc.name, &tools).unwrap_or_else(|| panic!("未知工具: {}", tc.name));

            // 直接调用 ExecutableTool::execute — 自动反序列化参数
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(tool.execute(&tc.arguments))
            });
            let tool_result_msg = Message::tool_result(tc, &result);
            msgs.push(tool_result_msg);
        }

        ctx.record(StateMutation::Put(
            KEY_MESSAGES.into(),
            messages_to_json(&msgs),
        ));
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
    let tools = Arc::new(get_tools());
    let mut builder = GraphBuilder::<State, StateMerge>::new("calculator_graph_mock");

    builder.start("budget_chk");

    builder.node(
        "budget_chk",
        NodeKind::Task(create_budget_check(max_iterations)),
    );
    builder.node(
        "llm_call",
        NodeKind::External(Arc::new(LlmCallNode::new(model))),
    );
    builder.node("post_llm_route", NodeKind::Task(create_post_llm_route()));
    builder.node("tool_execute", NodeKind::Task(create_tool_execute(tools)));
    builder.node(
        "done",
        NodeKind::Task(TaskNode::new(
            "done",
            |_ctx: &mut NodeContext<'_, State>| Ok(()),
        )),
    );

    builder.edge("budget_chk", "llm_call");
    builder.edge("llm_call", "post_llm_route");
    builder.edge("tool_execute", "budget_chk");
    builder.end("done");

    builder.compile()
}

// ─── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
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
            vec![ContentBlock::text("3 + 4 = 7，7 × 2 = 14。答案是 14。")],
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

    let graph = build_graph(model, 10).expect("Graph 构建失败");
    println!("=== Calculator Graph (Mock) ===");
    println!("Graph: {} | 节点: {:?}\n", graph.name(), graph.node_names());

    let user_question = "3加4等于多少，然后再乘以2。";
    let mut state = State::new();
    state.insert(
        KEY_MESSAGES.into(),
        messages_to_json(&[Message::user_text(user_question)]),
    );
    state.insert(KEY_ITERATIONS.into(), Value::from(0));

    println!("用户: {}\n", user_question);

    let start = std::time::Instant::now();

    let mut exec_ctx =
        lellm_graph::ExecutionEngine::new(&mut state, None, CancellationToken::new(), None, None);

    struct NoopStepCallback;
    impl<'e> lellm_graph::StepCallback<'e> for NoopStepCallback {
        fn on_step(&mut self, _: &str, _: usize, _: std::time::Duration) {}
    }

    match graph
        .run_inline(&mut exec_ctx, 50, &mut NoopStepCallback)
        .await
    {
        Ok(()) => {
            println!("\n=== 执行完成 ({}ms) ===", start.elapsed().as_millis());
            println!("迭代次数: {}", get_iterations(&state));
            println!("\n=== 对话历史 ===");
            for msg in get_messages(&state) {
                print_message(&msg);
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
