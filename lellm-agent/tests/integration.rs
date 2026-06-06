use lellm_agent::{
    AgentBuilder, ToolArgs, ToolCategory, ToolExecutor, ToolRegistration, ToolUseLoop,
};
use lellm_agent::schemars::JsonSchema;
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage, ToolCall, ToolDefinition};
use lellm_macros::ToolDefinition as ToolDefinitionDerive;
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

#[tokio::test]
async fn test_tool_use_loop_no_tool_calls() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hello".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        provider,
        model: "test-model".to_string(),
    };

    let executor = ToolExecutor::new();
    let messages = vec![Message::User {
        content: lellm_core::text_block("test".to_string()),
    }];

    let result = ToolUseLoop::simple(model, executor)
        .with_max_iterations(5)
        .execute(messages)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(!result.response.has_tool_calls());
}

#[tokio::test]
async fn test_tool_executor_register_and_execute() {
    let mut executor = ToolExecutor::new();
    let def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo tool".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            }
        }),
    };
    executor.register(
        "echo",
        ToolRegistration::safe(def, |args: &serde_json::Value| {
            let args_clone = args.clone();
            async move {
                let text = args_clone
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(format!("echo: {}", text))
            }
        }),
    );

    let call = ToolCall {
        id: "1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::json!({"text": "hello"}),
    };

    let result = executor.execute(&call).await;
    assert!(matches!(result, Ok(ref s) if s == "echo: hello"));
}

#[test]
fn test_tool_category() {
    assert_eq!(ToolCategory::FILE_IO.0, "file_io");
    assert_eq!(ToolCategory::NETWORK.0, "network");
    assert_eq!(ToolCategory::DATABASE.0, "database");
}

// ─── ToolArgs trait + derive(ToolDefinition) 测试 ───

#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "weather_search", description = "搜索天气信息")]
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
    let schema = &def.parameters;
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
    let schema = WeatherArgs::__schema();
    let def_schema = WeatherArgs::tool_definition().parameters;
    assert_eq!(schema, def_schema);
}

#[test]
fn test_tool_definition_default_name() {
    // 不指定 name，应自动转换为 snake_case
    #[derive(JsonSchema, ToolDefinitionDerive)]
    #[tool(description = "测试默认命名")]
    struct MySearchTool {
        pub query: String,
    }

    assert_eq!(MySearchTool::NAME, "my_search_tool");
    assert_eq!(MySearchTool::__name(), "my_search_tool");
}

#[test]
fn test_option_type_inference() {
    // 验证 Option<T> 正确推导内部类型
    #[derive(JsonSchema, ToolDefinitionDerive)]
    #[tool(name = "typed_test", description = "测试类型推导")]
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
    let required = def.parameters.get("required");
    assert!(required.map_or(true, |r| r.as_array().map_or(true, |a| a.is_empty())));
}

// ─── AgentBuilder 测试 ───

#[tokio::test]
async fn test_builder_basic_build() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("done".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test-model".to_string(),
    };

    let agent = AgentBuilder::new(model).build();

    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("hello".to_string()),
        }])
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(result.is_success());
}

#[tokio::test]
async fn test_builder_with_config() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("done".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test-model".to_string(),
    };

    let agent = AgentBuilder::new(model)
        .system_prompt("你是助手".to_string())
        .max_iterations(20)
        .build();

    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("hello".to_string()),
        }])
        .await
        .unwrap();

    assert!(result.is_success());
}

#[tokio::test]
async fn test_builder_with_tool() {
    let mut executor = ToolExecutor::new();
    executor.register(
        "echo",
        ToolRegistration::safe(
            ToolDefinition {
                name: "echo".to_string(),
                description: "echo".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                }),
            },
            |args| {
                let t = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                async move { Ok(format!("echo: {}", t)) }
            },
        ),
    );

    // 验证 AgentBuilder 可以通过 tool() 方法注册工具
    let def = ToolDefinition {
        name: "echo".to_string(),
        description: "echo tool".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } }
        }),
    };

    let reg = ToolRegistration::safe(def, |args| {
        let m = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(format!("reply: {}", m)) }
    });

    let response = ChatResponse::new(
        vec![ContentBlock::text("ok".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let _agent = AgentBuilder::new(model).tool(reg).build();

    // 如果 build() 成功，说明 tool() 方法工作正常
}

#[test]
fn test_builder_chain_api() {
    // 测试链式调用 API 的编译正确性
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let def = ToolDefinition {
        name: "test_tool".to_string(),
        description: "test".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    };
    let reg = ToolRegistration::safe(def, |_| async { Ok("done".to_string()) });

    // 完整链式调用
    let _agent = AgentBuilder::new(model)
        .system_prompt("你是测试助手".to_string())
        .tool(reg)
        .max_iterations(15)
        .build();
}

// ─── 糖衣 API 测试 ───

#[tokio::test]
async fn test_create_agent() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("hi".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let agent = lellm_agent::create_agent(model);
    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("hello".to_string()),
        }])
        .await
        .unwrap();

    assert!(result.is_success());
}

#[tokio::test]
async fn test_create_agent_with_tools() {
    let def = ToolDefinition {
        name: "greet".to_string(),
        description: "greet someone".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } }
        }),
    };
    let reg = ToolRegistration::safe(def, |args| {
        let n = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("world")
            .to_string();
        async move { Ok(format!("hello {}", n)) }
    });

    let response = ChatResponse::new(
        vec![ContentBlock::text("done".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let _agent = lellm_agent::create_agent_with_tools(model, vec![reg]);
    // 如果构建成功，API 即工作正常
}

#[tokio::test]
async fn test_create_agent_with_system() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let agent = lellm_agent::create_agent_with_system(model, "你是助手".to_string());

    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("hi".to_string()),
        }])
        .await
        .unwrap();

    assert!(result.is_success());
}

#[test]
fn test_create_agent_full() {
    let response = ChatResponse::new(
        vec![ContentBlock::text("ok".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));
    let model = ResolvedModel {
        provider,
        model: "test".to_string(),
    };

    let def = ToolDefinition {
        name: "t".to_string(),
        description: "test".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    };
    let reg = ToolRegistration::safe(def, |_| async { Ok("ok".to_string()) });

    let _agent = lellm_agent::create_agent_full(model, "你是助手".to_string(), vec![reg], 20);
}
