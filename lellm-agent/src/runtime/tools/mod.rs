//! 工具系统 — 执行、目录抽象。
//!
//! 独立的工具子系统，被 runtime 层使用。
//!
//! **分层：**
//! - 协议层（lellm-core）：`ToolDefinition`, `ParallelSafety`, `ToolCategory`
//! - 可执行描述（lellm-core）：`ExecutableTool`, `ToolFn`
//! - 构造框架（lellm-core/tool feature）：`ToolArgs`, schema 生成
//! - 运行时层（本模块）：`ToolExecutor`, `ToolCatalog`, `ToolSnapshot`
//! - MCP 集成（可选）：`McpCatalog`, `McpServerRegistry` 等

mod executor;

#[cfg(feature = "mcp")]
pub mod mcp;

// Re-export protocol types from lellm-core
pub use lellm_core::{ExecutableTool, ParallelSafety, ToolCategory, ToolFn};

// Re-export tool construction from lellm-core
pub use lellm_core::ToolArgs;

// Re-export runtime types
#[allow(deprecated)]
pub use executor::execute_batch_with;
pub use executor::{BatchExecutionResult, ToolExecutor};

// ─── 工具快照 ────────────────────────────────────────────────────

/// 工具快照 — 冻结视图（Frozen View）。
///
/// 一旦创建，快照内容不再变化。
/// `definitions` 通过 `OnceLock` 懒构建——大部分轮次不需要定义列表。
/// `diagnostics` 记录本次合并产生的诊断信息（如工具冲突），与快照绑定。
///
/// Clone 很便宜——内部 `tools` 是 Arc 浅拷贝，`definitions` OnceLock 重建即可。
pub struct ToolSnapshot {
    tools: std::sync::Arc<indexmap::IndexMap<String, ExecutableTool>>,
    definitions: std::sync::OnceLock<Vec<lellm_core::ToolDefinition>>,
    diagnostics: Vec<CatalogDiagnostic>,
}

impl ToolSnapshot {
    /// 从工具映射构建快照（无诊断信息）。
    pub fn new(tools: indexmap::IndexMap<String, ExecutableTool>) -> Self {
        Self {
            tools: std::sync::Arc::new(tools),
            definitions: std::sync::OnceLock::new(),
            diagnostics: Vec::new(),
        }
    }

    /// 按名称查找工具。
    pub fn get(&self, name: &str) -> Option<&ExecutableTool> {
        self.tools.get(name)
    }

    /// 获取所有工具定义（懒构建）。
    pub fn definitions(&self) -> &[lellm_core::ToolDefinition] {
        self.definitions
            .get_or_init(|| self.tools.values().map(|t| t.definition.clone()).collect())
    }

    /// 获取本次快照的诊断信息（如工具冲突）。
    ///
    /// 诊断信息与快照绑定，不会被后续 `snapshot()` 调用覆盖。
    pub fn diagnostics(&self) -> &[CatalogDiagnostic] {
        &self.diagnostics
    }

