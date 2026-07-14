use lellm_agent::{
    AgentBuilder, ContextBudget, ContextCompactor, ExecutableTool, LocalCompactor, StaticCatalog,
    ToolCategory, ToolExecutor, estimate_message, estimate_tokens,
};
use lellm_core::ToolArgs;
use lellm_core::{
    ChatResponse, ContentBlock, Message, TokenUsage, ToolCall, ToolDefinition, ToolSchema,
};
use lellm_derive::Tool;
use lellm_provider::{MockProvider, ResolvedModel};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[tokio::test]
async fn test_tool_use_loop_no_tool_calls() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hello")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    let messages = vec![Message::user_text("test")];

    let result = AgentBuilder::new(model)
        .max_iterations(5)
        .compile()
        .invoke(messages)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(!result.response.has_tool_calls());
}

#[tokio::test]
async fn test_tool_executor_snapshot_and_execute() {
    let def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo tool".to_string(),
        parameters: ToolSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            }
        })),
        cache_control: None,
    };
    let reg = ExecutableTool::safe(def, |args: &serde_json::Value| {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(serde_json::json!(format!("echo: {}", text))) }
    });

    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::new(Arc::new(catalog));

    let call = ToolCall {
        id: "1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::json!({"text": "hello"}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;
    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("echo: hello"));
}

#[test]
fn test_tool_category() {
    assert_eq!(ToolCategory::FILE_IO.0, "file_io");
    assert_eq!(ToolCategory::NETWORK.0, "network");
    assert_eq!(ToolCategory::DATABASE.0, "database");
}

// ─── ToolArgs trait + derive(Tool) 测试 ───

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "weather_search", description = "搜索天气信息")]
#[allow(dead_code)]
struct WeatherArgs {
    /// 城市名称
    city: String,
    /// 单位（摄氏度/华氏度）
    unit: Option<String>,
    /// 是否包含预报
    include_forecast: bool,
}

#[test]
fn test_tool_args_trait() {
    assert_eq!(WeatherArgs::NAME, "weather_search");
    assert_eq!(WeatherArgs::DESCRIPTION, "搜索天气信息");
}

#[test]
fn test_tool_args_backward_compat() {
    assert_eq!(WeatherArgs::__name(), "weather_search");
    assert_eq!(WeatherArgs::__description(), "搜索天气信息");
}

#[test]
fn test_tool_definition_generation() {
    let def = WeatherArgs::tool_definition();

    assert_eq!(def.name, "weather_search");
    assert_eq!(def.description, "搜索天气信息");

    // 检查 Schema 结构
    let schema = def.parameters.as_value();
    assert_eq!(schema.get("type").unwrap(), "object");

    let properties = schema.get("properties").unwrap().as_object().unwrap();
    assert_eq!(properties.len(), 3);

    // city — String → required + string
    assert_eq!(properties["city"]["type"], "string");
    assert_eq!(properties["city"]["description"], "城市名称");

    // unit — Option<String> → not required + ["string", "null"] (JSON Schema nullable)
    let unit_type = &properties["unit"]["type"];
    assert!(
        *unit_type == "string" || *unit_type == serde_json::json!(["string", "null"]),
        "expected string or [string, null], got {}",
        unit_type
    );
    assert_eq!(properties["unit"]["description"], "单位（摄氏度/华氏度）");

    // include_forecast — bool → required + boolean
    assert_eq!(properties["include_forecast"]["type"], "boolean");
    assert_eq!(
        properties["include_forecast"]["description"],
        "是否包含预报"
    );

    // 检查 required 字段
    let required = schema.get("required").unwrap().as_array().unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("city")));
    assert!(
        required
            .iter()
            .any(|v| v.as_str() == Some("include_forecast"))
    );
    // unit 是 Option，不应在 required 中
    assert!(!required.iter().any(|v| v.as_str() == Some("unit")));
}

#[test]
fn test_tool_args_schema_backward_compat() {
    let schema = WeatherArgs::schema();
    let def_schema = WeatherArgs::tool_definition().parameters;
    assert_eq!(schema, def_schema);
}

#[test]
fn test_tool_definition_default_name() {
    // 不指定 name，应自动转换为 snake_case
    #[derive(Deserialize, JsonSchema, Tool)]
    #[tool(description = "测试默认命名")]
    #[allow(dead_code)]
    struct MySearchTool {
        pub query: String,
    }

    assert_eq!(MySearchTool::NAME, "my_search_tool");
    assert_eq!(MySearchTool::__name(), "my_search_tool");
}

