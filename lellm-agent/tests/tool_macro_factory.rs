//! #[tool] 函数宏集成测试 — 工厂函数与执行流
//!
//! 覆盖：_tool_with() 依赖注入, 完整执行流程, 多工具 catalog,
//! _with 工厂函数类型安全

#![allow(dead_code, unused_variables)]

use lellm_agent::{ParallelSafety, StaticCatalog, ToolExecutor};
use lellm_core::{ToolCall, ToolResult};
use lellm_derive::tool;
use std::sync::Arc;

// ============================================================================
// 工具定义
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
// 1. _tool_with() 依赖注入工厂函数
// ============================================================================

#[derive(Clone)]
struct MockDbClient {
    prefix: String,
}

#[tool(name = "db_query", description = "查询数据库")]
fn db_query(table: String, condition: String) -> ToolResult {
    Ok(serde_json::json!(format!(
        "default: {} WHERE {}",
        table, condition
    )))
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
// 2. 完整执行流程 — 注册 → ToolExecutor 执行
// ============================================================================

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
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("嘿, Alice！"));
}

#[tokio::test]
async fn test_full_execution_flow_tool_with_factory() {
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
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;

    assert!(result.is_ok());
    let val = result.unwrap();
    assert_eq!(val, serde_json::json!("custom greet: Bob"));

    assert_eq!(*call_log.lock().unwrap(), vec!["Bob".to_string()]);
}

// ============================================================================
// 3. 多工具注册到同一个 catalog
// ============================================================================

#[tokio::test]
async fn test_multiple_tools_in_catalog() {
    let catalog = StaticCatalog::from_tools(vec![add_numbers_tool(), greet_tool()]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.get("add_numbers").is_some());
    assert!(snapshot.get("greet").is_some());

    let call_add = ToolCall {
        id: "c1".to_string(),
        name: "add_numbers".to_string(),
        arguments: serde_json::json!({"a": 100, "b": 200}),
    };
    let result = executor
        .execute_one_with_snapshot(&call_add, &snapshot)
        .await;
    assert_eq!(result.unwrap(), serde_json::json!(300));

    let call_greet = ToolCall {
        id: "c2".to_string(),
        name: "greet".to_string(),
        arguments: serde_json::json!({"name": "Team", "formal": false}),
    };
    let result = executor
        .execute_one_with_snapshot(&call_greet, &snapshot)
        .await;
    assert_eq!(result.unwrap(), serde_json::json!("嘿, Team！"));
}

// ============================================================================
// 4. _with 工厂函数类型安全 — 闭包接收强类型 Args
// ============================================================================

#[tokio::test]
async fn test_with_factory_strong_typed_args() {
    let reg = add_numbers_tool_with(|args| async move {
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
    let result = executor.execute_one_with_snapshot(&call, &snapshot).await;
    assert_eq!(result.unwrap(), serde_json::json!("computed: 15"));
}
