//! ToolCatalog → ToolExecutor 链路集成测试。
//!
//! 验证：
//! - Case 1: StaticCatalog → snapshot → execute_batch_with
//! - Case 2: Mock 动态目录 → snapshot → execute_batch_with
//! - Case 3: execute_batch_with 工具不存在 → NotFound
//! - Case 4: 热刷新（discover → call → refresh → new tool visible）
//! - Case 5: ToolRegistration Clone 编译验证
//! - Case 6: CompositeCatalog 遮蔽策略
//! - Case 7: 计数器验证 snapshot 调用次数
//! - Case 8: ToolSnapshot 基本行为

use lellm_agent::{
    execute_batch_with, BackoffStrategy, CompositeCatalog, ParallelSafety, RetryPolicy,
    StaticCatalog, ToolCatalog, ToolExecutor, ToolRegistration, ToolSnapshot,
};
use lellm_core::{ToolCall, ToolDefinition, ToolErrorKind};
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
            async move { Ok(serde_json::json!(format!("[{}] {}", name, text))) }
        },
    )
}

fn make_exclusive_tool(name: &str) -> ToolRegistration {
    let name_str = name.to_string();
    ToolRegistration::exclusive(
        ToolDefinition {
            name: name_str.clone(),
            description: format!("exclusive {}", name_str),
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
            async move { Ok(serde_json::json!(format!("[{}] {}", name, text))) }
        },
    )
}

fn make_category_tool(name: &str, cat: lellm_agent::ToolCategory) -> ToolRegistration {
    let name_str = name.to_string();
    ToolRegistration::category_exclusive(
        ToolDefinition {
            name: name_str.clone(),
            description: format!("category {}", name_str),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } }
            }),
        },
        cat,
        move |args| {
            let name = name_str.clone();
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move { Ok(serde_json::json!(format!("[{}] {}", name, text))) }
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

/// 从 Message 中提取 JSON 内容（第一个 ContentBlock）
fn extract_tool_text(msg: &lellm_core::Message) -> String {
    msg.content()
        .iter()
        .filter_map(|b: &lellm_core::ContentBlock| b.as_text())
        .collect::<Vec<_>>()
        .join("")
}

// ─── Case 1: StaticCatalog → snapshot → execute_batch_with ──────

#[tokio::test]
async fn test_static_catalog_snapshot_and_execute() {
    let catalog = StaticCatalog::from_tools(vec![make_echo_tool("echo")]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    assert!(snapshot.get("echo").is_some());

    let calls = vec![make_tool_call("1", "echo", "static-catalog")];
    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[echo] static-catalog"));
}

#[tokio::test]
async fn test_static_batch() {
    let catalog = StaticCatalog::from_tools(vec![
        make_echo_tool("echo"),
        make_echo_tool("greet"),
    ]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    let calls = vec![
        make_tool_call("1", "echo", "hi"),
        make_tool_call("2", "greet", "world"),
    ];

    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 2);
    assert!(!batch.panicked);

    let text0 = extract_tool_text(&batch.results[0]);
    let text1 = extract_tool_text(&batch.results[1]);

    assert!(text0.contains("[echo] hi"));
    assert!(text1.contains("[greet] world"));
}

// ─── Case 2: Mock 动态目录 ──────────────────────────────────────

/// 模拟 MCP 目录的可变工具集合。
struct MockDynamicCatalog {
    tools: Mutex<indexmap::IndexMap<String, ToolRegistration>>,
}

impl MockDynamicCatalog {
    fn new(tools: Vec<ToolRegistration>) -> Self {
        let mut map = indexmap::IndexMap::new();
        for t in tools {
            map.insert(t.definition().name.clone(), t);
        }
        Self {
            tools: Mutex::new(map),
        }
    }

    async fn add_tool(&self, tool: ToolRegistration) {
        self.tools
            .lock()
            .await
            .insert(tool.definition().name.clone(), tool);
    }
}

#[async_trait::async_trait]
impl ToolCatalog for MockDynamicCatalog {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        let guard = self.tools.lock().await;
        Arc::new(ToolSnapshot::new(guard.clone(), 0))
    }
}

#[tokio::test]
async fn test_dynamic_catalog_snapshot_and_execute() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("dynamic_echo")]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    assert!(snapshot.get("dynamic_echo").is_some());

    let calls = vec![make_tool_call("1", "dynamic_echo", "dynamic-hello")];
    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[dynamic_echo] dynamic-hello"));
}

