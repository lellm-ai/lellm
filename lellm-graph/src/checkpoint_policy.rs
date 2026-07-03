//! Checkpoint 策略 — Trigger / Retention 分层。
//!
//! v0.5 重构：将原来的 `CheckpointPolicy` enum 拆分为两层正交策略：
//!
//! ```text
//! CheckpointConfig
//!   ├── TriggerPolicy:    何时保存 Checkpoint
//!   ├── RetentionPolicy:  保留多少个 Checkpoint
//!   └── Store:            存储后端（BlobCheckpointStore）
//! ```
//!
//! # 设计原则
//!
//! - **正交性**：Trigger 与 Retention 独立组合，互不干扰
//! - **渐进式**：默认值与 v0.4 行为一致（EveryNode + KeepAll）
//! - **可扩展**：未来可添加 OnMutation、TimeBased 等策略

use std::time::Duration;

use crate::checkpoint::{Checkpoint, CheckpointStoreError, TraceId};

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

// ─── TriggerPolicy ─────────────────────────────────────────────

/// Checkpoint 触发策略 — 决定何时保存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TriggerPolicy {
    /// 每次节点执行后保存（默认，与 v0.4 CheckpointPolicy::EveryNode 一致）
    #[default]
    EveryNode,
    /// 仅在 Barrier 决策后保存
    BarrierOnly,
    /// 手动控制 — 调用方显式触发
    Manual,
    /// 有新 Mutation 时才保存（无 Mutation 的节点跳过）
    OnMutation,
}

// ─── RetentionPolicy ───────────────────────────────────────────

/// Checkpoint 保留策略 — 决定保留多少个。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// 保留所有 Checkpoint（默认，与 v0.4 行为一致）
    #[default]
    KeepAll,
    /// 仅保留最新的 N 个
    KeepLatest(usize),
    /// 保留指定时间范围内的 Checkpoint
    TimeBased(Duration),
}

impl RetentionPolicy {
    /// 根据策略计算需要保留的数量。
    ///
    /// - `KeepAll` → `None`（不修剪）
    /// - `KeepLatest(n)` → `Some(n)`
    /// - `TimeBased` → `None`（需要存储层按时间判断，暂不支援自动修剪）
    pub fn prune_keep(&self) -> Option<usize> {
        match self {
            RetentionPolicy::KeepAll => None,
            RetentionPolicy::KeepLatest(n) => Some(*n),
            RetentionPolicy::TimeBased(_) => None, // 需要存储层支持
        }
    }
}

// ─── 向后兼容 ──────────────────────────────────────────────────

/// v0.4 的 CheckpointPolicy — 已弃用，请使用 TriggerPolicy。
#[allow(deprecated)]
#[deprecated(
    since = "0.5.0",
    note = "Use TriggerPolicy instead. EveryNode → TriggerPolicy::EveryNode, \
            BarrierOnly → TriggerPolicy::BarrierOnly, Manual → TriggerPolicy::Manual"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckpointPolicy {
    #[default]
    EveryNode,
    BarrierOnly,
    Manual,
}

#[allow(deprecated)]
impl From<CheckpointPolicy> for TriggerPolicy {
    fn from(policy: CheckpointPolicy) -> Self {
        match policy {
            CheckpointPolicy::EveryNode => TriggerPolicy::EveryNode,
            CheckpointPolicy::BarrierOnly => TriggerPolicy::BarrierOnly,
            CheckpointPolicy::Manual => TriggerPolicy::Manual,
        }
    }
}