#[test]
fn test_option_type_inference() {
    // 验证 Option<T> 正确推导内部类型
    #[derive(Deserialize, JsonSchema, Tool)]
    #[tool(name = "typed_test", description = "测试类型推导")]
    #[allow(dead_code)]
    struct TypedTestArgs {
        /// 可选字符串
        opt_string: Option<String>,
        /// 可选整数
        opt_int: Option<i32>,
        /// 可选布尔
        opt_bool: Option<bool>,
        /// 可选浮点数
        opt_float: Option<f64>,
    }

    let def = TypedTestArgs::tool_definition();
    let properties = def
        .parameters
        .as_value()
        .get("properties")
        .unwrap()
        .as_object()
        .unwrap();

    // Option<T> → [T, "null"] in JSON Schema (schemars standard)
    fn expect_type(actual: &serde_json::Value, expected: &str) {
        assert!(
            *actual == expected || *actual == serde_json::json!([expected, "null"]),
            "expected {} or [{} , \"null\"], got {}",
            expected,
            expected,
            actual
        );
    }
    expect_type(&properties["opt_string"]["type"], "string");
    expect_type(&properties["opt_int"]["type"], "integer");
    expect_type(&properties["opt_bool"]["type"], "boolean");
    expect_type(&properties["opt_float"]["type"], "number");

    // 全部是 Option，required 应为空
    let required = def.parameters.as_value().get("required");
    assert!(required.map_or(true, |r| r.as_array().map_or(true, |a| a.is_empty())));
}

// ─── Level 2: safe() 便捷方法测试 ───

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "greet_tool", description = "打招呼")]
struct GreetArgs {
    /// 名字
    name: String,
}

#[test]
fn test_tool_safe_method() {
    // 验证 safe() 方法正常工作
    let reg =
        GreetArgs::safe(
            |args| async move { Ok(serde_json::json!(format!("你好, {}!", args.name))) },
        );

    assert_eq!(reg.definition().name, "greet_tool");
    assert_eq!(reg.definition().description, "打招呼");
}

#[tokio::test]
async fn test_tool_safe_execution() {
    let reg =
        GreetArgs::safe(
            |args| async move { Ok(serde_json::json!(format!("你好, {}!", args.name))) },
        );

    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::new(Arc::new(catalog));

    let call = ToolCall {
        id: "1".to_string(),
        name: "greet_tool".to_string(),
        arguments: serde_json::json!({"name": "世界"}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;
    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("你好, 世界!"));
}

// ─── AgentBuilder 测试 ───

#[tokio::test]
async fn test_builder_basic_build() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("done")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    let agent = AgentBuilder::new(model).compile();

    let messages = vec![Message::user_text("hello")];
    let result = agent.invoke(messages).await.unwrap();

    assert_eq!(result.iterations, 1);
    assert!(result.is_success());
}

#[tokio::test]
async fn test_builder_with_config() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("done")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    let agent = AgentBuilder::new(model)
        .system("你是助手".to_string())
        .max_iterations(20)
        .compile();

    let messages = vec![Message::user_text("hello")];
    let result = agent.invoke(messages).await.unwrap();

    assert!(result.is_success());
}

#[tokio::test]
async fn test_builder_with_tool() {
    let def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo tool".to_string(),
        parameters: ToolSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } }
        })),
        cache_control: None,
    };

    let reg = ExecutableTool::safe(def, |args| {
        let m = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(serde_json::json!(format!("reply: {}", m))) }
    });

    let response = ChatResponse::new(
        vec![ContentBlock::text("ok")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let _agent = AgentBuilder::new(model).tool(reg).compile();

    // 如果 build() 成功，说明 tool() 方法工作正常
}

#[test]
fn test_builder_chain_api() {
    // 测试链式调用 API 的编译正确性
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let def = ToolDefinition {
        name: "test_tool".to_string(),
        description: "test".to_string(),
        parameters: ToolSchema::new(serde_json::json!({"type": "object", "properties": {}})),
        cache_control: None,
    };
    let reg = ExecutableTool::safe(def, |_| async { Ok(serde_json::json!("done")) });

    // 完整链式调用
    let _agent = AgentBuilder::new(model)
        .system("你是测试助手".to_string())
        .tool(reg)
        .max_iterations(15)
        .compile();
}

// ─── 糖衣 API 测试 ───

#[tokio::test]
async fn test_create_agent() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hi")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let agent = lellm_agent::create_agent(model);
    let messages = vec![Message::user_text("hello")];
    let result = agent.invoke(messages).await.unwrap();

    assert!(result.is_success());
}

