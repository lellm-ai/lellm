//! ToolCall Accumulator — 按 index 聚合增量 delta。
//!
//! 以 index 为 key，因为很多 Provider 的第一批 delta 只有 index 而没有 id。

use lellm_core::{LlmError, ToolCall};

/// 工具调用增量 — 统一格式，吸收所有 Provider 差异
#[derive(Debug)]
pub struct ToolCallDelta {
    /// 工具调用在消息中的位置索引（用于聚合）
    pub index: usize,
    /// 工具调用 ID（可能延迟出现）
    pub id: Option<String>,
    /// 工具名称（可能延迟出现）
    pub name: Option<String>,
    /// 参数增量片段（最终拼接为完整 JSON）
    pub arguments_delta: Option<String>,
}

/// ToolCall 增量组装器 — 按 index 聚合。
///
/// 独立状态机，可单独测试。
pub struct ToolCallAccumulator {
    current: std::collections::HashMap<usize, PendingToolCall>,
}

struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self {
            current: std::collections::HashMap::new(),
        }
    }

    /// 接收统一的 ToolCallDelta 增量并组装
    pub fn push(&mut self, delta: &ToolCallDelta) {
        let entry = self
            .current
            .entry(delta.index)
            .or_insert_with(|| PendingToolCall {
                id: None,
                name: None,
                arguments: String::new(),
            });

        if let Some(ref id) = delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(ref name) = delta.name {
            entry.name = Some(name.clone());
        }
        if let Some(ref d) = delta.arguments_delta {
            entry.arguments.push_str(d);
        }
    }

    /// 完成组装，返回完整的 ToolCall 列表（按 index 排序）
    pub fn finalize(self) -> Result<Vec<ToolCall>, LlmError> {
        let mut entries: Vec<_> = self.current.into_iter().collect();
        entries.sort_by_key(|&(idx, _)| idx);

        let mut result = Vec::new();
        for (_index, pending) in entries {
            let id = pending.id.unwrap_or_else(|| "unknown".to_string());
            let name = pending.name.unwrap_or_else(|| "unknown".to_string());
            let arguments: serde_json::Value = if pending.arguments.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&pending.arguments)
                    .unwrap_or(serde_json::Value::String(pending.arguments))
            };
            result.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_delta_into_tool_call() {
        let mut acc = ToolCallAccumulator::new();

        // 第一批 delta: 只有 index + name
        acc.push(&ToolCallDelta {
            index: 0,
            id: None,
            name: Some("read_file".into()),
            arguments_delta: None,
        });

        // 第二批 delta: arguments 片段
        acc.push(&ToolCallDelta {
            index: 0,
            id: Some("tc_123".into()),
            name: None,
            arguments_delta: Some(r#""{"path""#.into()),
        });

        // 第三批 delta: arguments 片段
        acc.push(&ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_delta: Some(r#": "/test"}}"#.into()),
        });

        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tc_123");
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn empty_accumulator_returns_empty() {
        let acc = ToolCallAccumulator::new();
        let calls = acc.finalize().unwrap();
        assert!(calls.is_empty());
    }
}
