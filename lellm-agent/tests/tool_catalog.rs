//! ToolCatalog → ToolExecutor 链路集成测试。
//!
//! 验证：
//! - Case 1: 静态注册回归（.register() 行为不变）
//! - Case 2: StaticCatalog → resolve_tools → execute_batch_with
//! - Case 3: Mock 动态目录 → resolve_tools → execute_batch_with
//! - Case 4: ToolUnavailable 语义（动态目录未预解析）
//! - Case 5: 热刷新（discover → call → refresh → new tool visible）
//! - Case 6: ToolRegistration Clone/Send/Sync 编译验证
//! - Case 7: definitions() 解析
//! - Case 8: execute_batch_with 工具不存在 → NotFound
//! - Case 9: 计数器验证 snapshot 调用次数
//! - Case 10: has_tools 行为

use lellm_agent::{
    execute_batch_with, BackoffStrategy, RetryPolicy, ToolCatalog, ToolExecutor, ToolRegistration,
};
use lellm_core::{ToolCall, ToolDefinition, ToolErrorKind};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;

// ─── 辅助函数 ────────────────────────────────────────────────────

fn make_echo_tool(name: &str) -> ToolRegistration {
    let name_str = name.to_string();
    ToolRegistration::safe(
        ToolDefinition {
            name: name_str.clone(),
            description: format!("echo {}", name_str),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } }
            }),
        },
        move |args| {
            let name = name_str.clone();
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move { Ok(format!("[{}] {}", name, text)) }
        },
    )
}

fn make_tool_call(id: &str, name: &str, arg: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::json!({"text": arg}),
    }
}

fn default_retry() -> RetryPolicy {
    RetryPolicy::new(1, BackoffStrategy::Fixed(std::time::Duration::from_millis(0)))
}

/// 从 Message 中提取文本内容
fn extract_tool_text(msg: &lellm_core::Message) -> String {
    msg.content()
        .iter()
        .filter_map(|b: &lellm_core::ContentBlock| b.as_text())
        .collect::<Vec<_>>()
        .join("")
}

// ─── Case 1: 静态注册回归 ────────────────────────────────────────

#[tokio::test]
async fn test_static_register_regression() {
    let mut executor = ToolExecutor::new();
    executor.register("echo", make_echo_tool("echo"));

    let call = make_tool_call("1", "echo", "hello");
    let result = executor.execute(&call).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "[echo] hello");
}

#[tokio::test]
async fn test_static_batch_regression() {
    let mut executor = ToolExecutor::new();
    executor.register("echo", make_echo_tool("echo"));
    executor.register("greet", make_echo_tool("greet"));

    let calls = vec![
        make_tool_call("1", "echo", "hi"),
        make_tool_call("2", "greet", "world"),
    ];

    let batch = executor.execute_batch(&calls).await;

    assert_eq!(batch.results.len(), 2);
    assert!(!batch.panicked);

    let text0 = extract_tool_text(&batch.results[0]);
    let text1 = extract_tool_text(&batch.results[1]);

    assert!(text0.contains("[echo] hi"));
    assert!(text1.contains("[greet] world"));
}

// ─── Case 2: resolve_tools → execute_batch_with ─────────────────

