//! 工具系统 — 注册、定义、执行、目录抽象。
//!
//! 独立的工具子系统，被 runtime 层使用。

mod args;
mod executor;

pub use args::ToolArgs;
pub use executor::{
    BatchExecutionResult, ParallelSafety, ToolCategory, ToolExecutor, ToolRegistration,
    execute_batch_with,
};

/// 异步工具函数类型（executor 内部使用）
pub(crate) type ToolFn = std::sync::Arc<
    dyn Fn(
            &serde_json::Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = lellm_core::ToolResult> + Send>>
        + Send
        + Sync,
>;

// ─── 工具快照 ────────────────────────────────────────────────────

/// 工具快照 — 冻结视图（Frozen View）。
///
/// 一旦创建，快照内容不再变化。通过 `version` 区分不同时刻的快照。
/// `definitions` 通过 `OnceLock` 懒构建——大部分轮次不需要定义列表。
pub struct ToolSnapshot {
    version: u64,
    tools: std::sync::Arc<indexmap::IndexMap<String, ToolRegistration>>,
    definitions: std::sync::OnceLock<Vec<lellm_core::ToolDefinition>>,
}

impl ToolSnapshot {
    /// 从工具映射构建快照。
    pub fn new(tools: indexmap::IndexMap<String, ToolRegistration>, version: u64) -> Self {
        Self {
            version,
            tools: std::sync::Arc::new(tools),
            definitions: std::sync::OnceLock::new(),
        }
    }

    /// 按名称查找工具注册信息。
    pub fn get(&self, name: &str) -> Option<&ToolRegistration> {
        self.tools.get(name)
    }

    /// 获取所有工具定义（懒构建）。
    pub fn definitions(&self) -> &[lellm_core::ToolDefinition] {
        self.definitions
            .get_or_init(|| self.tools.values().map(|t| t.definition.clone()).collect())
    }

    /// 是否有工具。
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 快照版本号。
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

// ─── 工具目录抽象 ────────────────────────────────────────────────

/// 工具目录 — 静态或动态的工具集合。
///
/// **设计目标：**
/// - 让 `ToolExecutor` 不关心工具来源（静态注册 vs MCP 发现）
/// - 每轮迭代调用 `snapshot()` 一次，固定本轮工具集（避免同轮不一致）
/// - `ToolRegistration` 必须 `Clone + Send + Sync`（快照在内存中传递）
///
/// **快照时机：**
/// - `ToolUseLoop::execute()` — 每轮迭代开始前调用一次
/// - `ToolUseLoop::execute_stream()` — 每轮迭代开始前调用一次
/// - **禁止**在 `execute_batch` 内部调用（会导致同轮工具集漂移）
#[async_trait::async_trait]
pub trait ToolCatalog: Send + Sync {
    /// 获取当前所有工具注册的快照。
    ///
    /// 返回的快照在调用瞬间冻结。
    /// 后续调用可能返回不同的工具集（动态目录刷新）。
    async fn snapshot(&self) -> std::sync::Arc<ToolSnapshot>;
}

/// 静态工具目录 — 构建后不可变的工具集合。
pub struct StaticCatalog {
    snapshot: std::sync::Arc<ToolSnapshot>,
}

impl StaticCatalog {
    /// 从工具注册列表构建静态目录。
    pub fn from_tools(tools: Vec<ToolRegistration>) -> Self {
        let mut map = indexmap::IndexMap::with_capacity(tools.len());
        for reg in tools {
            map.insert(reg.definition.name.clone(), reg);
        }
        Self {
            snapshot: std::sync::Arc::new(ToolSnapshot::new(map, 0)),
        }
    }

    /// 空目录。
    pub fn empty() -> Self {
        Self {
            snapshot: std::sync::Arc::new(ToolSnapshot::new(indexmap::IndexMap::new(), 0)),
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for StaticCatalog {
    async fn snapshot(&self) -> std::sync::Arc<ToolSnapshot> {
        self.snapshot.clone()
    }
}

/// 组合目录 — 按优先级合并多个工具源。
///
/// **遮蔽策略（Shadowing）：** 靠前的源优先级高，同名工具被遮蔽。
/// 遮蔽发生时通过 `tracing::warn!` 记录结构化日志。
pub struct CompositeCatalog {
    sources: Vec<std::sync::Arc<dyn ToolCatalog>>,
    version_counter: std::sync::atomic::AtomicU64,
}

impl CompositeCatalog {
    /// 创建组合目录。
    ///
    /// `sources` 按优先级从高到低排列。
    pub fn new(sources: Vec<std::sync::Arc<dyn ToolCatalog>>) -> Self {
        Self {
            sources,
            version_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for CompositeCatalog {
    async fn snapshot(&self) -> std::sync::Arc<ToolSnapshot> {
        let mut merged = indexmap::IndexMap::new();

        // 反向遍历（从低优先级到高优先级），高优先级自然覆盖低优先级
        for source in self.sources.iter().rev() {
            let snap = source.snapshot().await;
            let snap_tools = &snap.tools;
            for (name, tool) in snap_tools.iter() {
                if merged.contains_key(name) {
                    tracing::warn!(
                        tool_name = %name,
                        "Tool conflict detected in CompositeCatalog. Higher priority tool shadows the lower one."
                    );
                }
                merged.insert(name.clone(), tool.clone());
            }
        }

        let version = self
            .version_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        std::sync::Arc::new(ToolSnapshot::new(merged, version))
    }
}