#[tokio::test]
async fn test_create_agent_with_tools() {
    let def = ToolDefinition {
        name: "greet".to_string(),
        description: "greet someone".to_string(),
        parameters: ToolSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } }
        })),
        cache_control: None,
    };
    let reg = ExecutableTool::safe(def, |args| {
        let n = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("world")
            .to_string();
        async move { Ok(serde_json::json!(format!("hello {}", n))) }
    });

    let response = ChatResponse::new(
        vec![ContentBlock::text("done")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let _agent = lellm_agent::create_agent_with_tools(model, vec![reg]);
    // 如果构建成功，API 即工作正常
}

#[tokio::test]
async fn test_create_agent_with_system() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let agent = lellm_agent::create_agent_with_system(model, "你是助手".to_string());

    let messages = vec![Message::user_text("hi")];
    let result = agent.invoke(messages).await.unwrap();

    assert!(result.is_success());
}

#[test]
fn test_create_agent_full() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok")],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test".to_string(),
    };

    let def = ToolDefinition {
        name: "t".to_string(),
        description: "test".to_string(),
        parameters: ToolSchema::new(serde_json::json!({"type": "object", "properties": {}})),
        cache_control: None,
    };
    let reg = ExecutableTool::safe(def, |_| async { Ok(serde_json::json!("ok")) });

    let _agent = lellm_agent::create_agent_full(model, "你是助手".to_string(), vec![reg], 20);
}

// ─── 上下文预算管理测试 ───

#[test]
fn test_estimate_tokens_ascii() {
    // "Hello World" ≈ 11/4 ≈ 2 tokens + 4 overhead = ~6
    let msg = Message::user_text("Hello World");
    let tokens = estimate_message(&msg);
    assert!(tokens >= 4 && tokens <= 10);
}

#[test]
fn test_estimate_tokens_chinese() {
    // "你好世界" ≈ 4 * 1.5 = 6 tokens + 4 overhead = ~10
    let msg = Message::user_text("你好世界");
    let tokens = estimate_message(&msg);
    assert!(tokens >= 6 && tokens <= 15);
}

#[test]
fn test_estimate_tokens_empty() {
    let tokens = estimate_tokens(&[]);
    assert_eq!(tokens, 0);
}

#[test]
fn test_truncate_tool_result_short() {
    let budget = ContextBudget::default();
    let short = lellm_core::text_block("short result".to_string());
    let result = budget.truncate_tool_result_blocks(&short);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_truncate_tool_result_long() {
    let budget = ContextBudget {
        max_tool_result_chars: 10,
        ..Default::default()
    };
    let long = lellm_core::text_block("0123456789ABCDEFG".to_string());
    let result = budget.truncate_tool_result_blocks(&long);
    assert!(result.len() >= 1);
    let text = result
        .iter()
        .filter_map(|b: &lellm_core::ContentBlock| b.as_text())
        .collect::<String>();
    assert!(text.starts_with("0123456789"));
    assert!(text.contains("[truncated"));
}

#[test]
fn test_should_compact() {
    let budget = ContextBudget {
        max_tokens: 1000,
        warning_ratio: 0.8,
        ..Default::default()
    };
    assert!(!budget.should_compact(500)); // 50%
    assert!(!budget.should_compact(799)); // 79.9%
    assert!(!budget.should_compact(800)); // 80% exactly, threshold is strict >
    assert!(budget.should_compact(801)); // 80.1%
    assert!(budget.should_compact(900)); // 90%
}

#[test]
fn test_local_compactor_no_op_when_under_limit() {
    let budget = ContextBudget {
        keep_recent_turns: 5,
        ..Default::default()
    };

    // 只有 2 个 turn，不需要压缩
    let messages = vec![
        Message::user_text("turn 1 user"),
        Message::assistant_text("turn 1 assistant"),
        Message::user_text("turn 2 user"),
        Message::assistant_text("turn 2 assistant"),
    ];

    let compactor = LocalCompactor::new();
    let result = compactor.compact(&messages, &budget);

    // 不需要压缩，原样返回
    assert_eq!(result.messages.len(), messages.len());
    assert_eq!(result.removed_messages, 0);
}

#[test]
fn test_local_compactor_compresses_old_turns() {
    let budget = ContextBudget {
        keep_recent_turns: 1,
        ..Default::default()
    };

    let mut messages = Vec::new();
    // 创建 3 个 turns（6 条消息）
    for i in 1..=3 {
        messages.push(Message::user(lellm_core::text_block(format!("user {}", i))));
        messages.push(Message::assistant(lellm_core::text_block(format!(
            "assistant {}",
            i
        ))));
    }

    let compactor = LocalCompactor::new();
    let result = compactor.compact(&messages, &budget);

    // 移除了旧 turns（6 条 → summary + 2 条 = 3 条）
    assert!(result.removed_messages > 0);
    assert!(result.messages.len() < messages.len());
    // 摘要消息应存在（System 角色）
    assert!(result
        .messages
        .iter()
        .any(|m| matches!(m, Message::System { content } if content.iter().any(|b| b.as_text().map(|t| t.contains("Compressed")).unwrap_or(false)))));
}
