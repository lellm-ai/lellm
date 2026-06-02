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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_term_memory_push_and_messages() {
        let mut memory = ShortTermMemory::new(10);
        memory.push(Message::User {
            content: lellm_core::text_block("hello".to_string()),
        });
        assert_eq!(memory.len(), 1);
        assert!(!memory.is_empty());
    }

    #[test]
    fn test_short_term_memory_capacity() {
        let mut memory = ShortTermMemory::new(2);
        for i in 0..5 {
            memory.push(Message::User {
                content: lellm_core::text_block(format!("msg{}", i)),
            });
        }
        assert_eq!(memory.len(), 2);
        let recent = memory.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].extract_text(), "msg3");
        assert_eq!(recent[1].extract_text(), "msg4");
    }

    #[test]
    fn test_short_term_memory_clear() {
        let mut memory = ShortTermMemory::new(10);
        memory.push(Message::User {
            content: lellm_core::text_block("hello".to_string()),
        });
        memory.clear();
        assert!(memory.is_empty());
    }

    #[test]
    fn test_short_term_memory_default_capacity() {
        let mut memory = ShortTermMemory::default();
        for i in 0..205 {
            memory.push(Message::User {
                content: lellm_core::text_block(format!("msg{}", i)),
            });
        }
        assert_eq!(memory.len(), 200);
    }
}
