//! ModelRouter — 任务分级路由。

/// 任务级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLevel {
    /// 快速/便宜，如简单问答、格式转换
    Flash,
    /// 默认，如一般对话
    Standard,
    /// 复杂推理，如代码生成、深度分析
    Pro,
}

/// Provider 的模型映射。
#[derive(Debug, Clone)]
pub struct ProviderModels {
    pub flash: String,
    pub standard: String,
    pub pro: String,
}

/// ModelRouter 配置。
#[derive(Debug, Clone)]
pub struct ModelRouterConfig {
    pub providers: Vec<(String, ProviderModels)>,
    pub default_level: TaskLevel,
}

impl Default for ModelRouterConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            default_level: TaskLevel::Standard,
        }
    }
}

/// 模型路由器 — 根据任务级别选择模型。
pub struct ModelRouter {
    #[allow(dead_code)]
    config: ModelRouterConfig,
}

impl ModelRouter {
    pub fn new(config: ModelRouterConfig) -> Self {
        Self { config }
    }

    /// 根据任务级别解析模型名称
    pub fn resolve_model(&self, level: TaskLevel) -> Option<&str> {
        // TODO: 根据 level 查找对应的模型
        let _ = level;
        None
    }
}