#[tokio::test]
async fn test_static_resolve_and_execute() {
    let mut executor = ToolExecutor::new();
    executor.register("echo", make_echo_tool("echo"));

    // resolve_tools 对 Static 应返回 O(1) 的 Arc 克隆
    let tools = executor.resolve_tools().await;
    assert!(tools.contains_key("echo"));

    // 使用 execute_batch_with 执行
    let calls = vec![make_tool_call("1", "echo", "static-catalog")];
    let batch = execute_batch_with(&calls, &tools, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[echo] static-catalog"));
}

// ─── Case 3: Mock 动态目录 ──────────────────────────────────────

/// 模拟 MCP 目录的可变工具集合。
struct MockDynamicCatalog {
    tools: Mutex<Vec<ToolRegistration>>,
}

impl MockDynamicCatalog {
    fn new(tools: Vec<ToolRegistration>) -> Self {
        Self {
            tools: Mutex::new(tools),
        }
    }

    async fn add_tool(&self, tool: ToolRegistration) {
        self.tools.lock().await.push(tool);
    }
}

#[async_trait::async_trait]
impl ToolCatalog for MockDynamicCatalog {
    async fn snapshot(&self) -> Vec<ToolRegistration> {
        self.tools.lock().await.clone()
    }
}

#[tokio::test]
async fn test_dynamic_catalog_resolve_and_execute() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("dynamic_echo")]);
    let mut executor = ToolExecutor::new();
    executor.catalog(Arc::new(catalog));

    // resolve_tools 对 Dynamic 应调用 snapshot()
    let tools = executor.resolve_tools().await;
    assert!(tools.contains_key("dynamic_echo"));

    let calls = vec![make_tool_call("1", "dynamic_echo", "dynamic-hello")];
    let batch = execute_batch_with(&calls, &tools, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[dynamic_echo] dynamic-hello"));
}

// ─── Case 4: ToolUnavailable 语义 ───────────────────────────────

#[tokio::test]
async fn test_dynamic_without_preresolution_returns_unavailable() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("some_tool")]);
    let mut executor = ToolExecutor::new();
    executor.catalog(Arc::new(catalog));

    // 不调用 resolve_tools()，直接 execute_batch → 应返回 ToolUnavailable
    let calls = vec![make_tool_call("1", "some_tool", "test")];
    let batch = executor.execute_batch(&calls).await;

    assert_eq!(batch.results.len(), 1);
    // 验证结果是错误
    assert!(batch.results[0].is_tool_error());
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("ToolUnavailable") || text.contains("not available"));
}

#[tokio::test]
async fn test_not_found_vs_tool_unavailable() {
    // Static: 工具不存在 → NotFound
    let mut static_executor = ToolExecutor::new();
    static_executor.register("echo", make_echo_tool("echo"));

    let call = make_tool_call("1", "non_existent", "test");
    let result = static_executor.execute(&call).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::NotFound);

    // Dynamic (未预解析): → ToolUnavailable
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("some_tool")]);
    let mut dynamic_executor = ToolExecutor::new();
    dynamic_executor.catalog(Arc::new(catalog));

    let result = dynamic_executor.execute(&call).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::ToolUnavailable);
}

// ─── Case 5: 热刷新 ──────────────────────────────────────────────

#[tokio::test]
async fn test_hot_refresh_new_tool_visible() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("tool_a")]);
    let catalog_ref = Arc::new(catalog);

    let mut executor = ToolExecutor::new();
    executor.catalog(catalog_ref.clone());

    // 第一次 resolve — 只有 tool_a
    let tools_v1 = executor.resolve_tools().await;
    assert!(tools_v1.contains_key("tool_a"));
    assert!(!tools_v1.contains_key("tool_b"));

    // 模拟刷新 — 添加 tool_b
    catalog_ref.add_tool(make_echo_tool("tool_b")).await;

    // 第二次 resolve — 应有 tool_a + tool_b
    let tools_v2 = executor.resolve_tools().await;
    assert!(tools_v2.contains_key("tool_a"));
    assert!(tools_v2.contains_key("tool_b"));

    // 用 v2 快照执行 tool_b
    let calls = vec![make_tool_call("1", "tool_b", "new-tool")];
    let batch = execute_batch_with(&calls, &tools_v2, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[tool_b] new-tool"));
}

#[tokio::test]
async fn test_snapshot_freezing_no_drift() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("tool_a")]);
    let catalog_ref = Arc::new(catalog);

    let mut executor = ToolExecutor::new();
    executor.catalog(catalog_ref.clone());

    // 获取快照
    let tools = executor.resolve_tools().await;

    // 刷新目录 — 添加新工具
    catalog_ref.add_tool(make_echo_tool("tool_b")).await;

    // 快照不应漂移 — 仍然只有 tool_a
    assert!(tools.contains_key("tool_a"));
    assert!(!tools.contains_key("tool_b"));
}

// ─── Case 6: Clone/Send/Sync 编译验证 ───────────────────────────

