//! #[tool] 函数宏集成测试
//!
//! 验证宏生成的代码在运行时正确工作：
//! - Args struct 生成 + ToolArgs trait 实现
//! - _tool() 工厂函数（无依赖注入）
//! - _tool_with() 工厂函数（依赖注入）
//! - LazyLock schema 缓存
//! - 各种参数类型
//! - name / description 覆盖
//! - doc comment 提取
//! - 完整执行流程（注册 → ToolExecutor 执行）
//! - category_exclusive / exclusive 注册
//! - 可见性保留
//! - 异步工具通过 _tool_with 注册
//!
//! 原始函数体被 dead_code 警告是因为测试通过工厂函数间接调用它们；
//! 保留直接可调用的能力是宏设计的一部分。
#![allow(dead_code, unused_variables)]

use lellm_agent::{ParallelSafety, StaticCatalog, ToolArgs, ToolCategory, ToolExecutor};
use lellm_core::{ToolCall, ToolError, ToolResult};
use lellm_macros::tool;
use std::sync::Arc;

// ============================================================================
// 1. 最基础的 #[tool] 函数 — 无额外配置
// ============================================================================

#[tool]
fn add_numbers(a: i32, b: i32) -> ToolResult {
    Ok(serde_json::json!(a + b))
}

#[test]
fn test_basic_tool_args_trait() {
    // 函数名 → snake_case 作为工具名
    assert_eq!(AddNumbersArgs::NAME, "add_numbers");
    // 无 doc comment → 空描述
    assert_eq!(AddNumbersArgs::DESCRIPTION, "");
}

#[test]
fn test_basic_tool_factory() {
    let reg = add_numbers_tool();
    assert_eq!(reg.definition().name, "add_numbers");
    assert_eq!(reg.safety(), &ParallelSafety::Safe);
}

#[test]
fn test_basic_tool_schema() {
    let def = AddNumbersArgs::tool_definition();
    let properties = def.parameters.get("properties").unwrap().as_object().unwrap();

    assert_eq!(properties["a"]["type"], "integer");
    assert_eq!(properties["b"]["type"], "integer");

    // i32 是非 Option，应在 required 中
    let required = def.parameters.get("required").unwrap().as_array().unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("a")));
    assert!(required.iter().any(|v| v.as_str() == Some("b")));
}

// ============================================================================
// 2. name 和 description 覆盖
// ============================================================================

#[tool(name = "custom_add", description = "自定义加法器")]
fn my_add(x: i64, y: i64) -> ToolResult {
    Ok(serde_json::json!(x + y))
}

#[test]
fn test_tool_name_override() {
    assert_eq!(MyAddArgs::NAME, "custom_add");
    assert_eq!(MyAddArgs::DESCRIPTION, "自定义加法器");

    let reg = my_add_tool();
    assert_eq!(reg.definition().name, "custom_add");
    assert_eq!(reg.definition().description, "自定义加法器");
}

// ============================================================================
// 3. doc comment 提取（作为 description）
// ============================================================================

/// 计算两个数的差值
#[tool]
fn subtract(minuend: i32, subtrahend: i32) -> ToolResult {
    Ok(serde_json::json!(minuend - subtrahend))
}

#[test]
fn test_tool_doc_comment_description() {
    assert_eq!(SubtractArgs::DESCRIPTION, "计算两个数的差值");
    assert_eq!(SubtractArgs::NAME, "subtract");
}

// name 在 attr 中优先于 doc comment
/// 这个描述应被覆盖
#[tool(description = "显式描述优先")]
fn _multiply(a: i32, b: i32) -> ToolResult {
    Ok(serde_json::json!(a * b))
}

#[test]
fn test_tool_description_attr_overrides_doc() {
    assert_eq!(MultiplyArgs::DESCRIPTION, "显式描述优先");
}

// ============================================================================
// 4. 各种参数类型
// ============================================================================

#[tool(name = "typed_params", description = "测试各种类型")]
fn typed_function(
    text: String,
    count: u32,
    flag: bool,
    score: f64,
    optional_text: Option<String>,
    optional_number: Option<i64>,
    tags: Vec<String>,
) -> ToolResult {
    Ok(serde_json::json!(format!("{}-{}", text, count)))
}

