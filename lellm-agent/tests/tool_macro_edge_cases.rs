//! #[tool] 函数宏集成测试 — 边缘情况
//!
//! 覆盖：注册模式, 可见性, 异步, PascalCase 命名,
//! 错误处理, 原始函数调用, 向后兼容, 解析错误, snake_to_pascal 边界

#![allow(dead_code, unused_variables)]

use lellm_agent::{StaticCatalog, ToolArgs, ToolCategory, ToolExecutor};
use lellm_core::{ToolCall, ToolError, ToolErrorKind, ToolResult};
use lellm_tool::tool;
use std::sync::Arc;

// ============================================================================
// 工具定义（供多个测试使用）
// ============================================================================

#[tool]
fn add_numbers(a: i32, b: i32) -> ToolResult {
    Ok(serde_json::json!(a + b))
}

/// 向某人打招呼
#[tool]
fn greet(name: String, formal: bool) -> ToolResult {
    if formal {
        Ok(serde_json::json!(format!("尊敬的 {}，您好！", name)))
    } else {
        Ok(serde_json::json!(format!("嘿, {}！", name)))
    }
}

// ============================================================================
// 1. category_exclusive / exclusive 注册
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
    let reg = SearchUserArgs::category_exclusive(ToolCategory::DATABASE, |args| async move {
        Ok(serde_json::json!(format!("search: {}", args.user_id)))
    });

    assert_eq!(
        reg.safety(),
        &lellm_agent::ParallelSafety::CategoryExclusive
    );
    assert_eq!(reg.category(), Some(&ToolCategory::DATABASE));
}

#[test]
fn test_exclusive_registration() {
    let reg = FileReadArgs::exclusive(|args| async move {
        Ok(serde_json::json!(format!("exclusive read: {}", args.path)))
    });

    assert_eq!(reg.safety(), &lellm_agent::ParallelSafety::Exclusive);
    assert!(reg.category().is_none());
}

// ============================================================================
// 2. 可见性保留 — pub 函数生成 pub struct 和 pub 函数
// ============================================================================

#[test]
fn test_visibility_preserved() {
    let _ = GreetArgs::NAME;
    let _ = greet_tool();
    let _ = greet_tool_with(|_| async { Ok(serde_json::json!("ok")) });
}

// ============================================================================
// 3. 异步函数支持 — #[tool] on async fn
// ============================================================================

/// 异步搜索
#[tool]
async fn async_search(query: String, limit: u32) -> ToolResult {
    Ok(serde_json::json!(format!(
        "async results for '{}' (limit={})",
        query, limit
    )))
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
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(
        val,
        serde_json::json!("async results for 'Rust' (limit=10)")
    );
}

// ============================================================================
// 4. PascalCase 命名转换
// ============================================================================

#[tool]
fn my_custom_search_tool(q: String) -> ToolResult {
    Ok(serde_json::json!(q))
}

#[test]
fn test_pascal_case_struct_name() {
    assert_eq!(MyCustomSearchToolArgs::NAME, "my_custom_search_tool");
    let reg = my_custom_search_tool_tool();
    assert_eq!(reg.definition().name, "my_custom_search_tool");
}

// ============================================================================
// 5. 错误处理 — 原始函数返回 Err
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

    let call_ok = ToolCall {
        id: "call_ok".to_string(),
        name: "risky_op".to_string(),
        arguments: serde_json::json!({"input": "hello"}),
    };
    let snapshot = executor.snapshot().await;
    let result = executor
        .execute_one_with_snapshot(&call_ok, &snapshot)
        .await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), serde_json::json!("success: hello"));

    let call_err = ToolCall {
        id: "call_err".to_string(),
        name: "risky_op".to_string(),
        arguments: serde_json::json!({"input": ""}),
    };
    let result = executor
        .execute_one_with_snapshot(&call_err, &snapshot)
        .await;
    assert!(result.is_err());
}

// ============================================================================
// 6. 原始函数可直接调用（不经过工具系统）
// ============================================================================

#[test]
fn test_raw_function_direct_call() {
    let sum = add_numbers(10, 20);
    assert_eq!(sum.unwrap(), serde_json::json!(30));

    let greeting = greet("世界".to_string(), true);
    assert_eq!(greeting.unwrap(), serde_json::json!("尊敬的 世界，您好！"));
}

// ============================================================================
// 7. 向后兼容方法
// ============================================================================

#[test]
fn test_backward_compat_methods() {
    assert_eq!(AddNumbersArgs::__name(), AddNumbersArgs::NAME);
    assert_eq!(AddNumbersArgs::__description(), AddNumbersArgs::DESCRIPTION);
    assert_eq!(
        AddNumbersArgs::schema(),
        AddNumbersArgs::tool_definition().parameters
    );
}

// ============================================================================
// 8. snake_to_pascal 边界情况
// ============================================================================

#[tool]
fn a_b_c() -> ToolResult {
    Ok(serde_json::json!("ok"))
}

#[test]
fn test_multi_word_snake_to_pascal() {
    assert_eq!(ABCArgs::NAME, "a_b_c");
    let _ = a_b_c_tool();
}

// ============================================================================
// 9. ExecutableTool 的完整定义检查
// ============================================================================

#[test]
fn test_tool_registration_definition() {
    let reg = add_numbers_tool();
    let def = reg.definition();

    assert_eq!(def.name, "add_numbers");
    let schema = def.parameters.as_value();
    assert!(schema.get("properties").is_some());
}

// ============================================================================
// 10. 解析错误处理 — 传入无效参数
// ============================================================================

#[tokio::test]
async fn test_invalid_argument_parsing() {
    let reg = add_numbers_tool();
    let catalog = StaticCatalog::from_tools(vec![reg]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let call = ToolCall {
        id: "call_bad".to_string(),
        name: "add_numbers".to_string(),
        arguments: serde_json::json!({"a": "not_a_number", "b": 5}),
    };

    let snapshot = executor.snapshot().await;
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::InvalidInput);
}
