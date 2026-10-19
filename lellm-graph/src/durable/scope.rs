use super::*;
use crate::{Graph, GraphError, MergeStrategy, WorkflowState};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

tokio::task_local! {
    static FRAME: FrameScope;
    static NODE: NodeScope;
}

pub(crate) struct Session {
    pub record: RunRecord,
    pub lease: Box<dyn RunLease>,
    poisoned: bool,
    pub in_flight: std::collections::HashSet<String>,
}

#[derive(Clone)]
pub(crate) struct FrameScope {
    pub session: Arc<Mutex<Session>>,
    pub path: String,
    pub root_cancel: crate::CancellationToken,
    pub observer: Option<RunObserver>,
}

#[derive(Clone)]
pub(crate) struct NodeScope {
    pub frame: FrameScope,
    pub node: String,
    pub invocation: usize,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// 新运行协议的指纹保留有语义的边顺序，独立于旧 checkpoint 的 hash 算法。
pub(crate) fn signature<S: WorkflowState, M: MergeStrategy<S>>(graph: &Graph<S, M>) -> String {
    let nodes: Vec<_> = graph
        .node_map()
        .iter()
        .map(|(name, kind)| {
            let nested = match kind {
                crate::NodeKind::Subgraph(spec) => spec.projector.durable_signature(),
                crate::NodeKind::Parallel(parallel) => Some(parallel.durable_signature()),
                #[allow(deprecated)]
                crate::NodeKind::External(node) => node.durable_signature(),
                _ => None,
            };
            (
                name,
                match kind {
                    crate::NodeKind::Task(_) => "task",
                    crate::NodeKind::Condition(_) => "condition",
                    crate::NodeKind::Barrier(_) => "barrier",
                    crate::NodeKind::Parallel(_) => "parallel",
                    crate::NodeKind::Subgraph(_) => "subgraph",
                    #[allow(deprecated)]
                    crate::NodeKind::External(_) => "external",
                    crate::NodeKind::ExternalLeaf(_) => "leaf",
                },
                nested,
            )
        })
        .collect();
    let edges: Vec<_> = graph
        .edges
        .iter()
        .map(|e| (&e.from, &e.to, e.condition.is_some(), e.fallback))
        .collect();
    serde_json::to_string(&(
        graph.canonical_hash(),
        graph.start_node(),
        graph.end_node(),
        nodes,
        edges,
    ))
    .expect("graph metadata")
}

impl FrameScope {
    pub fn new(record: RunRecord, lease: Box<dyn RunLease>) -> Self {
        Self {
            session: Arc::new(Mutex::new(Session {
                record,
                lease,
                poisoned: false,
                in_flight: Default::default(),
            })),
            path: "root".into(),
            root_cancel: crate::CancellationToken::new(),
            observer: None,
        }
    }
    pub fn emit(&self, node: &str, phase: &'static str) {
        if let Some(observer) = &self.observer {
            let event = RunEvent {
                revision: self.session.lock().record.revision,
                frame: self.path.clone(),
                node: node.into(),
                phase,
            };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(event))).is_err() {
                tracing::warn!("durable observer panicked");
            }
        }
    }
    pub fn current() -> Option<Self> {
        FRAME.try_with(Clone::clone).ok()
    }
    pub async fn within<F: Future>(&self, future: F) -> F::Output {
        FRAME.scope(self.clone(), future).await
    }
    pub fn record(&self) -> RunRecord {
        self.session.lock().record.clone()
    }
    pub fn update<T>(
        &self,
        edit: impl FnOnce(&mut RunRecord) -> Result<T, DurableError>,
    ) -> Result<T, DurableError> {
        let mut session = self.session.lock();
        if session.poisoned {
            return Err(DurableError::Conflict(
                "previous save failed; reopen run".into(),
            ));
        }
        let mut record = session.record.clone();
        let result = edit(&mut record)?;
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| DurableError::Conflict("revision overflow".into()))?;
        if let Err(error) = session.lease.save(&record) {
            session.poisoned = true;
            return Err(error);
        }
        session.record = record;
        Ok(result)
    }
    pub fn enter<S: WorkflowState, M: MergeStrategy<S>>(
        &self,
        graph: &Graph<S, M>,
        state: &mut S,
        max_steps: usize,
    ) -> Result<FrameRecord, GraphError> {
        let expected = signature(graph);
        let existing = self.record().frames.get(&self.path).cloned();
        let frame = match existing {
            Some(frame) => frame,
            None => {
                let frame = FrameRecord {
                    graph_signature: expected.clone(),
                    state: serde_json::to_value(state.snapshot()).map_err(DurableError::from)?,
                    next_node: Some(graph.start_node().into()),
                    steps_used: 0,
                    max_steps,
                    active: false,
                };
                self.update(|r| {
                    r.frames.insert(self.path.clone(), frame.clone());
                    Ok(())
                })?;
                frame
            }
        };
        if frame.graph_signature != expected || frame.max_steps != max_steps {
            return Err(DurableError::Incompatible(format!(
                "graph or budget changed at {}",
                self.path
            ))
            .into());
        }
        if frame
            .next_node
            .as_ref()
            .is_some_and(|n| !graph.node_map().contains_key(n))
        {
            return Err(DurableError::Incompatible("cursor node missing".into()).into());
        }
        *state =
            S::restore(serde_json::from_value(frame.state.clone()).map_err(DurableError::from)?);
        Ok(frame)
    }
    pub fn begin(&self, node: &str) -> Result<NodeScope, GraphError> {
        let invocation = self.update(|r| {
            if matches!(
                r.status,
                RunStatus::Rejected | RunStatus::Expired | RunStatus::Cancelled
            ) {
                return Err(DurableError::Stopped);
            }
            let f = r.frames.get_mut(&self.path).ok_or(DurableError::NotFound)?;
            if f.next_node.as_deref() != Some(node) {
                return Err(DurableError::Conflict("cursor mismatch".into()));
            }
            if !f.active {
                if f.steps_used >= f.max_steps || r.steps_used >= r.max_steps {
                    return Err(DurableError::Conflict("step budget exhausted".into()));
                }
                f.steps_used += 1;
                r.steps_used += 1;
                f.active = true;
            }
            Ok(f.steps_used)
        })?;
        self.emit(node, "started");
        Ok(NodeScope {
            frame: self.clone(),
            node: node.into(),
            invocation,
        })
    }
    pub fn commit<S: WorkflowState>(
        &self,
        state: &S,
        next: Option<String>,
    ) -> Result<(), GraphError> {
        let node = self.record().frames[&self.path]
            .next_node
            .clone()
            .unwrap_or_default();
        let snapshot = serde_json::to_value(state.snapshot()).map_err(DurableError::from)?;
        self.update(|r| {
            let f = r.frames.get_mut(&self.path).ok_or(DurableError::NotFound)?;
            let approval_id = f.next_node.as_ref().map(|node| {
                format!(
                    "{}:{}",
                    self.path,
                    serde_json::to_string(&(node, f.steps_used)).unwrap()
                )
            });
            f.state = snapshot;
            f.next_node = next;
            f.active = false;
            if let Some(request) = approval_id.and_then(|id| r.approvals.get_mut(&id)) {
                request.applied = true;
            }
            Ok(())
        })?;
        self.emit(&node, "committed");
        Ok(())
    }
    pub fn cancel(&self) -> Result<(), GraphError> {
        if !self.root_cancel.is_cancelled() {
            return Err(DurableError::BranchCancelled.into());
        }
        self.update(|r| {
            r.status = RunStatus::Cancelled;
            Ok(())
        })?;
        Err(DurableError::Stopped.into())
    }
}