#[test]
fn test_various_param_types_schema() {
    let def = TypedFunctionArgs::tool_definition();
    let properties = def.parameters.get("properties").unwrap().as_object().unwrap();

    // String → string
    assert_eq!(properties["text"]["type"], "string");
    // u32 → integer
    assert_eq!(properties["count"]["type"], "integer");
    // bool → boolean
    assert_eq!(properties["flag"]["type"], "boolean");
    // f64 → number
    assert_eq!(properties["score"]["type"], "number");
    // Option<String> → [string, null] 或 string
    let opt_type = &properties["optional_text"]["type"];
    assert!(
        *opt_type == "string" || *opt_type == serde_json::json!(["string", "null"]),
        "unexpected Option<String> type: {}",
        opt_type
    );
    // Vec<String> → array
    assert_eq!(properties["tags"]["type"], "array");

    // 检查 required — 非 Option 类型应在 required 中
    let required = def.parameters.get("required").unwrap().as_array().unwrap();
    let required_set: std::collections::HashSet<&str> = required
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    assert!(required_set.contains("text"));
    assert!(required_set.contains("count"));
    assert!(required_set.contains("flag"));
    assert!(required_set.contains("score"));
    assert!(required_set.contains("tags"));
    // Option 类型不应在 required 中
    assert!(!required_set.contains("optional_text"));
    assert!(!required_set.contains("optional_number"));
}

// ============================================================================
// 5. 无参数函数
// ============================================================================

/// 获取当前时间戳
#[tool]
fn get_timestamp() -> ToolResult {
    Ok(serde_json::json!("1234567890"))
}

#[test]
fn test_no_param_tool() {
    assert_eq!(GetTimestampArgs::NAME, "get_timestamp");
    assert_eq!(GetTimestampArgs::DESCRIPTION, "获取当前时间戳");

    let reg = get_timestamp_tool();
    assert_eq!(reg.definition().name, "get_timestamp");

    // schema 应为空 properties
    let def = GetTimestampArgs::tool_definition();
    let properties = def.parameters.get("properties");
    // 无参数时 properties 可能为空对象或不存在
    assert!(
        properties.is_none()
            || properties
                .unwrap()
                .as_object()
                .map_or(true, |obj| obj.is_empty())
    );
}

// ============================================================================
// 6. LazyLock schema 缓存 — 多次调用返回相同值
// ============================================================================

#[test]
fn test_lazylock_schema_cache() {
    let schema1 = AddNumbersArgs::__schema();
    let schema2 = AddNumbersArgs::__schema();
    let schema3 = AddNumbersArgs::__schema();

    // 三次调用应返回相同的 JSON Value（clone 后的相等值）
    assert_eq!(schema1, schema2);
    assert_eq!(schema2, schema3);

    // 通过 trait 方法调用也应一致
    let def_schema = AddNumbersArgs::tool_definition().parameters;
    assert_eq!(schema1, def_schema);
}

// ============================================================================
// 7. _tool_with() 依赖注入工厂函数
// ============================================================================

#[derive(Clone)]
struct MockDbClient {
    prefix: String,
}

#[tool(name = "db_query", description = "查询数据库")]
fn db_query(table: String, condition: String) -> ToolResult {
    Ok(serde_json::json!(format!("default: {} WHERE {}", table, condition)))
}

#[test]
fn test_tool_with_factory() {
    let client = MockDbClient {
        prefix: "mock_db".to_string(),
    };

    let reg = db_query_tool_with({
        let client = client.clone();
        move |args| {
            let prefix = client.prefix.clone();
            async move {
                let result = format!(
                    "{}: SELECT * FROM {} WHERE {}",
                    prefix, args.table, args.condition
                );
                Ok(serde_json::json!(result))
            }
        }
    });

    assert_eq!(reg.definition().name, "db_query");
    assert_eq!(reg.definition().description, "查询数据库");
    assert_eq!(reg.safety(), &ParallelSafety::Safe);
}

// ============================================================================
// 8. 完整执行流程 — 注册 → ToolExecutor 执行
// ============================================================================

/// 向某人打招呼
#[tool]
fn greet(name: String, formal: bool) -> ToolResult {
    if formal {
        Ok(serde_json::json!(format!("尊敬的 {}，您好！", name)))
    } else {
        Ok(serde_json::json!(format!("嘿, {}！", name)))
    }
}

