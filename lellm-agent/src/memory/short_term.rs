//! 短期记忆 — 环形缓冲区，默认 200 条容量。

use super::{MemoryEntry, MemoryType};

#[derive(Debug)]
pub struct ShortTermMemory {
    buffer: Vec<MemoryEntry>,
    capacity: usize,
    next_id: u64,
}

impl ShortTermMemory {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Vec::new(),
            capacity,
            next_id: 1,
        }
    }

    pub fn add(&mut self, content: String, r#type: MemoryType) {
        if self.buffer.len() >= self.capacity {
            self.buffer.remove(0);
        }

        let entry = MemoryEntry {
            id: self.next_id,
            content,
            r#type: r#type.clone(),
            timestamp: chrono::Utc::now(),
            score: r#type.default_score(),
        };

        self.next_id += 1;
        self.buffer.push(entry);
    }

    pub fn recent(&self, n: usize) -> &[MemoryEntry] {
        if n >= self.buffer.len() {
            &self.buffer
        } else {
            &self.buffer[self.buffer.len() - n..]
        }
    }

    pub fn entries(&self) -> &[MemoryEntry] {
        &self.buffer
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

impl Default for ShortTermMemory {
    fn default() -> Self {
        Self::new(200)
    }
}