// ─── Case 3: execute_batch_with 工具不存在 → NotFound ───────────

#[tokio::test]
async fn test_execute_batch_with_not_found() {
    let snapshot = ToolSnapshot::new(indexmap::IndexMap::new(), 0);
    let calls = vec![make_tool_call("1", "ghost_tool", "test")];

    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    assert!(batch.results[0].is_tool_error());
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("NotFound") || text.contains("unknown tool"));
}

#[tokio::test]
async fn test_executor_not_found() {
    let catalog = StaticCatalog::from_tools(vec![make_echo_tool("echo")]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    let call = make_tool_call("1", "non_existent", "test");
    let result = executor.execute_with_snapshot(&call, &snapshot).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind, ToolErrorKind::NotFound);
}

// ─── Case 4: 热刷新 ──────────────────────────────────────────────

#[tokio::test]
async fn test_hot_refresh_new_tool_visible() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("tool_a")]);
    let catalog_ref = Arc::new(catalog);
    let executor = ToolExecutor::with_catalog(catalog_ref.clone());

    // 第一次 snapshot — 只有 tool_a
    let snap_v1 = executor.snapshot().await;
    assert!(snap_v1.get("tool_a").is_some());
    assert!(snap_v1.get("tool_b").is_none());

    // 模拟刷新 — 添加 tool_b
    catalog_ref.add_tool(make_echo_tool("tool_b")).await;

    // 第二次 snapshot — 应有 tool_a + tool_b
    let snap_v2 = executor.snapshot().await;
    assert!(snap_v2.get("tool_a").is_some());
    assert!(snap_v2.get("tool_b").is_some());

    // 用 v2 快照执行 tool_b
    let calls = vec![make_tool_call("1", "tool_b", "new-tool")];
    let batch = execute_batch_with(&calls, &snap_v2, &default_retry()).await;

    assert_eq!(batch.results.len(), 1);
    let text = extract_tool_text(&batch.results[0]);
    assert!(text.contains("[tool_b] new-tool"));
}

#[tokio::test]
async fn test_snapshot_freezing_no_drift() {
    let catalog = MockDynamicCatalog::new(vec![make_echo_tool("tool_a")]);
    let catalog_ref = Arc::new(catalog);
    let executor = ToolExecutor::with_catalog(catalog_ref.clone());

    // 获取快照
    let snap = executor.snapshot().await;

    // 刷新目录 — 添加新工具
    catalog_ref.add_tool(make_echo_tool("tool_b")).await;

    // 快照不应漂移 — 仍然只有 tool_a
    assert!(snap.get("tool_a").is_some());
    assert!(snap.get("tool_b").is_none());
}

// ─── Case 5: Clone 编译验证 ─────────────────────────────────────

/// 编译时验证：ToolRegistration 满足 Clone + Send + Sync
fn assert_traits<T: Clone + Send + Sync>() {}

#[test]
fn test_tool_registration_traits() {
    assert_traits::<ToolRegistration>();
}

/// 编译时验证：Arc<dyn ToolCatalog> 可以克隆
#[tokio::test]
async fn test_catalog_arc_clone() {
    let catalog: Arc<dyn ToolCatalog> = Arc::new(MockDynamicCatalog::new(vec![]));
    let _clone = catalog.clone();
}

// ─── Case 6: CompositeCatalog 遮蔽策略 ───────────────────────────

#[tokio::test]
async fn test_composite_catalog_shadowing() {
    let high_priority = StaticCatalog::from_tools(vec![make_echo_tool("shared")]);
    let low_priority = StaticCatalog::from_tools(vec![
        make_echo_tool("shared"),
        make_echo_tool("only_in_low"),
    ]);

    let composite = CompositeCatalog::new(vec![
        Arc::new(high_priority),
        Arc::new(low_priority),
    ]);

    let snap = composite.snapshot().await;

    // shared 来自高优先级
    assert!(snap.get("shared").is_some());
    assert!(snap.get("only_in_low").is_some());
    // 因为遮蔽，shared 只出现一次
    assert_eq!(snap.len(), 2);
}

