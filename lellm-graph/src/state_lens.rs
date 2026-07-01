//! StateLens — 状态投影，不是状态转换。
//!
//! 用于 Subgraph 组合时，从外层 State 投影出内层 State。
//! 零拷贝，只有借用。
//!
//! # 设计理念
//!
//! ```text
//! StateAdapter (❌)          StateLens (✅)
//! ─────────────────         ─────────────────
//! extract(outer) → inner    get(outer) → &mut inner
//! merge(inner, outer)       (不需要，借用自动释放)
//!
//! 需要 clone + merge        零拷贝，只有借用
//! 两个闭包容易不一致        一个方法，简单清晰
//! ```
//!
//! # 示例
//!
//! ```ignore
//! use lellm_graph::{StateLens, SubgraphSpec, GraphBuilder, NodeKind};
//!
//! // 定义 State
//! struct WorkflowState {
//!     agent: AgentState,
//! }
//!
//! // 定义 Lens
//! struct AgentLens;
//!
//! impl StateLens<WorkflowState, AgentState> for AgentLens {
//!     fn get<'a>(&self, outer: &'a mut WorkflowState) -> &'a mut AgentState {
//!         &mut outer.agent
//!     }
//! }
//!
//! // 使用 SubgraphSpec
//! let agent_graph = AgentBuilder::new(model).tools([...]).build();
//!
//! let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
//! builder.node(
//!     "agent",
//!     NodeKind::Subgraph(SubgraphSpec::new(agent_graph, AgentLens).compile()),
//! );
//! // 或使用语法糖
//! // builder.subgraph("agent", SubgraphSpec::new(agent_graph, AgentLens));
//! ```

use std::marker::PhantomData;

/// 状态投影 trait — 从外层 State 投影出内层 State。
///
/// 用于 Subgraph 组合，实现零拷贝状态访问。
///
/// # 设计原则
///
/// - **投影（Projection）**，不是转换（Conversion）
/// - **零拷贝**，只有借用
/// - **Agent 不知道 WorkflowState 存在**
///
/// # 生命周期
///
/// `get()` 返回的引用生命周期与外层 State 相同，
/// 退出 Subgraph 后借用自动释放。
pub trait StateLens<Outer, Inner>: Send + Sync {
    /// 从外层 State 投影出内层 State 的可变引用。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// fn get<'a>(&self, outer: &'a mut WorkflowState) -> &'a mut AgentState {
    ///     &mut outer.agent
    /// }
    /// ```
    fn get<'a>(&self, outer: &'a mut Outer) -> &'a mut Inner;
}

/// 不做任何投影 — 直接使用同一个 State。
///
/// 用于 Subgraph 和外层使用相同 State 类型的场景。
pub struct IdentityLens<S> {
    _phantom: PhantomData<S>,
}

impl<S> IdentityLens<S> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<S> Default for IdentityLens<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Send + Sync> StateLens<S, S> for IdentityLens<S> {
    fn get<'a>(&self, outer: &'a mut S) -> &'a mut S {
        outer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct OuterState {
        inner: InnerState,
        other: String,
    }

    #[derive(Debug, PartialEq)]
    struct InnerState {
        value: i32,
    }

    struct TestLens;

    impl StateLens<OuterState, InnerState> for TestLens {
        fn get<'a>(&self, outer: &'a mut OuterState) -> &'a mut InnerState {
            &mut outer.inner
        }
    }

    #[test]
    fn test_state_lens_projection() {
        let mut outer = OuterState {
            inner: InnerState { value: 42 },
            other: "test".to_string(),
        };

        let lens = TestLens;
        let inner = lens.get(&mut outer);

        // 修改 inner
        inner.value = 100;

        // 验证 outer.inner 被修改
        assert_eq!(outer.inner.value, 100);
        assert_eq!(outer.other, "test");
    }

    #[test]
    fn test_identity_lens() {
        let mut state = InnerState { value: 42 };

        let lens = IdentityLens::new();
        let inner = lens.get(&mut state);

        inner.value = 100;

        assert_eq!(state.value, 100);
    }
}