/// 编译时验证：ToolRegistration 满足 Clone + Send + Sync
fn assert_traits<T: Clone + Send + Sync>() {}

#[test]
fn test_tool_registration_traits() {
    assert_traits::<ToolRegistration>();
}

/// 编译时验证：Arc<dyn ToolCatalog> 可以作为 ToolSource::Dynamic 使用
#[tokio::test]
async fn test_catalog_arc_clone() {
    let catalog: Arc<dyn ToolCatalog> = Arc::new(MockDynamicCatalog::new(vec![]));
    // Arc 克隆应 O(1)
    let _clone = catalog.clone();
}

// ─── Case 7: definitions() 解析 ─────────────────────────────────

#[tokio::test]
async fn test_resolve_definitions_static() {
    let mut executor = ToolExecutor::new();
    executor.register("echo", make_echo_tool("echo"));
    executor.register("greet", make_echo_tool("greet"));

    let defs = executor.resolve_definitions().await;
    assert_eq!(defs.len(), 2);
    let names: Vec<_> = defs.iter().map(|d| &d.name).collect();
    assert!(names.contains(&&"echo".to_string()));
    assert!(names.contains(&&"greet".to_string()));
}

#[tokio::test]
async fn test_resolve_definitions_dynamic() {
    let catalog = MockDynamicCatalog::new(vec![
        make_echo_tool("mcp_tool_a"),
        make_echo_tool("mcp_tool_b"),
    ]);
    let mut executor = ToolExecutor::new();
    executor.catalog(Arc::new(catalog));

    let defs = executor.resolve_definitions().await;
    assert_eq!(defs.len(), 2);
    let names: Vec<_> = defs.iter().map(|d| &d.name).collect();
    assert!(names.contains(&&"mcp_tool_a".to_string()));
    assert!(names.contains(&&"mcp_tool_b".to_string()));
}

// ─── Case 8: execute_batch_with 工具不存在 → NotFound ───────────

#[tokio::test]
async fn test_execute_batch_with_not_found() {
    let tools: std::collections::HashMap<String, ToolRegistration> = HashMap::new();
    let calls = vec![make_tool_call("1", "ghost_tool", "test")];

    let batch = execute_batch_with(&calls, &tools, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    assert!(batch.results[0].is_tool_error());
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("NotFound") || text.contains("unknown tool"));
}

// ─── Case 9: 计数器验证 snapshot 调用次数 ────────────────────────

/// 带调用计数的 Mock 目录
struct CountingCatalog {
    counter: Arc<AtomicUsize>,
    tools: Vec<ToolRegistration>,
}

impl CountingCatalog {
    fn new(counter: Arc<AtomicUsize>, tools: Vec<ToolRegistration>) -> Self {
        Self { counter, tools }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for CountingCatalog {
    async fn snapshot(&self) -> Vec<ToolRegistration> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        self.tools.clone()
    }
}

#[tokio::test]
async fn test_snapshot_called_once_per_resolve() {
    let counter = Arc::new(AtomicUsize::new(0));
    let catalog = CountingCatalog::new(counter.clone(), vec![make_echo_tool("echo")]);
    let mut executor = ToolExecutor::new();
    executor.catalog(Arc::new(catalog));

    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // 第一次 resolve
    let _tools = executor.resolve_tools().await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // 第二次 resolve
    let _tools = executor.resolve_tools().await;
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

// ─── Case 10: has_tools 行为 ────────────────────────────────────

#[tokio::test]
async fn test_has_tools_static() {
    let empty = ToolExecutor::new();
    assert!(!empty.has_tools());

    let mut with_tool = ToolExecutor::new();
    with_tool.register("echo", make_echo_tool("echo"));
    assert!(with_tool.has_tools());
}

#[tokio::test]
async fn test_has_tools_dynamic() {
    let catalog = MockDynamicCatalog::new(vec![]);
    let mut executor = ToolExecutor::new();
    executor.catalog(Arc::new(catalog));

    // Dynamic 模式返回 true（假设目录可能提供工具）
    assert!(executor.has_tools());
}
