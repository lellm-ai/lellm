//! 计算器 Graph — LangGraph Tutorial 的 LeLLM 对照实现
//!
//! 对照 LangGraph 官方教程：
//!   https://langchain-ai.github.io/langgraph/tutorials/quickstart/
//!
//! LangGraph 用 3 个节点手动构建 Agent Loop：
//!   llm_node → tool_node → condition → (llm_node | END)
//!
//! LeLLM 的设计哲学不同：
//! - `ToolUseLoop` 内部完成 LLM ↔ Tools 的 ReAct 循环
//! - `AgentNode` 包装 ToolUseLoop，作为 Graph 的一个节点
//! - Graph 层负责宏观编排（预处理 → Agent → 后处理）
//!
//! 运行：
//! ```text
//! cargo run -p lellm-graph --example calculator_graph
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{AgentBuilder, ResolvedModel, ToolUseLoop};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TokenUsage, ToolCall,
};
use lellm_graph::{GraphBuilder, GraphExecutor, NodeKind, TaskNode};
use lellm_macros::Tool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ─── 1. 定义工具（对应 LangGraph Step 1）─────────────────────────

/// 加法工具
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "add", description = "Add two numbers")]
struct AddArgs {
    /// First number
    a: f64,
    /// Second number
    b: f64,
}

/// 乘法工具
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "multiply", description = "Multiply two numbers")]
struct MultiplyArgs {
    /// First number
    a: f64,
    /// Second number
    b: f64,
}

/// 除法工具
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "divide", description = "Divide two numbers")]
struct DivideArgs {
    /// First number
    a: f64,
    /// Second number
    b: f64,
}

// ─── 2. 模拟 Provider（模拟 LLM 的 ReAct 循环）──────────────────

/// 模拟多轮 ReAct 循环的 Provider。
///
/// 第 1 轮 → 返回 tool_call(add(3, 4))
/// 第 2 轮 → 返回 tool_call(multiply(7, 2))
/// 第 3 轮 → 返回最终答案
struct CalculatorMockProvider {
    round_responses: Vec<ChatResponse>,
    current_round: Mutex<usize>,
}

impl CalculatorMockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            round_responses: responses,
            current_round: Mutex::new(0),
        }
    }
}

#[::async_trait::async_trait]
impl lellm_provider::LlmProvider for CalculatorMockProvider {
    async fn call(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let round = {
            let mut r = self.current_round.lock().unwrap();
            let current = *r;
            *r += 1;
            current
        };

        Ok(self.round_responses.get(round).cloned().unwrap_or_else(|| {
            ChatResponse::new(
                vec![ContentBlock::text("计算完成。".to_string())],
                TokenUsage::default(),
                serde_json::json!(null),
            )
        }))
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
    ) -> Result<lellm_provider::ProviderStream, LlmError> {
        unimplemented!("stream not needed for this example")
    }

    fn provider_id(&self) -> &str {
        "calculator-mock"
    }
}

/// 构建模拟计算器的 Agent —— 对应 LangGraph Step 1 + 3 + 4 + 5
fn create_calculator_agent() -> ToolUseLoop {
    // 第 1 轮：LLM 决定调用 add(3, 4)
    let add_call = ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_add_001".to_string(),
            name: "add".to_string(),
            arguments: serde_json::json!({ "a": 3.0, "b": 4.0 }),
        })],
        TokenUsage::default(),
        serde_json::json!(null),
    );

    // 第 2 轮：LLM 决定调用 multiply(7, 2)
    let multiply_call = ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_mul_002".to_string(),
            name: "multiply".to_string(),
            arguments: serde_json::json!({ "a": 7.0, "b": 2.0 }),
        })],
        TokenUsage::default(),
        serde_json::json!(null),
    );

    // 第 3 轮：LLM 返回最终答案
    let final_answer = ChatResponse::new(
        vec![ContentBlock::text(
            "3 + 4 = 7，然后 7 × 2 = 14。最终答案是 14。".to_string(),
        )],
        TokenUsage {
            prompt_tokens: 300,
            completion_tokens: 40,
            total_tokens: 340,
        },
        serde_json::json!(null),
    );

    let provider = Arc::new(CalculatorMockProvider::new(vec![
        add_call,
        multiply_call,
        final_answer,
    ]));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "claude-sonnet-4-5".to_string(),
    };

    // 注册工具 —— 对应 LangGraph Step 1 的 tools = [add, multiply, divide]
    let tools = vec![
        AddArgs::safe(|args| async move {
            let result = args.a + args.b;
            Ok(serde_json::json!(result))
        }),
        MultiplyArgs::safe(|args| async move {
            let result = args.a * args.b;
            Ok(serde_json::json!(result))
        }),
        DivideArgs::safe(|args| async move {
            if args.b == 0.0 {
                Err(lellm_agent::ToolError::invalid_input("Division by zero"))
            } else {
                let result = args.a / args.b;
                Ok(serde_json::json!(result))
            }
        }),
    ];

    AgentBuilder::new(model)
        .system_prompt("你是一个数学助手，负责对数字执行算术运算。".to_string())
        .tools(tools)
        .max_iterations(10)
        .build()
}

