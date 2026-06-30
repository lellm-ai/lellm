//! CompilerContext — 编译上下文。

use crate::workflow_state::WorkflowState;

/// 编译上下文 — 包含配置和统计信息。
pub struct CompilerContext<S: WorkflowState> {
    /// 最大内联图大小（节点数）
    pub max_inline_size: usize,

    /// 是否启用调试输出
    pub debug: bool,

    /// 统计信息
    pub stats: CompilerStats,

    /// PhantomData
    _phantom: std::marker::PhantomData<S>,
}

impl<S: WorkflowState> CompilerContext<S> {
    /// 创建新的编译上下文。
    pub fn new() -> Self {
        Self {
            max_inline_size: 100, // 默认最大内联图大小
            debug: false,
            stats: CompilerStats::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// 设置最大内联图大小。
    pub fn max_inline_size(mut self, max: usize) -> Self {
        self.max_inline_size = max;
        self
    }

    /// 启用调试输出。
    pub fn debug(mut self, enable: bool) -> Self {
        self.debug = enable;
        self
    }
}

impl<S: WorkflowState> Default for CompilerContext<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// 编译统计信息。
#[derive(Debug, Default, Clone)]
pub struct CompilerStats {
    /// Subgraph 总数
    pub subgraph_count: usize,

    /// 已内联的 Subgraph 数量
    pub inlined_count: usize,

    /// 未内联的 Subgraph 数量
    pub not_inlined_count: usize,

    /// 总节点数（优化前）
    pub total_nodes_before: usize,

    /// 总节点数（优化后）
    pub total_nodes_after: usize,
}
