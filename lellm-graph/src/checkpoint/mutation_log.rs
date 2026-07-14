//! MutationLog — 持久化审计日志，独立于 Checkpoint。
//!
//! Checkpoint = Snapshot（快速恢复）
//! ExecutionTrace = 内存 WAL（session 调试）
//! MutationLog = 持久化审计（可选重放）
//!
//! # 四层数据模型
//!
//! ```text
//! Runtime (AgentState)     ← 工作集，Prompt Buffer
//!       ↓ commit_batch
//! Checkpoint (Snapshot)    ← 快速恢复，物化状态
//!       ↓ mutation_log.append()
//! MutationLog (审计)       ← 持久化，可选重放
//!       ↓ archive
//! Conversation Archive     ← 长期存储
//! ```

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::checkpoint_data::NodeId;
use super::checkpoint_data::{CheckpointId, CheckpointStoreError, TraceId};

// ─── MutationLogEntry ──────────────────────────────────────────

/// Mutation 日志条目 — 持久化审计记录。
///
/// 使用 `serde_json::Value` 存储 mutation 内容，
/// 避免在存储层引入对强类型 Mutation 的依赖。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationLogEntry {
    /// 执行追踪 ID
    pub trace_id: TraceId,
    /// 步骤序号（从 1 开始）
    pub step: usize,
    /// 节点标识
    pub node_id: NodeId,
    /// 关联的 Checkpoint（如果有）
    pub checkpoint_id: Option<CheckpointId>,
    /// 步骤内的 mutation 序号
    pub mutation_index: usize,
    /// 序列化后的 mutation 内容
    pub mutation: serde_json::Value,
    /// 记录时间
    pub timestamp: SystemTime,
}

impl MutationLogEntry {
    pub fn new(
        trace_id: TraceId,
        step: usize,
        node_id: NodeId,
        checkpoint_id: Option<CheckpointId>,
        mutation_index: usize,
        mutation: serde_json::Value,
    ) -> Self {
        Self {
            trace_id,
            step,
            node_id,
            checkpoint_id,
            mutation_index,
            mutation,
            timestamp: SystemTime::now(),
        }
    }
}

// ─── MutationLogStore SPI ──────────────────────────────────────

/// MutationLog 存储后端 SPI。
///
/// 独立于 CheckpointStore，允许不同的持久化策略。
#[async_trait]
pub trait MutationLogStore: Send + Sync {
    /// 追加一条 mutation 日志。
    async fn append(&self, entry: MutationLogEntry) -> Result<(), CheckpointStoreError>;

    /// 批量追加 mutation 日志。
    async fn append_batch(
        &self,
        entries: Vec<MutationLogEntry>,
    ) -> Result<(), CheckpointStoreError> {
        for entry in entries {
            self.append(entry).await?;
        }
        Ok(())
    }

    /// 重放 trace 从指定步骤开始的 mutation 日志。
    async fn replay(
        &self,
        trace_id: &TraceId,
        from_step: usize,
    ) -> Result<Vec<MutationLogEntry>, CheckpointStoreError>;

    /// 截断 trace 的旧日志，保留从指定步骤开始的。
    async fn truncate(
        &self,
        trace_id: &TraceId,
        keep_from_step: usize,
    ) -> Result<usize, CheckpointStoreError>;
}

// ─── InMemoryMutationLog ───────────────────────────────────────

/// 基于内存的 MutationLog 实现。
///
/// 适用于测试和开发环境。
///
/// 内部使用单个 RwLock 保护索引，确保原子性。
/// 索引直接存储条目（而非索引号），避免 truncate 后的索引失效问题。
#[derive(Default)]
pub struct InMemoryMutationLog {
    /// trace_id → [MutationLogEntry] 索引（按时间正序）
    index: RwLock<HashMap<TraceId, Vec<MutationLogEntry>>>,
}

impl InMemoryMutationLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 所有 trace 的条目总数。
    pub fn len(&self) -> usize {
        let index = self.index.read().unwrap();
        index.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl MutationLogStore for InMemoryMutationLog {
    async fn append(&self, entry: MutationLogEntry) -> Result<(), CheckpointStoreError> {
        let mut index = self
            .index
            .write()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        index.entry(entry.trace_id).or_default().push(entry);
        Ok(())
    }

    async fn replay(
        &self,
        trace_id: &TraceId,
        from_step: usize,
    ) -> Result<Vec<MutationLogEntry>, CheckpointStoreError> {
        let index = self
            .index
            .read()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        let entries = index.get(trace_id).cloned().unwrap_or_default();
        Ok(entries.into_iter().filter(|e| e.step >= from_step).collect())
    }

    async fn truncate(
        &self,
        trace_id: &TraceId,
        keep_from_step: usize,
    ) -> Result<usize, CheckpointStoreError> {
        let mut index = self
            .index
            .write()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        let entries = index.entry(*trace_id).or_default();
        let original_len = entries.len();
        entries.retain(|e| e.step >= keep_from_step);
        Ok(original_len - entries.len())
    }
}

// ─── MutationLogConverter ──────────────────────────────────────

/// Mutation 到 JSON 的转换器 — 供执行循环使用。
///
/// 将强类型 Mutation 批量转换为 MutationLogEntry。
pub fn mutations_to_log_entries<E: Serialize>(
    trace_id: TraceId,
    step: usize,
    node_id: NodeId,
    checkpoint_id: Option<CheckpointId>,
    mutations: impl IntoIterator<Item = E>,
) -> Vec<MutationLogEntry> {
    let mut result = Vec::new();
    for (idx, mutation) in mutations.into_iter().enumerate() {
        if let Ok(value) = serde_json::to_value(&mutation) {
            result.push(MutationLogEntry::new(
                trace_id,
                step,
                node_id.clone(),
                checkpoint_id.clone(),
                idx,
                value,
            ));
        }
    }
    result
}