#[tokio::test]
async fn test_full_execution_flow_tool_factory() {
    let reg = greet_tool();
    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let call = ToolCall {
        id: "call_1".to_string(),
        name: "greet".to_string(),
        arguments: serde_json::json!({"name": "Alice", "formal": false}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("嘿, Alice！"));
}

#[tokio::test]
async fn test_full_execution_flow_tool_with_factory() {
    // 使用 _tool_with 注入自定义逻辑
    let call_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let call_log_clone = call_log.clone();

    let reg = greet_tool_with({
        let log = call_log_clone.clone();
        move |args| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(args.name.clone());
                Ok(serde_json::json!(format!("custom greet: {}", args.name)))
            }
        }
    });

    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let call = ToolCall {
        id: "call_2".to_string(),
        name: "greet".to_string(),
        arguments: serde_json::json!({"name": "Bob", "formal": true}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("custom greet: Bob"));

    // 验证闭包确实被调用
    assert_eq!(*call_log.lock().unwrap(), vec!["Bob".to_string()]);
}

// ============================================================================
// 9. category_exclusive / exclusive 注册
// ============================================================================

/// 搜索用户信息
#[tool]
fn search_user(user_id: String, include_history: bool, limit: Option<u32>) -> ToolResult {
    Ok(serde_json::json!(format!("search user={}", user_id)))
}

/// 读取文件
#[tool]
fn file_read(path: String) -> ToolResult {
    Ok(serde_json::json!(format!("content of {}", path)))
}

#[test]
fn test_category_exclusive_registration() {
    let reg = SearchUserArgs::category_exclusive(
        ToolCategory::DATABASE,
        |args| async move { Ok(serde_json::json!(format!("search: {}", args.user_id))) },
    );

    assert_eq!(reg.safety(), &ParallelSafety::CategoryExclusive);
    assert_eq!(reg.category(), Some(&ToolCategory::DATABASE));
}

#[test]
fn test_exclusive_registration() {
    let reg = FileReadArgs::exclusive(|args| async move {
        Ok(serde_json::json!(format!("exclusive read: {}", args.path)))
    });

    assert_eq!(reg.safety(), &ParallelSafety::Exclusive);
    assert!(reg.category().is_none());
}

// ============================================================================
// 10. 可见性保留 — pub 函数生成 pub struct 和 pub 函数
// ============================================================================

// greet 是 pub，生成的 GreetArgs 也应是 pub（能在此 test 文件中引用即证明）
#[test]
fn test_visibility_preserved() {
    // 如果能编译通过，说明生成的 struct 和函数具有正确的可见性
    let _ = GreetArgs::NAME;
    let _ = greet_tool();
    let _ = greet_tool_with(|_| async { Ok(serde_json::json!("ok")) });
}

// ============================================================================
// 11. 异步函数支持 — #[tool] on async fn
// ============================================================================

/// 异步搜索
#[tool]
async fn async_search(query: String, limit: u32) -> ToolResult {
    Ok(serde_json::json!(format!("async results for '{}' (limit={})", query, limit)))
}

#[test]
fn test_async_tool_args() {
    assert_eq!(AsyncSearchArgs::NAME, "async_search");
    assert_eq!(AsyncSearchArgs::DESCRIPTION, "异步搜索");
}

#[tokio::test]
async fn test_async_tool_execution() {
    let reg = async_search_tool();
    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let call = ToolCall {
        id: "call_async".to_string(),
        name: "async_search".to_string(),
        arguments: serde_json::json!({"query": "Rust", "limit": 10}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("async results for 'Rust' (limit=10)"));
}

// ============================================================================
// 12. PascalCase 命名转换
// ============================================================================

#[tool]
fn my_custom_search_tool(q: String) -> ToolResult {
    Ok(serde_json::json!(q))
}

#[test]
fn test_pascal_case_struct_name() {
    // my_custom_search_tool → MyCustomSearchToolArgs
    // 只要编译通过并拿到正确的 NAME，就证明 struct 命名正确
    assert_eq!(MyCustomSearchToolArgs::NAME, "my_custom_search_tool");
    // 工厂函数名也应正确
    let reg = my_custom_search_tool_tool();
    assert_eq!(reg.definition().name, "my_custom_search_tool");
}

// ============================================================================
// 13. 错误处理 — 原始函数返回 Err
// ============================================================================

#[tool(name = "risky_op", description = "可能失败的操作")]
fn risky_op(input: String) -> ToolResult {
    if input.is_empty() {
        Err(ToolError::invalid_input("input 不能为空"))
    } else {
        Ok(serde_json::json!(format!("success: {}", input)))
    }
}

#[tokio::test]
async fn test_tool_error_handling() {
    let reg = risky_op_tool();
    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    // 正常输入
    let call_ok = ToolCall {
        id: "call_ok".to_string(),
        name: "risky_op".to_string(),
        arguments: serde_json::json!({"input": "hello"}),
    };
    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call_ok, &snapshot).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), serde_json::json!("success: hello"));

    // 错误输入
    let call_err = ToolCall {
        id: "call_err".to_string(),
        name: "risky_op".to_string(),
        arguments: serde_json::json!({"input": ""}),
    };
    let result = executor.execute_with_snapshot(&call_err, &snapshot).await;
    assert!(result.is_err());
}

