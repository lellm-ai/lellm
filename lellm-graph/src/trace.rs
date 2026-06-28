//! ExecutionTrace + TraceSink — 审计日志，与 Checkpoint 分离。
//!
//! Checkpoint = Snapshot（恢复）
//! ExecutionTrace = WAL（审计）
//!
//! Runtime 层：强类型 `ExecutionTrace<E>`，`E = S::Mutation`
//! 导出层：`ExportedTrace`，JSON 序列化

use serde::{Deserialize, Serialize};

use crate::checkpoint::NodeId;

// ─── TraceStep ─────────────────────────────────────────────────

/// 执行步骤记录 — 单个节点的 Mutation 审计。
#[derive(Debug, Clone)]
pub struct TraceStep<E> {
    /// 步骤序号（从 1 开始）
    pub step: usize,
    /// 节点标识
    pub node_id: NodeId,
    /// 该节点产生的 Effects
    pub mutations: Vec<E>,
}

// ─── ExecutionTrace ────────────────────────────────────────────

/// 执行追踪 — 强类型 Mutation 审计日志。
///
/// `E = S::Mutation`，Runtime 层保持编译期类型安全。
#[derive(Debug, Clone, Default)]
pub struct ExecutionTrace<E> {
    pub steps: Vec<TraceStep<E>>,
}

impl<E> ExecutionTrace<E> {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn push(&mut self, step: TraceStep<E>) {
        self.steps.push(step);
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ─── TraceSink ─────────────────────────────────────────────────

/// 审计日志接收器 — Executor 通过 TraceSink 记录每一步。
///
/// 默认实现：`MemoryTraceSink<E>`（内存收集）
/// 未来扩展：`SqliteTraceSink`、`OpenTelemetryTraceSink`、`ParquetTraceSink`
pub trait TraceSink<E>: Send + Sync {
    /// 记录一个执行步骤。
    fn record_step(&mut self, step: TraceStep<E>);
}

/// 内存 TraceSink — v0.4 默认实现。
#[derive(Debug)]
pub struct MemoryTraceSink<E: Send + Sync> {
    pub trace: ExecutionTrace<E>,
}

impl<E: Send + Sync> Default for MemoryTraceSink<E> {
    fn default() -> Self {
        Self {
            trace: ExecutionTrace::new(),
        }
    }
}

impl<E: Send + Sync> MemoryTraceSink<E> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_trace(self) -> ExecutionTrace<E> {
        self.trace
    }
}

impl<E: Send + Sync> TraceSink<E> for MemoryTraceSink<E> {
    fn record_step(&mut self, step: TraceStep<E>) {
        self.trace.push(step);
    }
}

// ─── ExportedTrace ─────────────────────────────────────────────

/// 导出的追踪记录 — 统一 JSON 序列化，供外部消费。
///
/// 通过 `ExecutionTrace::export()` 生成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedTrace {
    pub steps: Vec<ExportedTraceStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedTraceStep {
    pub step: usize,
    pub node_id: String,
    pub mutations: Vec<serde_json::Value>,
}

impl<E: Serialize> ExecutionTrace<E> {
    /// 导出为 JSON 可序列化的追踪记录。
    pub fn export(&self) -> ExportedTrace {
        ExportedTrace {
            steps: self
                .steps
                .iter()
                .map(|s| ExportedTraceStep {
                    step: s.step,
                    node_id: s.node_id.0.clone(),
                    mutations: s
                        .mutations
                        .iter()
                        .filter_map(|e| serde_json::to_value(e).ok())
                        .collect(),
                })
                .collect(),
        }
    }
}
