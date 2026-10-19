use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const RUN_FORMAT_VERSION: u32 = 2;

/// 展示事件；恢复与审批以存储为准，不能依赖回调收到每个事件。
#[derive(Debug, Clone)]
pub struct RunEvent {
    pub revision: u64,
    pub frame: String,
    pub node: String,
    pub phase: &'static str,
}

pub type RunObserver = std::sync::Arc<dyn Fn(RunEvent) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum DurableError {
    #[error("run is busy")]
    Busy,
    #[error("run not found")]
    NotFound,
    #[error("run already exists")]
    AlreadyExists,
    #[error("unsupported run format")]
    UnsupportedFormat,
    #[error("incompatible workflow: {0}")]
    Incompatible(String),
    #[error("conflicting request: {0}")]
    Conflict(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("run is suspended")]
    Suspended,
    #[error("run is stopped")]
    Stopped,
    #[error("parallel branch cancelled")]
    BranchCancelled,
    #[error("storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid run record: {0}")]
    Codec(#[from] serde_json::Error),
}

impl From<DurableError> for crate::GraphError {
    fn from(value: DurableError) -> Self {
        Self::Terminal(crate::TerminalError::Durable(value))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Suspended,
    Completed,
    Rejected,
    Cancelled,
    Expired,
}

#[derive(Debug)]
pub enum RunOutcome<S> {
    Completed(S),
    Suspended(RunRecord),
    Stopped(RunStatus),
}

/// 业务版本必须显式声明；修改节点/工具行为时由调用者升级。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunVersion {
    pub workflow: String,
    pub state_schema: String,
}

impl RunVersion {
    pub fn new(workflow: impl Into<String>, state_schema: impl Into<String>) -> Self {
        Self {
            workflow: workflow.into(),
            state_schema: state_schema.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub format_version: u32,
    pub run_id: String,
    pub revision: u64,
    pub version: RunVersion,
    pub status: RunStatus,
    pub steps_used: usize,
    pub max_steps: usize,
    pub frames: BTreeMap<String, FrameRecord>,
    pub approvals: BTreeMap<String, ApprovalRequest>,
    pub tasks: BTreeMap<String, super::TaskRecord>,
}

impl RunRecord {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DurableError> {
        let value: Value = serde_json::from_slice(bytes)?;
        if value.get("format_version").and_then(Value::as_u64)
            != Some(u64::from(RUN_FORMAT_VERSION))
        {
            return Err(DurableError::UnsupportedFormat);
        }
        // Option 字段也必须出现，不能将缺失的游标解释为完成。
        for frame in value
            .get("frames")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
        {
            if frame.get("next_node").is_none() {
                return Err(DurableError::UnsupportedFormat);
            }
        }
        for approval in value
            .get("approvals")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
        {
            if approval.get("receipt").is_none() || approval.get("deadline_unix_ms").is_none() {
                return Err(DurableError::UnsupportedFormat);
            }
        }
        let record: Self = serde_json::from_value(value)?;
        record.validate()?;
        Ok(record)
    }
    pub(crate) fn validate(&self) -> Result<(), DurableError> {
        let invalid = || DurableError::Incompatible("invalid run invariants".into());
        if self.format_version != RUN_FORMAT_VERSION {
            return Err(DurableError::UnsupportedFormat);
        }
        let root = self.frames.get("root").ok_or_else(invalid)?;
        if self.version.workflow.is_empty()
            || self.version.state_schema.is_empty()
            || self.steps_used > self.max_steps
            || (self.status == RunStatus::Completed && root.next_node.is_some())
            || self
                .frames
                .values()
                .any(|f| f.steps_used > f.max_steps || (f.active && f.next_node.is_none()))
            || self.approvals.iter().any(|(id, a)| {
                id != &a.id
                    || !self.frames.contains_key(&a.frame)
                    || (a.applied && a.receipt.is_none())
                    || a.receipt
                        .as_ref()
                        .is_some_and(|r| r.accepted_revision > self.revision)
            })
            || self.tasks.iter().any(|(id, t)| {
                id != &t.id || (t.status == super::TaskStatus::Completed && t.result.is_none())
            })
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRecord {
    pub graph_signature: String,
    pub state: Value,
    pub next_node: Option<String>,
    pub steps_used: usize,
    pub max_steps: usize,
    /// 已持久化开始记录；恢复同一次调用不重复扣减步数。
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Reject { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionReceipt {
    pub request_id: String,
    pub decision: ApprovalDecision,
    pub accepted_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub frame: String,
    pub node: String,
    /// 审批绑定持久化的状态快照，token 随请求生成后保持不变。
    pub content: Value,
    pub content_token: String,
    pub deadline_unix_ms: Option<u64>,
    pub receipt: Option<DecisionReceipt>,
    pub applied: bool,
}
