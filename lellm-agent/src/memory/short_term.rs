//! 短期记忆 — 基于 VecDeque 的对话历史窗口。

use std::collections::VecDeque;

use lellm_core::Message;

/// 短期记忆 — 环形缓冲区，默认 200 条容量。
pub struct ShortTermMemory {
    messages: VecDeque<Message>,
    capacity: usize,
}

impl ShortTermMemory {
    pub fn new(capacity: usize) -> Self {
        Self {
            messages: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, msg: Message) {
        if self.messages.len() >= self.capacity {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }

    /// 获取最近 n 条消息
    pub fn recent(&self, n: usize) -> Vec<Message> {
        if n >= self.messages.len() {
            self.messages.iter().cloned().collect()
        } else {
            self.messages
                .iter()
                .skip(self.messages.len() - n)
                .cloned()
                .collect()
        }
    }

    /// 获取所有消息
    pub fn messages(&self) -> Vec<Message> {
        self.messages.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

impl Default for ShortTermMemory {
    fn default() -> Self {
        Self::new(200)
    }
}
