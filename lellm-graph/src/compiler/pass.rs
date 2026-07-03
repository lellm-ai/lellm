//! CompilerPass trait — 优化 pass 的统一接口。

use super::context::CompilerContext;
use crate::Graph;
use crate::MergeStrategy;
use crate::state::workflow_state::WorkflowState;

/// 编译器优化 pass trait。
///
/// 每个 pass 负责一个特定的优化，
/// 例如：内联、死代码消除、Barrier 合并等。
///
/// # 示例
///
/// ```ignore
/// struct MyPass;
///
/// impl<S: WorkflowState, M: MergeStrategy<S>> CompilerPass<S, M> for MyPass {
///     fn name(&self) -> &str {
///         "my_pass"
///     }
///
///     fn run(&self, graph: &mut Graph<S, M>, ctx: &mut CompilerContext<S>) -> bool {
///         // 优化逻辑
///         false
///     }
/// }
/// ```
pub trait CompilerPass<S: WorkflowState, M: MergeStrategy<S>>: Send + Sync {
    /// pass 的名称。
    fn name(&self) -> &str;

    /// 执行优化 pass。
    ///
    /// # 参数
    ///
    /// - `graph` — 要优化的图（可变引用）
    /// - `ctx` — 编译上下文，包含配置和统计信息
    ///
    /// # 返回
    ///
    /// 如果 pass 修改了图，返回 `true`；
    /// 否则返回 `false`。
    fn run(&self, graph: &mut Graph<S, M>, ctx: &mut CompilerContext<S>) -> bool;
}
