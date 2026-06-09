//! 循环检测器 — 指纹去重 + 阈值触发。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use lellm_core::ToolCall;

/// 工具调用指纹（参数归一化后）。
#[derive(Clone, Debug)]
pub struct ToolCallFingerprint {
    pub tool_name: String,
    pub normalized_args: String,
}

impl Hash for ToolCallFingerprint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.tool_name.hash(state);
        self.normalized_args.hash(state);
    }
}

impl PartialEq for ToolCallFingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.tool_name == other.tool_name && self.normalized_args == other.normalized_args
    }
}

impl Eq for ToolCallFingerprint {}

impl ToolCallFingerprint {
    /// 从 ToolCall 生成指纹
    pub fn from_call(call: &ToolCall) -> Self {
        let normalized = Self::normalize_json(&call.arguments);
        Self {
            tool_name: call.name.clone(),
            normalized_args: normalized,
        }
    }

    /// JSON 键排序 + 空白去除
    fn normalize_json(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(k, _)| (*k).clone());
                let parts: Vec<_> = entries
                    .iter()
                    .map(|(k, v)| format!("{}:{}", k, Self::normalize_json(v)))
                    .collect();
                parts.join(",")
            }
            serde_json::Value::Array(arr) => {
                let parts: Vec<_> = arr.iter().map(Self::normalize_json).collect();
                format!("[{}]", parts.join(","))
            }
            serde_json::Value::String(s) => s.replace(char::is_whitespace, ""),
            other => other.to_string().replace(char::is_whitespace, ""),
        }
    }
}

/// 循环干预方式
#[derive(Debug, Clone)]
pub enum LoopIntervention {
    /// 注入系统提示
    InjectHint(String),
    /// 中断循环
    Break,
}

/// 循环检测器
pub struct LoopDetector {
    history: Vec<ToolCallFingerprint>,
    threshold: usize,
}

impl LoopDetector {
    pub fn new(threshold: usize) -> Self {
        Self {
            history: Vec::new(),
            threshold,
        }
    }

    /// 记录一轮 tool_calls 的指纹
    pub fn record(&mut self, calls: &[ToolCall]) {
        let fingerprints: Vec<_> = calls.iter().map(ToolCallFingerprint::from_call).collect();
        self.history.extend(fingerprints);
    }

    /// 检查是否检测到循环
    pub fn check(&self) -> Option<LoopIntervention> {
        if self.history.len() < self.threshold * 2 {
            return None;
        }

        let recent_len = self.threshold.min(self.history.len());
        let recent = &self.history[self.history.len() - recent_len..];

        // 检查最近 N 个指纹是否高度重复
        let mut counts: HashMap<&ToolCallFingerprint, usize> = HashMap::new();
        for fp in recent {
            *counts.entry(fp).or_insert(0) += 1;
        }

        let max_repeat = counts.values().max().copied().unwrap_or(0);
        if max_repeat >= self.threshold {
            Some(LoopIntervention::InjectHint(
                "你正在重复调用相同的工具，请尝试不同方法".to_string(),
            ))
        } else {
            None
        }
    }

    /// 重置检测器
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new(3)
    }
}
