//! Compiler — 图优化 pass 框架。
//!
//! 提供 CompilerPass trait 和优化上下文，用于在编译阶段对图进行优化。
//!
//! # 设计理念
//!
//! ```text
//! 用户 API：
//!   builder.build()      → Graph（不触发优化）
//!   builder.compile()    → Graph（触发优化）
//!
//! 编译器内部流程：
//!   1. 分析 SubgraphNode
//!   2. 评估是否值得内联
//!   3. 如果值得：展开 Subgraph，合并到外层 Graph
//!   4. 如果不值得：保持 Subgraph，运行时递归执行
//! ```

pub mod context;
pub mod inline_pass;
pub mod pass;

pub use context::CompilerContext;
pub use inline_pass::InlinePass;
pub use pass::CompilerPass;
