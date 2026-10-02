//! Checkpoint 保留策略。
//!
//! 第一阶段固定保存策略：每个成功提交的节点都同步等待检查点保存，
//! 完成态也保存 — 保存路径从不读取触发策略，故不暴露 Trigger 配置。
//!
//! 可配置的只有保留策略（Retention）：
//!
//! ```text
//! CheckpointConfig
//!   ├── RetentionPolicy:  保留多少个 Checkpoint（KeepAll / KeepLatest(n)）
//!   └── Store:            存储后端（BlobCheckpointStore）
//! ```

use super::checkpoint_data::{Checkpoint, CheckpointStoreError, TraceId};

/// Checkpoint 保存回调类型别名。
pub type CheckpointSaveFn<S> = Box<
    dyn Fn(
            Checkpoint<S>,
            TraceId,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), CheckpointStoreError>> + Send>,
        > + Send
        + Sync,
>;

// ─── RetentionPolicy ───────────────────────────────────────────

/// Checkpoint 保留策略 — 决定保留多少个。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// 保留所有 Checkpoint（默认）
    #[default]
    KeepAll,
    /// 仅保留最新的 N 个
    KeepLatest(usize),
}

impl RetentionPolicy {
    /// 根据策略计算需要保留的数量。
    ///
    /// - `KeepAll` → `None`（不修剪）
    /// - `KeepLatest(n)` → `Some(n)`
    pub fn prune_keep(&self) -> Option<usize> {
        match self {
            RetentionPolicy::KeepAll => None,
            RetentionPolicy::KeepLatest(n) => Some(*n),
        }
    }
}