// ─── 3. 构建 Graph（对应 LangGraph Step 6）───────────────────────

#[tokio::main]
async fn main() {
    // 创建 Agent（内部包含完整的 ToolUseLoop ReAct 循环）
    let agent = create_calculator_agent();

    // 构建 Graph —— 对应 LangGraph:
    //   StateGraph.add_node("llmCall", llmCall)
    //     .addNode("toolNode", toolNode)
    //     .addEdge(START, "llmCall")
    //     .addConditionalEdges("llmCall", shouldContinue, ["toolNode", END])
    //     .addEdge("toolNode", "llmCall")
    //     .compile()
    //
    // LeLLM 中，AgentNode 内部就是完整的 ToolUseLoop，
    // 所以 Graph 只需要一个 Agent 节点 + 预处理/后处理节点。
    let mut g = GraphBuilder::new("calculator");
    // 预处理：初始化状态
    let _ = g.start("init");
    let _ = g.node(
        "init",
        NodeKind::Task(TaskNode::new("init", |state| {
            state.insert(
                "calc.messages".into(),
                serde_json::json!(vec![Message::User {
                    content: lellm_core::text_block("3加4等于多少，然后再乘以2。".to_string(),),
                }]),
            );
            Ok(())
        })),
    );
    // Agent 节点：执行完整的 ReAct 循环
    // P0: 显式声明写入 — 默认不写任何 State，用户显式绑定
    let _ = g.node(
        "agent",
        NodeKind::Agent(Box::new(
            lellm_graph::AgentNode::new("agent", agent)
                .with_output("calc.output")
                .with_messages("calc.messages")
                .with_input_key("calc.messages"),
        )),
    );
    // 后处理：读取 AgentNode 显式写回的状态
    let _ = g.node(
        "summary",
        NodeKind::Task(TaskNode::new("summary", |state| {
            println!("\n=== Graph 执行结果 ===");

            // 读取最终输出
            let output = state
                .get("calc.output")
                .and_then(|v| v.as_str())
                .unwrap_or("(no output)");
            println!("最终输出: {}", output);

            // 读取完整对话历史
            if let Some(msgs) = state.get("calc.messages") {
                let count = if let Some(arr) = msgs.as_array() {
                    arr.len()
                } else {
                    0
                };
                println!("对话消息数: {}", count);
            }

            // 执行元数据（iterations, tool_calls, stop_reason）不写入 State
            // 如需获取，应通过 ExecutionTrace 或流式事件
            Ok(())
        })),
    );
    // 连接边
    let _ = g.edge("init", "agent");
    let _ = g.edge("agent", "summary");
    let _ = g.end("summary");
    let graph = g.build().expect("Graph 构建失败");

    // 执行 Graph —— 对应 LangGraph: agent.invoke({messages: [...]})
    println!("=== LeLLM Calculator Graph ===\n");
    println!("Graph 节点: {:?}", graph.node_names());
    println!("起始节点: {}", graph.start_node());
    println!();

    let result = GraphExecutor::default()
        .execute(std::sync::Arc::new(graph), HashMap::new())
        .await
        .expect("Graph 执行失败");

    // 打印执行日志
    println!("\n=== 执行日志 ===");
    for (i, entry) in result.execution_log.iter().enumerate() {
        let status = if entry.success { "✅" } else { "❌" };
        println!(
            "  [{}] {} {} {}ms",
            i + 1,
            entry.node_name,
            status,
            entry.elapsed().as_millis(),
        );
    }
    println!("总耗时: {}ms", result.duration.as_millis());

    // 打印最终状态
    println!("\n=== 最终状态 ===");
    for (key, value) in &result.state {
        println!("  {}: {}", key, value);
    }
}
