use super::*;
use crate::GraphError;
use futures::FutureExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplayPolicy {
    ManualRecovery,
    ReplaySafe,
    IdempotentWithKey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    Planned,
    Started,
    Completed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskResolution {
    /// 外部对账确认的结果，必须符合原任务的输出类型。
    Completed(Value),
    /// 显式授权再次尝试；外部副作用的重复风险由对账者承担。
    Retry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    pub id: String,
    pub version: String,
    pub input: Value,
    pub policy: ReplayPolicy,
    pub status: TaskStatus,
    pub attempts: u32,
    #[serde(with = "optional_json")]
    pub result: Option<Value>,
    pub resolutions: BTreeMap<String, TaskResolution>,
}

#[derive(Debug, Clone)]
pub struct TaskInvocation {
    pub task_id: String,
    pub idempotency_key: String,
    pub attempt: u32,
}

/// 在 Graph 节点执行期间捕获的任务上下文。spawn 时必须显式传递，避免丢失身份。
#[derive(Clone)]
pub struct TaskRuntime {
    scope: NodeScope,
}

struct InFlight {
    frame: FrameScope,
    id: String,
}
impl Drop for InFlight {
    fn drop(&mut self) {
        self.frame.session.lock().in_flight.remove(&self.id);
    }
}

impl TaskRuntime {
    pub fn current() -> Option<Self> {
        NodeScope::current().map(|scope| Self { scope })
    }

    pub async fn execute<T, F, Fut>(
        &self,
        slot: &str,
        version: &str,
        input: Value,
        policy: ReplayPolicy,
        execute: F,
    ) -> Result<T, GraphError>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(TaskInvocation) -> Fut,
        Fut: Future<Output = Result<T, GraphError>>,
    {
        let id = format!(
            "{}:{}",
            self.scope.id(),
            serde_json::to_string(slot).unwrap()
        );
        let frame = &self.scope.frame;
        if !frame.session.lock().in_flight.insert(id.clone()) {
            return Err(DurableError::Conflict("duplicate concurrent task identity".into()).into());
        }
        let _guard = InFlight {
            frame: frame.clone(),
            id: id.clone(),
        };
        if frame.root_cancel.is_cancelled() {
            frame.cancel()?;
            unreachable!();
        }
        let record = frame.record();
        if matches!(
            record.status,
            RunStatus::Completed | RunStatus::Cancelled | RunStatus::Rejected | RunStatus::Expired
        ) {
            return Err(DurableError::Stopped.into());
        }
        if let Some(task) = record.tasks.get(&id) {
            if task.version != version || task.input != input || task.policy != policy {
                return Err(DurableError::Incompatible(format!(
                    "task input/version/policy changed: {id}"
                ))
                .into());
            }
            if task.status == TaskStatus::Completed {
                return serde_json::from_value(task.result.clone().ok_or_else(|| {
                    DurableError::Conflict("completed task without result".into())
                })?)
                .map_err(|e| DurableError::Codec(e).into());
            }
            if matches!(task.status, TaskStatus::Started | TaskStatus::Unknown)
                && policy == ReplayPolicy::ManualRecovery
            {
                return self.unknown(&id);
            }
        } else {
            frame.update(|r| {
                r.tasks.insert(
                    id.clone(),
                    TaskRecord {
                        id: id.clone(),
                        version: version.into(),
                        input,
                        policy,
                        status: TaskStatus::Planned,
                        attempts: 0,
                        result: None,
                        resolutions: BTreeMap::new(),
                    },
                );
                Ok(())
            })?;
        }
        let attempt = frame.update(|r| {
            let task = r.tasks.get_mut(&id).unwrap();
            task.status = TaskStatus::Started;
            task.attempts = task
                .attempts
                .checked_add(1)
                .ok_or_else(|| DurableError::Conflict("task attempt overflow".into()))?;
            Ok(task.attempts)
        })?;
        let invocation = TaskInvocation {
            task_id: id.clone(),
            idempotency_key: serde_json::to_string(&(&record.run_id, &id)).unwrap(),
            attempt,
        };
        let result = std::panic::AssertUnwindSafe(async { execute(invocation).await })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(
                    DurableError::Conflict("task panicked; external outcome may be unknown".into())
                        .into(),
                )
            });
        match result {
            Ok(result) => {
                let value = serde_json::to_value(&result).map_err(DurableError::from)?;
                frame.update(|r| {
                    let task = r.tasks.get_mut(&id).unwrap();
                    task.result = Some(value);
                    task.status = TaskStatus::Completed;
                    Ok(())
                })?;
                Ok(result)
            }
            Err(error) => {
                if policy == ReplayPolicy::ManualRecovery {
                    self.unknown(&id)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn unknown<T>(&self, id: &str) -> Result<T, GraphError> {
        self.scope.frame.update(|r| {
            r.tasks.get_mut(id).ok_or(DurableError::NotFound)?.status = TaskStatus::Unknown;
            if matches!(
                r.status,
                RunStatus::Cancelled | RunStatus::Rejected | RunStatus::Expired
            ) {
                return Err(DurableError::Stopped);
            }
            r.status = RunStatus::Suspended;
            Ok(())
        })?;
        Err(DurableError::Suspended.into())
    }
}

// Option<Value> 默认会把 Some(Null) 读成 None；显式包裹已存在的结果。
mod optional_json {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Present {
        value: Value,
    }
    pub fn serialize<S: Serializer>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(|value| Present {
                value: value.clone(),
            })
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error> {
        Ok(Option::<Present>::deserialize(deserializer)?.map(|present| present.value))
    }
}
