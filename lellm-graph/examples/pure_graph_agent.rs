//! 纯 Graph Agent — 不引入 lellm-agent，仅用 lellm-graph + lellm-core + lellm-provider
//!
//! 演示如何用 Graph 原语（TaskNode, ConditionNode, GraphBuilder）
//! 手动构建一个 ReAct Agent Loop。
//!
//! # 图结构
//!
//! ```text
//!    ┌─────────────┐     超出轮次    ┌──────────┐
//!    │  budget_chk  │───────────────│   done    │
//!    └──────┬──────┘               └──────────┘
//!           ▼
//!    ┌─────────────┐
//!    │  llm_call   │
//!    └──────┬──────┘
//!           ▼
//!    ┌────────────────┐  有 tool_call  ┌──────────────┐
//!    │ post_llm_route │───────────────│ tool_execute  │
//!    └──────┬─────────┘               └───────┬──────┘
//!           │ 无 tool_call                    ▼
//!           │                         (loop back)
//!           ▼
//!        (end)
//! ```
//!
//! # 运行
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run -p lellm-graph --example pure_graph_agent
//! # 或 Ollama:
//! OPENAI_API_BASE=http://localhost:11434/v1 OPENAI_API_KEY=ollama \
//!   cargo run -p lellm-graph --example pure_graph_agent
//! ```

use async_trait::async_trait;
use lellm_core::{ChatRequest, ContentBlock, ExecutableTool, Message, ToolCall, ToolDefinition};
use lellm_derive::tool;
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

// ─── 工具定义（#[tool] 宏自动生成 Schema + ExecutableTool）─────

/// 查询指定城市的天气情况
#[tool(name = "query_weather", description = "查询指定城市的天气情况")]
async fn query_weather(location: String) -> lellm_core::ToolResult {
    let result = match location.to_lowercase().as_str() {
        "北京" | "beijing" => "北京当前天气: 晴, 28°C, 湿度 45%".to_string(),
        "上海" | "shanghai" => "上海当前天气: 多云, 32°C, 湿度 70%".to_string(),
        "深圳" | "shenzhen" => "深圳当前天气: 雷阵雨, 30°C, 湿度 85%".to_string(),
        _ => format!("{}当前天气: 晴, 25°C, 湿度 50%（模拟数据）", location),
    };
    Ok(Value::String(result))
}

/// 数学计算器，支持加减乘除
#[tool(name = "calculator", description = "数学计算器，支持加减乘除")]
async fn calculator(expression: String) -> lellm_core::ToolResult {
    let result = match eval_expr(expression.trim()) {
        Ok(r) => format!("{} = {}", expression, r),
        Err(e) => format!("计算错误: {}", e),
    };
    Ok(Value::String(result))
}

fn eval_expr(expr: &str) -> Result<String, String> {
    let op_pos = expr
        .char_indices()
        .find_map(|(i, c)| if "+-*/".contains(c) { Some(i) } else { None });
    let Some(pos) = op_pos else {
        return Err("无效表达式".into());
    };
    let op = expr.chars().nth(pos).unwrap();
    let left: f64 = expr[..pos].trim().parse().map_err(|_| "无效数字")?;
    let right: f64 = expr[pos + 1..].trim().parse().map_err(|_| "无效数字")?;
    let result = match op {
        '+' => left + right,
        '-' => left - right,
        '*' | 'x' | '×' => left * right,
        '/' => {
            if right == 0.0 {
                return Err("除零错误".into());
            }
            left / right
        }
        _ => return Err(format!("不支持的操作符: {}", op)),
    };
    if result.fract() == 0.0 {
        Ok(format!("{}", result as i64))
    } else {
        Ok(format!("{:.2}", result))
    }
}

fn get_tools() -> Vec<ExecutableTool> {
    vec![query_weather_tool(), calculator_tool()]
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
            let result: lellm_core::ToolResult = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(tool.execute(&tc.arguments))
            });
            let result_str = match &result {
                Ok(v) => v.to_string(),
                Err(e) => e.to_string(),
            };
            let tool_result_msg = Message::tool_result(tc, &result);

            tracing::info!(tool = %tc.name, result = %result_str, "tool_executed");
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

fn build_agent_graph(
    model: ResolvedModel,
    max_iterations: usize,
) -> Result<lellm_graph::Graph<State, StateMerge>, lellm_graph::BuildErrors> {
    let tools = Arc::new(get_tools());
    let mut builder = GraphBuilder::<State, StateMerge>::new("pure_graph_agent");

    builder.start("budget_chk");

    builder.node(
        "budget_chk",
        NodeKind::Task(create_budget_check(max_iterations)),
    );
    builder.node(
        "llm_call",
        NodeKind::External(Arc::new(LlmCallNode::new(
            model,
            "你是一个智能助手。你可以使用 query_weather 查询天气，使用 calculator 进行数学计算。\n\
             当用户提问时，优先判断是否需要使用工具。如果需要，调用工具获取结果后再回答。",
        ))),
    );
    builder.node("post_llm_route", NodeKind::Task(create_post_llm_route()));
    builder.node("tool_execute", NodeKind::Task(create_tool_execute(tools)));
    builder.node(
        "done",
        NodeKind::Task(TaskNode::new(
            "done",
            |_ctx: &mut NodeContext<'_, State>| {
                tracing::info!("agent done");
                Ok(())
            },
        )),
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
            "step_completed"
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

    println!("=== 纯 Graph Agent (无 lellm-agent) ===\n");

    let provider =
        CodecProvider::load(lellm_provider::providers::openai_compat::OpenAICompatCodec::openai())
            .expect("请设置 OPENAI_API_KEY 环境变量");

    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "llama3.2".into(),
        context_window: Some(8192),
    };

    let graph = build_agent_graph(model, 10).expect("Graph 构建失败");
    println!("Graph 构建完成: {}", graph.name());
    println!("节点: {:?}", graph.node_names());
    println!();

    let user_question = "北京天气怎么样？3加4乘以2等于多少？";
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
            println!("\n=== 执行摘要 ===");
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