// ─── Case 7: 计数器验证 snapshot 调用次数 ────────────────────────

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
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        let mut map = indexmap::IndexMap::new();
        for t in &self.tools {
            map.insert(t.definition().name.clone(), t.clone());
        }
        Arc::new(ToolSnapshot::new(map, 0))
    }
}

#[tokio::test]
async fn test_snapshot_called_once_per_resolve() {
    let counter = Arc::new(AtomicUsize::new(0));
    let catalog = CountingCatalog::new(counter.clone(), vec![make_echo_tool("echo")]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // 第一次 snapshot
    let _snap = executor.snapshot().await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // 第二次 snapshot
    let _snap = executor.snapshot().await;
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

// ─── Case 8: ToolSnapshot 基本行为 ───────────────────────────────

#[tokio::test]
async fn test_tool_snapshot_basic() {
    let tools = vec![make_echo_tool("echo"), make_echo_tool("greet")];
    let mut map = indexmap::IndexMap::new();
    for t in tools {
        map.insert(t.definition().name.clone(), t);
    }
    let snap = ToolSnapshot::new(map, 42);

    assert!(snap.has_tools());
    assert_eq!(snap.len(), 2);
    assert_eq!(snap.version(), 42);
    assert!(snap.get("echo").is_some());
    assert!(snap.get("missing").is_none());

    // definitions 懒构建
    let defs = snap.definitions();
    assert_eq!(defs.len(), 2);
}

#[tokio::test]
async fn test_empty_snapshot() {
    let snap = ToolSnapshot::new(indexmap::IndexMap::new(), 0);

    assert!(!snap.has_tools());
    assert!(snap.is_empty());
    assert_eq!(snap.len(), 0);
    assert!(snap.definitions().is_empty());
}

// ─── Case 9: ParallelSafety 验证 ────────────────────────────────

#[tokio::test]
async fn test_parallel_safety_safe() {
    let catalog = StaticCatalog::from_tools(vec![
        make_echo_tool("safe_a"),
        make_echo_tool("safe_b"),
    ]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    let calls = vec![
        make_tool_call("1", "safe_a", "A"),
        make_tool_call("2", "safe_b", "B"),
    ];

    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 2);
    assert!(!batch.panicked);
}

#[tokio::test]
async fn test_parallel_safety_exclusive() {
    let catalog = StaticCatalog::from_tools(vec![
        make_exclusive_tool("excl_a"),
        make_exclusive_tool("excl_b"),
    ]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    let calls = vec![
        make_tool_call("1", "excl_a", "A"),
        make_tool_call("2", "excl_b", "B"),
    ];

    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 2);
    assert!(!batch.panicked);
}

#[tokio::test]
async fn test_parallel_safety_category_exclusive() {
    let catalog = StaticCatalog::from_tools(vec![
        make_category_tool("cat_a_1", lellm_agent::ToolCategory::FILE_IO),
        make_category_tool("cat_a_2", lellm_agent::ToolCategory::FILE_IO),
        make_category_tool("cat_b_1", lellm_agent::ToolCategory::NETWORK),
    ]);
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snapshot = executor.snapshot().await;
    let calls = vec![
        make_tool_call("1", "cat_a_1", "1"),
        make_tool_call("2", "cat_a_2", "2"),
        make_tool_call("3", "cat_b_1", "3"),
    ];

    let batch = execute_batch_with(&calls, &snapshot, &default_retry()).await;

    assert_eq!(batch.results.len(), 3);
    assert!(!batch.panicked);
}

// ─── Case 10: StaticCatalog empty ────────────────────────────────

#[tokio::test]
async fn test_empty_static_catalog() {
    let catalog = StaticCatalog::empty();
    let executor = ToolExecutor::with_catalog(Arc::new(catalog));

    let snap = executor.snapshot().await;
    assert!(snap.is_empty());
    assert!(!snap.has_tools());
}
