//! ModelRouter — 任务分级路由。

use std::collections::HashMap;
use std::sync::Arc;

use lellm_core::LlmError;

use crate::LlmProvider;

/// 任务级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskLevel {
    /// 快速/便宜，如简单问答、格式转换
    Flash,
    /// 默认，如一般对话
    Standard,
    /// 复杂推理，如代码生成、深度分析
    Pro,
}

/// 路由条目 — Provider + Model 的组合
#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub provider_id: String,
    pub model: String,
}

/// 解析后的模型 — 从 Registry 中解析 RouteEntry 得到
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn LlmProvider>,
    pub model: String,
    /// 模型上下文窗口大小（Token），用于 v0.2 自动推导 ContextBudget
    /// 若未知则设为 `None`，使用 ContextBudget 的固定默认值
    pub context_window: Option<usize>,
}

/// 模型路由器 — 根据任务级别选择路由。
pub struct ModelRouter {
    routes: HashMap<TaskLevel, RouteEntry>,
}

impl ModelRouter {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn add_route(&mut self, level: TaskLevel, entry: RouteEntry) {
        self.routes.insert(level, entry);
    }

    /// 根据任务级别解析路由
    pub fn resolve(&self, level: TaskLevel) -> Option<&RouteEntry> {
        self.routes.get(&level)
    }
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Provider 注册表 — 持有所有 Provider 实例。
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, id: &str, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(id.to_string(), provider);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(id).cloned()
    }

    /// 从 RouteEntry 解析为 ResolvedModel
    pub fn resolve(&self, route: &RouteEntry) -> Result<ResolvedModel, LlmError> {
        let provider = self
            .get(&route.provider_id)
            .ok_or_else(|| LlmError::ApiError {
                provider: route.provider_id.clone(),
                status: 0,
                code: None,
                message: format!("provider not registered: {}", route.provider_id),
            })?;
        Ok(ResolvedModel {
            provider,
            model: route.model.clone(),
            context_window: None,
        })
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
