//! 工具注册表 — 支持按名称、同义词、分类搜索。

use std::collections::{HashMap, HashSet};

use lellm_core::ToolDefinition;

/// 工具来源分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Builtin,
    Dynamic,
    Mcp,
    Skill,
}

/// 工具搜索结果（包含来源信息）。
#[derive(Debug, Clone)]
pub struct ToolSearchResult {
    pub definition: ToolDefinition,
    pub source: ToolSource,
    pub category: String,
}

impl ToolSearchResult {
    pub fn name(&self) -> &str {
        &self.definition.name
    }
}

/// 工具注册表。
pub struct ToolRegistry {
    tools: HashMap<String, ToolSearchResult>,
    synonyms: HashMap<String, Vec<String>>,
    categories: HashMap<String, Vec<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            synonyms: HashMap::new(),
            categories: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, source: ToolSource, def: ToolDefinition) {
        let category = Self::infer_category(name);
        let result = ToolSearchResult {
            definition: def,
            source,
            category: category.clone(),
        };
        self.tools.insert(name.to_string(), result);
        self.categories
            .entry(category)
            .or_insert_with(Vec::new)
            .push(name.to_string());
    }

    pub fn add_synonyms(&mut self, tool_name: &str, synonyms: &[&str]) {
        for syn in synonyms {
            self.synonyms
                .entry(syn.to_string())
                .or_insert_with(Vec::new)
                .push(tool_name.to_string());
        }
    }

    /// 搜索工具（精确 → 同义词 → 子串兜底）
    pub fn search(&self, query: &str) -> Vec<ToolSearchResult> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        let mut seen = HashSet::new();

        if let Some(result) = self.tools.get(query) {
            results.push(result.clone());
            seen.insert(query);
        }

        if let Some(names) = self.synonyms.get(&query_lower) {
            for name in names {
                if seen.insert(name) {
                    if let Some(result) = self.tools.get(name) {
                        results.push(result.clone());
                    }
                }
            }
        }

        if results.is_empty() {
            for (name, result) in &self.tools {
                if name.to_lowercase().contains(&query_lower) {
                    results.push(result.clone());
                }
            }
        }

        results
    }

    pub fn search_category(&self, category: &str) -> Vec<ToolSearchResult> {
        let mut results = Vec::new();
        if let Some(names) = self.categories.get(category) {
            for name in names {
                if let Some(result) = self.tools.get(name) {
                    results.push(result.clone());
                }
            }
        }
        results
    }

    pub fn list_tools(&self) -> Vec<ToolSearchResult> {
        self.tools.values().cloned().collect()
    }

    fn infer_category(name: &str) -> String {
        if name.starts_with("read") || name.starts_with("write") || name.starts_with("bash") {
            "builtin".to_string()
        } else {
            "custom".to_string()
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
