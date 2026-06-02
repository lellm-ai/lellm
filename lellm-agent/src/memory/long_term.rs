//! 长期记忆 — SQLite 持久化。

use super::{MemoryStore, MemoryType};
use anyhow::Result;

#[derive(Debug)]
pub struct LongTermMemory {
    store: MemoryStore,
}

impl LongTermMemory {
    pub async fn new(path: &str) -> Result<Self> {
        let store = MemoryStore::new(path).await?;
        Ok(Self { store })
    }

    pub async fn save(
        &self,
        content: &str,
        _type: MemoryType,
        keywords: &[&str],
        score: f64,
    ) -> Result<()> {
        let type_str = match _type {
            MemoryType::ToolCall => "ToolCall",
            MemoryType::ToolResult => "ToolResult",
            MemoryType::LlmResponse => "LlmResponse",
            MemoryType::UserInput => "UserInput",
            MemoryType::Decision => "Decision",
            MemoryType::Summary => "Summary",
        };

        self.store
            .insert(content, type_str, keywords, score)
            .await?;
        Ok(())
    }

    pub async fn retrieve(&self, keyword: &str) -> Result<Vec<String>> {
        Ok(self.store.search(keyword).await?)
    }
}