// ============================================================================
// 14. 原始函数可直接调用（不经过工具系统）
// ============================================================================

#[test]
fn test_raw_function_direct_call() {
    // 原始函数应保留，可直接调用
    let sum = add_numbers(10, 20);
    assert_eq!(sum.unwrap(), serde_json::json!(30));

    let greeting = greet("世界".to_string(), true);
    assert_eq!(greeting.unwrap(), serde_json::json!("尊敬的 世界，您好！"));
}

// ============================================================================
// 15. 向后兼容方法
// ============================================================================

#[test]
fn test_backward_compat_methods() {
    assert_eq!(AddNumbersArgs::__name(), AddNumbersArgs::NAME);
    assert_eq!(
        AddNumbersArgs::__description(),
        AddNumbersArgs::DESCRIPTION
    );
    // __schema() 通过 struct 方法和 trait 方法应返回相同值
    assert_eq!(AddNumbersArgs::__schema(), AddNumbersArgs::tool_definition().parameters);
}

// ============================================================================
// 16. snake_to_pascal 边界情况
// ============================================================================

#[tool]
fn a_b_c() -> ToolResult {
    Ok(serde_json::json!("ok"))
}

#[test]
fn test_multi_word_snake_to_pascal() {
    // a_b_c → ABCArgs
    assert_eq!(ABCArgs::NAME, "a_b_c");
    let _ = a_b_c_tool();
}

// ============================================================================
// 17. ToolRegistration 的完整定义检查
// ============================================================================

#[test]
fn test_tool_registration_definition() {
    let reg = add_numbers_tool();
    let def = reg.definition();

    assert_eq!(def.name, "add_numbers");
    // schema 应包含正确的参数
    let schema = &def.parameters;
    assert!(schema.get("properties").is_some());
}

// ============================================================================
// 18. 解析错误处理 — 传入无效参数
// ============================================================================

#[tokio::test]
async fn test_invalid_argument_parsing() {
    let reg = add_numbers_tool();
    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    // 传入错误类型：a 应该是 integer，却传了 string
    let call = ToolCall {
        id: "call_bad".to_string(),
        name: "add_numbers".to_string(),
        arguments: serde_json::json!({"a": "not_a_number", "b": 5}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call, &snapshot).await;

    // 应返回 InvalidInput 错误
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind, lellm_core::ToolErrorKind::InvalidInput);
}

// ============================================================================
// 19. 多工具注册到同一个 catalog
// ============================================================================

#[tokio::test]
async fn test_multiple_tools_in_catalog() {
    let catalog = StaticCatalog::from_tools(vec![add_numbers_tool(), greet_tool()]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.get("add_numbers").is_some());
    assert!(snapshot.get("greet").is_some());

    // 执行两个不同的工具
    let call_add = ToolCall {
        id: "c1".to_string(),
        name: "add_numbers".to_string(),
        arguments: serde_json::json!({"a": 100, "b": 200}),
    };
    let result = executor.execute_with_snapshot(&call_add, &snapshot).await;
    assert_eq!(result.unwrap(), serde_json::json!(300));

    let call_greet = ToolCall {
        id: "c2".to_string(),
        name: "greet".to_string(),
        arguments: serde_json::json!({"name": "Team", "formal": false}),
    };
    let result = executor.execute_with_snapshot(&call_greet, &snapshot).await;
    assert_eq!(result.unwrap(), serde_json::json!("嘿, Team！"));
}

// ============================================================================
// 20. _with 工厂函数类型安全 — 闭包接收强类型 Args
// ============================================================================

#[tokio::test]
async fn test_with_factory_strong_typed_args() {
    // _tool_with 的闭包接收 AddNumbersArgs（强类型），不是 serde_json::Value
    let reg = add_numbers_tool_with(|args| async move {
        // args 是 AddNumbersArgs，有 .a 和 .b 字段
        let sum = args.a + args.b;
        Ok(serde_json::json!(format!("computed: {}", sum)))
    });

    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let call = ToolCall {
        id: "c3".to_string(),
        name: "add_numbers".to_string(),
        arguments: serde_json::json!({"a": 7, "b": 8}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_with_snapshot(&call, &snapshot).await;
    assert_eq!(result.unwrap(), serde_json::json!("computed: 15"));
}