    /// 是否有工具。
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 设置诊断信息（仅供 `CompositeCatalog` 使用）。
    pub(crate) fn with_diagnostics(mut self, diagnostics: Vec<CatalogDiagnostic>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// 迭代所有工具条目。
    ///
    /// Registry 合并快照时使用——直接遍历 `(name, ExecutableTool)`，
    /// 无需 `definitions() → get(name)` 的绕圈。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ExecutableTool)> {
        self.tools.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Clone 很便宜——`tools` 是 Arc 浅拷贝，`definitions` OnceLock 重建后懒计算。
impl Clone for ToolSnapshot {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
            definitions: std::sync::OnceLock::new(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

// ─── 工具目录抽象 ────────────────────────────────────────────────

/// 工具目录 — 静态或动态的工具集合。
///
/// **设计目标：**
/// - 让 `ToolExecutor` 不关心工具来源（静态注册 vs MCP 发现）
/// - 每轮迭代调用 `snapshot()` 一次，固定本轮工具集（避免同轮不一致）
/// - `ExecutableTool` 必须 `Clone + Send + Sync`（快照在内存中传递）
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
    pub fn from_tools(tools: Vec<ExecutableTool>) -> Self {
        let mut map = indexmap::IndexMap::with_capacity(tools.len());
        for reg in tools {
            map.insert(reg.definition.name.clone(), reg);
        }
        Self {
            snapshot: std::sync::Arc::new(ToolSnapshot::new(map)),
        }
    }

    /// 空目录。
    pub fn empty() -> Self {
        Self {
            snapshot: std::sync::Arc::new(ToolSnapshot::new(indexmap::IndexMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for StaticCatalog {
    async fn snapshot(&self) -> std::sync::Arc<ToolSnapshot> {
        self.snapshot.clone()
    }
}

/// 目录诊断信息 — 快照合并时产生的元数据。
///
/// 与 `ToolSnapshot` 绑定，不会被后续 `snapshot()` 调用覆盖。
#[derive(Debug, Clone)]
pub enum CatalogDiagnostic {
    /// 工具名称冲突 — 高优先级源遮蔽了低优先级源的同名工具。
    Conflict {
        /// 冲突的工具名称
        tool_name: String,
        /// 获胜的 catalog 名称（优先级高）
        winner: String,
        /// 被覆盖的 catalog 名称（优先级低）
        loser: String,
        /// 使用的冲突策略
        policy: ConflictPolicy,
    },
}

/// 冲突解决策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// 默认：前面优先级高，同名工具被遮蔽
    #[default]
    Shadow,
    /// 严格模式：冲突即报错
    Error,
}

/// 组合目录构建器
///
/// 支持两种使用风格：
/// - **链式构建**（`with()`）：固定来源，一行写尽
/// - **动态装配**（`add()`）：条件添加来源，运行时组装
#[derive(Default)]
pub struct CompositeCatalogBuilder {
    sources: Vec<(String, std::sync::Arc<dyn ToolCatalog>)>,
    conflict_policy: ConflictPolicy,
}

impl CompositeCatalogBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置冲突策略（consuming，用于链式调用）
    pub fn conflict_policy(mut self, policy: ConflictPolicy) -> Self {
        self.conflict_policy = policy;
        self
    }

    /// 添加工具源（可变引用，用于动态装配）。
    ///
    /// # Example
    /// ```ignore
    /// let mut builder = CompositeCatalog::builder();
    /// if let Some(mcp) = mcp_catalog {
    ///     builder.add("mcp", mcp);
    /// }
    /// builder.add("builtin", builtin_catalog);
    /// let catalog = builder.build();
    /// ```
    pub fn add(
        &mut self,
        name: impl Into<String>,
        catalog: std::sync::Arc<dyn ToolCatalog>,
    ) -> &mut Self {
        self.sources.push((name.into(), catalog));
        self
    }

    /// 添加工具源（consuming，用于链式调用）。
    ///
    /// # Example
    /// ```ignore
    /// let catalog = CompositeCatalog::builder()
    ///     .with("mcp", mcp_catalog)
    ///     .with("builtin", builtin_catalog)
    ///     .build();
    /// ```
    pub fn with(
        mut self,
        name: impl Into<String>,
        catalog: std::sync::Arc<dyn ToolCatalog>,
    ) -> Self {
        self.sources.push((name.into(), catalog));
        self
    }

    /// 构建组合目录
    pub fn build(self) -> CompositeCatalog {
        CompositeCatalog {
            sources: self.sources,
            conflict_policy: self.conflict_policy,
        }
    }
}

/// 组合目录 — 按优先级合并多个工具源。
///
/// **遮蔽策略（Shadowing）：** 靠前的源优先级高，同名工具被遮蔽。
/// 遮蔽发生时通过 `tracing::warn!` 记录结构化日志，
/// 并作为 `CatalogDiagnostic::Conflict` 嵌入返回的 `ToolSnapshot`。
pub struct CompositeCatalog {
    sources: Vec<(String, std::sync::Arc<dyn ToolCatalog>)>,
    conflict_policy: ConflictPolicy,
}

impl CompositeCatalog {
    /// 创建组合目录（Builder 模式）。
    pub fn builder() -> CompositeCatalogBuilder {
        CompositeCatalogBuilder::new()
    }

    /// 创建组合目录（简单模式，默认 Shadow 策略）。
    ///
    /// `sources` 按优先级从高到低排列。源名称自动生成为 `source_0`, `source_1` 等。
    pub fn new(sources: Vec<(String, std::sync::Arc<dyn ToolCatalog>)>) -> Self {
        Self {
            sources,
            conflict_policy: ConflictPolicy::default(),
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for CompositeCatalog {
    async fn snapshot(&self) -> std::sync::Arc<ToolSnapshot> {
        // merged 追踪工具来源，以便正确记录冲突信息
        let mut merged: indexmap::IndexMap<String, (ExecutableTool, String)> =
            indexmap::IndexMap::new();
        let mut diagnostics = Vec::new();

        // 反向遍历（从低优先级到高优先级），高优先级自然覆盖低优先级
        for source in self.sources.iter().rev() {
            let source_name = &source.0;
            let snap = source.1.snapshot().await;
            for (name, tool) in snap.iter() {
                if let Some((_, existing_source)) = merged.get(name) {
                    let diagnostic = CatalogDiagnostic::Conflict {
                        tool_name: name.to_string(),
                        winner: source_name.clone(),
                        loser: existing_source.clone(),
                        policy: self.conflict_policy,
                    };
                    tracing::warn!(
                        tool_name = %name,
                        winner = %source_name,
                        loser = %existing_source,
                        "Tool conflict detected in CompositeCatalog. Higher priority tool shadows the lower one."
                    );
                    diagnostics.push(diagnostic);
                }
                merged.insert(name.to_string(), (tool.clone(), source_name.clone()));
            }
        }

        // 提取纯工具映射（丢弃来源追踪信息）
        let tools: indexmap::IndexMap<String, ExecutableTool> = merged
            .into_iter()
            .map(|(name, (tool, _source))| (name, tool))
            .collect();

        let snapshot = ToolSnapshot::new(tools).with_diagnostics(diagnostics);

        std::sync::Arc::new(snapshot)
    }
}