impl NodeScope {
    pub fn child(&self, name: &str) -> FrameScope {
        FrameScope {
            session: self.frame.session.clone(),
            path: format!("{}:{}", self.id(), serde_json::to_string(name).unwrap()),
            root_cancel: self.frame.root_cancel.clone(),
            observer: self.frame.observer.clone(),
        }
    }
    pub fn current() -> Option<Self> {
        NODE.try_with(Clone::clone).ok()
    }
    pub async fn within<F: Future>(&self, future: F) -> F::Output {
        NODE.scope(self.clone(), future).await
    }
    pub fn id(&self) -> String {
        // JSON 编码片段，避免节点名称中的 '/' 等字符造成身份碰撞。
        format!(
            "{}:{}",
            self.frame.path,
            serde_json::to_string(&(&self.node, self.invocation)).unwrap()
        )
    }
    pub fn approve(&self, timeout: Option<Duration>) -> Result<(), GraphError> {
        let id = self.id();
        let record = self.frame.record();
        if let Some(request) = record.approvals.get(&id) {
            if let Some(receipt) = &request.receipt {
                let approved = receipt.decision == ApprovalDecision::Approve;
                if !approved {
                    self.frame.update(|r| {
                        r.approvals.get_mut(&id).unwrap().applied = true;
                        r.status = RunStatus::Rejected;
                        Ok(())
                    })?;
                }
                return if approved {
                    Ok(())
                } else {
                    Err(DurableError::Stopped.into())
                };
            }
            if request.deadline_unix_ms.is_some_and(|t| now_ms() >= t) {
                self.frame.update(|r| {
                    r.status = RunStatus::Expired;
                    Ok(())
                })?;
                return Err(DurableError::Stopped.into());
            }
        } else {
            let content = record.frames[&self.frame.path].state.clone();
            self.frame.update(|r| {
                r.approvals.insert(
                    id.clone(),
                    ApprovalRequest {
                        id,
                        frame: self.frame.path.clone(),
                        node: self.node.clone(),
                        content,
                        content_token: uuid::Uuid::new_v4().to_string(),
                        deadline_unix_ms: timeout.map(|t| {
                            now_ms().saturating_add(t.as_millis().min(u128::from(u64::MAX)) as u64)
                        }),
                        receipt: None,
                        applied: false,
                    },
                );
                Ok(())
            })?;
        }
        self.frame.update(|r| {
            r.status = RunStatus::Suspended;
            Ok(())
        })?;
        self.frame.emit(&self.node, "waiting_approval");
        Err(DurableError::Suspended.into())
    }
}

pub(crate) fn root_record<S: WorkflowState, M: MergeStrategy<S>>(
    id: &str,
    version: RunVersion,
    graph: &Graph<S, M>,
    state: &S,
    max_steps: usize,
) -> Result<RunRecord, DurableError> {
    let frame = FrameRecord {
        graph_signature: signature(graph),
        state: serde_json::to_value(state.snapshot())?,
        next_node: Some(graph.start_node().into()),
        steps_used: 0,
        max_steps,
        active: false,
    };
    Ok(RunRecord {
        format_version: RUN_FORMAT_VERSION,
        run_id: id.into(),
        revision: 0,
        version,
        status: RunStatus::Running,
        steps_used: 0,
        max_steps,
        frames: BTreeMap::from([("root".into(), frame)]),
        approvals: BTreeMap::new(),
        tasks: BTreeMap::new(),
    })
}
