//! AgentBuilder — Agent 链式构建器。
//!
//! 提供推荐的入口 API，一步构建 `Graph<AgentState>`。
//!
//! # 示例
//! ```ignore
//! use lellm_agent::{AgentBuilder, ToolRegistration};
//! use lellm_core::Prompt;
//!
//! // 简单文本 — build() 返回 Graph<AgentState>
//! let graph = AgentBuilder::new(model)
//!     .system("你是一个有帮助的助手。")
//!     .tool(search_tool)
//!     .tool(weather_tool)
//!     .max_iterations(20)
//!     .build();
//!
//! // 分层构建 — 最大化前缀缓存
//! let graph = AgentBuilder::new(model)
//!     .system(
//!         Prompt::new()
//!             .stable("核心身份…")
//!             .stable("工具指南…")
//!             .dynamic("会话上下文…")
//!             .build(),
//!     )
//!     .build();
//!
//! // 便捷执行 — build_loop() 返回 ToolUseLoop Facade
//! let result = AgentBuilder::new(model)
//!     .tools([search_tool, weather_tool])
//!     .build_loop()
//!     .invoke(messages)
//!     .await?;
//! ```

use std::sync::Arc;

use lellm_core::{Prompt, ReasoningConfig, ToolChoice};
use lellm_graph::Graph;
use lellm_provider::ResolvedModel;

use super::config::{ToolCachePolicy, ToolUseConfig, ToolUseDeps};
use super::context::{ContextBudget, LocalCompactor};
use super::fallback::FallbackStrategy;
use super::react::{CompactorNode, LLMNode, ToolNode, build_react_graph};
use super::request_opts::RequestOptions;
use super::retry::RetryPolicy;
use super::runtime::ToolUseLoop;
use super::tools::{CompositeCatalog, StaticCatalog, ToolCatalog, ToolExecutor, ToolRegistration};
use super::typed_state::{AgentState, AgentStateMerge};

/// Agent 链式构建器 — 推荐的 Agent 创建方式。
///
/// 内部收集静态工具和动态目录，`build()` 时组装为 `ToolCatalog`，
/// 再传给 `ToolUseLoop::new()`。所有 setter 返回 `self`（不借用），
/// 支持流畅的链式调用。
pub struct AgentBuilder {
    model: ResolvedModel,
    /// 收集通过 `.tool()` 注册的本地静态工具（最高优先级）
    static_tools: Vec<ToolRegistration>,
    /// 收集通过 `.catalog()` 注册的动态目录（按注册顺序，先绑定的优先级高于后绑定的）
    catalogs: Vec<Arc<dyn ToolCatalog>>,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_hash_stability() {
        // 相同输入应该产生相同的 hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher1 = DefaultHasher::new();
        let mut hasher2 = DefaultHasher::new();

        // 模拟相同的 DSL 输入（工具顺序相同）
        "test-provider".hash(&mut hasher1);
        "test-model".hash(&mut hasher1);
        "alpha".hash(&mut hasher1);
        "beta".hash(&mut hasher1);
        "test prompt".hash(&mut hasher1);
        10usize.hash(&mut hasher1);
        4096u32.hash(&mut hasher1);

        "test-provider".hash(&mut hasher2);
        "test-model".hash(&mut hasher2);
        "alpha".hash(&mut hasher2);
        "beta".hash(&mut hasher2);
        "test prompt".hash(&mut hasher2);
        10usize.hash(&mut hasher2);
        4096u32.hash(&mut hasher2);

        assert_eq!(
            hasher1.finish(),
            hasher2.finish(),
            "Same inputs should produce same hash"
        );
    }

    #[test]
    fn test_canonical_hash_order_dependent() {
        // D11: 工具顺序不同应该产生不同的 hash
        // canonical = DSL 原貌，不做语义归一化
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher1 = DefaultHasher::new();
        let mut hasher2 = DefaultHasher::new();

        // .tools([alpha, beta])
        "alpha".hash(&mut hasher1);
        "beta".hash(&mut hasher1);

        // .tools([beta, alpha])
        "beta".hash(&mut hasher2);
        "alpha".hash(&mut hasher2);

        assert_ne!(
            hasher1.finish(),
            hasher2.finish(),
            "Tool order should affect hash — canonical = DSL 原貌"
        );
    }

    #[test]
    fn test_canonical_hash_different_inputs() {
        // 不同输入应该产生不同的 hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher1 = DefaultHasher::new();
        let mut hasher2 = DefaultHasher::new();

        "alpha".hash(&mut hasher1);
        "alpha".hash(&mut hasher2);
        "beta".hash(&mut hasher2);

        assert_ne!(
            hasher1.finish(),
            hasher2.finish(),
            "Different inputs should produce different hash"
        );
    }
}

impl AgentBuilder {
    /// 创建构建器，绑定模型。
    pub fn new(model: ResolvedModel) -> Self {
        Self {
            model,
            static_tools: Vec::new(),
            catalogs: Vec::new(),
            config: ToolUseConfig::default(),
            deps: ToolUseDeps::default(),
        }
    }

    /// 注册工具。
    pub fn tool(mut self, reg: ToolRegistration) -> Self {
        self.static_tools.push(reg);
        self
    }

    /// 批量注册工具。
    pub fn tools(mut self, registrations: impl IntoIterator<Item = ToolRegistration>) -> Self {
        self.static_tools.extend(registrations);
        self
    }

    /// 绑定动态工具目录（MCP、插件系统等）。
    ///
    /// 可调用多次。按注册顺序，先绑定的优先级高于后绑定的。
    /// 静态工具（`.tool()`）永远拥有最高优先级。
    ///
    /// # 示例
    /// ```ignore
    /// use std::sync::Arc;
    /// use lellm_agent::{AgentBuilder, ToolCatalog};
    /// use lellm_mcp::{McpCatalog, McpClient};
    ///
    /// let client = Arc::new(McpClient::with_transport(transport));
    /// let catalog = McpCatalog::discover(client).await?;
    /// let agent = AgentBuilder::new(model)
    ///     .catalog(Arc::new(catalog))
    ///     .build();
    /// ```
    pub fn catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalogs.push(catalog);
        self
    }

    /// 设置最大迭代轮次（默认 10）。
    pub fn max_iterations(mut self, max: usize) -> Self {
        self.config.max_iterations = max;
        self
    }

    /// 设置每次 LLM 请求的最大输出 token 数（默认 4k）。
    pub fn max_output_tokens(mut self, max: u32) -> Self {
        self.config.max_output_tokens = max;
        self
    }

    /// 设置整个 Agent Run 的最大输出 token 总数。
    pub fn max_total_output_tokens(mut self, max: u32) -> Self {
        self.config.max_total_output_tokens = Some(max);
        self
    }

    /// 设置系统提示。
    ///
    /// 支持简单文本或分层 `Prompt`（通过 `From<String>` 自动转换）。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// // 简单文本
    /// let agent = AgentBuilder::new(model)
    ///     .system("你是一个有帮助的助手。")
    ///     .build();
    ///
    /// // 分层构建 — 最大化前缀缓存
    /// use lellm_core::Prompt;
    /// let agent = AgentBuilder::new(model)
    ///     .system(
    ///         Prompt::new()
    ///             .stable("核心身份…")
    ///             .stable("工具指南…")
    ///             .dynamic("会话上下文…")
    ///             .build(),
    ///     )
    ///     .build();
    /// ```
    pub fn system(mut self, system: impl Into<Prompt>) -> Self {
        self.config.system = Some(system.into());
        self
    }

    /// 设置系统提示（纯文本）。
    ///
    /// 这是 `.system()` 的别名，保留用于向后兼容。
    #[deprecated(since = "0.5.0", note = "Use `.system()` instead")]
    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.config.system = Some(prompt.into());
        self
    }

    /// 设置工具缓存策略（默认 `Auto`）。
    ///
    /// - `Auto`：为未设置 `cache_control` 的工具自动添加 `Breakpoint`
    /// - `Preserve`：不修改用户设置的 `cache_control`
    /// - `Disabled`：清除所有工具的 `cache_control`
    pub fn tool_cache_policy(mut self, policy: ToolCachePolicy) -> Self {
        self.config.tool_cache_policy = policy;
        self
    }

    // ─── RequestOptions 快捷 setter ──────────────────────────

    /// 设置完整的 RequestOptions（覆盖所有生成参数）。
    pub fn request_options(mut self, opts: RequestOptions) -> Self {
        self.config.request_options = opts;
        self
    }

    /// 设置生成温度（0.0 ~ 2.0）。
    pub fn temperature(mut self, t: f64) -> Self {
        self.config.request_options.temperature = Some(t);
        self
    }

    /// 设置 nucleus sampling 阈值（0.0 ~ 1.0）。
    pub fn top_p(mut self, p: f64) -> Self {
        self.config.request_options.top_p = Some(p);
        self
    }

    /// 设置随机种子，保证可复现性。
    pub fn seed(mut self, s: u64) -> Self {
        self.config.request_options.seed = Some(s);
        self
    }

    /// 设置工具选择策略（仅首轮生效）。
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.config.request_options.tool_choice = Some(choice);
        self
    }

    /// 设置停止序列。
    pub fn stop_sequences(mut self, seqs: Vec<String>) -> Self {
        self.config.request_options.stop_sequences = Some(seqs);
        self
    }

    /// 设置预填充文本（引导模型输出方向）。
    pub fn prefill(mut self, text: String) -> Self {
        self.config.request_options.prefill = Some(text);
        self
    }

    /// 设置推理配置（控制模型是否进行深度推理）。
    pub fn reasoning(mut self, r: ReasoningConfig) -> Self {
        self.config.request_options.reasoning = Some(r);
        self
    }

    /// 设置是否流式输出推理过程。
    pub fn stream_thinking(mut self, enable: bool) -> Self {
        self.config.stream_thinking = enable;
        self
    }

    /// 设置单轮推理 Token 上限。
    pub fn reasoning_budget(mut self, max: u32) -> Self {
        self.config.request_options.max_reasoning_tokens = Some(max);
        self
    }

    /// 设置整个 Agent Run 的最大推理 Token 总数。
    pub fn max_total_reasoning_tokens(mut self, max: u32) -> Self {
        self.config.max_total_reasoning_tokens = Some(max);
        self
    }

    /// 设置 Fallback 策略。
    pub fn fallback(mut self, fallback: Arc<dyn FallbackStrategy>) -> Self {
        self.deps.fallback = fallback;
        self
    }

    /// 设置工具重试策略。
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.config.retry_policy = policy;
        self
    }

    /// 设置上下文预算（Token 上限 + 压缩策略）。
    /// 若要关闭限制，设置 `max_tokens = usize::MAX`。
    pub fn context_budget(mut self, budget: ContextBudget) -> Self {
        self.config.context_budget = budget;
        self
    }

    /// 计算 canonical AST hash — 保持 DSL 输入顺序。
    ///
    /// Hash 输入来自 DSL 层（model, tools, system prompt, config），
    /// 不依赖 compiled graph 的 HashMap 迭代顺序。
    /// 这保证：相同 DSL 输入永远产生相同 hash，Checkpoint 不会因此失效。
    ///
    /// # Hash 输入
    ///
    /// - provider_id（稳定字符串标识，如 "openai", "anthropic"）
    /// - model name
    /// - tool names（保持 DSL 插入顺序，不排序）
    /// - system prompt hash
    /// - max_iterations
    /// - max_output_tokens
    ///
    /// # 设计原则
    ///
    /// canonical = DSL 原貌，不做语义归一化。
    /// `.tools([A, B])` 和 `.tools([B, A])` 产生不同 hash，
    /// 因为它们是不同的 DSL 输入。
    pub fn canonical_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        // 1. Model — 使用 provider_id()（稳定字符串）+ model 名称
        self.model.provider.provider_id().hash(&mut hasher);
        self.model.model.hash(&mut hasher);

        // 2. Tools — 保持 DSL 插入顺序，不排序
        //    .tools([A, B]) != .tools([B, A])
        for t in &self.static_tools {
            t.definition().name.hash(&mut hasher);
        }

        // 3. System prompt (hash 内容)
        if let Some(ref system) = self.config.system {
            format!("{:?}", system).hash(&mut hasher);
        }

        // 4. 结构性配置
        self.config.max_iterations.hash(&mut hasher);
        self.config.max_output_tokens.hash(&mut hasher);
        self.config.max_total_output_tokens.hash(&mut hasher);
        self.config.max_total_reasoning_tokens.hash(&mut hasher);

        hasher.finish()
    }

    /// 构建 ReAct Graph — 返回 `Arc<Graph<AgentState>>`。
    ///
    /// 这是核心 API，返回共享的 Graph，可直接用 `graph.run_inline()` 执行。
    /// Graph 携带 `canonical_hash`，用于 Checkpoint 的 graph compatibility 校验。
    ///
    /// # 示例
    /// ```ignore
    /// let graph = AgentBuilder::new(model).tools([...]).build();
    ///
    /// // 直接执行
    /// let state = AgentState::from_messages(messages);
    /// let mut ctx = ExecutionContext::new(state, None, CancellationToken::new());
    /// graph.run_inline(&mut ctx, max_steps).await?;
    /// ```
    pub fn build(self) -> Arc<Graph<AgentState, AgentStateMerge>> {
        let canonical_hash = self.canonical_hash();
        let (model, executor, config, deps) = self.into_parts();

        let invoker = Arc::new(super::invoker::LlmInvoker::from_config(
            model,
            &config,
            deps.fallback.clone(),
        ));

        let llm_node = LLMNode::new("llm", invoker, executor.clone(), config.clone());
        let tool_node = ToolNode::new("tool", executor.clone(), config.clone());
        let compactor_node = CompactorNode::new(
            "compactor",
            Arc::new(LocalCompactor::new()),
            config.context_budget.clone(),
        );

        Arc::new(build_react_graph(
            llm_node,
            tool_node,
            compactor_node,
            canonical_hash,
        ))
    }

    /// 构建 ToolUseLoop — 便捷 Facade。
    ///
    /// 返回 `ToolUseLoop`，持有共享的 Graph，提供 `invoke()` / `invoke_stream()` 等高级 API。
    /// 内部调用 `Graph::run_inline()`，封装了 State 初始化和结果提取。
    ///
    /// # 示例
    /// ```ignore
    /// let result = AgentBuilder::new(model)
    ///     .tools([search_tool, weather_tool])
    ///     .build_loop()
    ///     .invoke(messages)
    ///     .await?;
    /// ```
    pub fn build_loop(self) -> ToolUseLoop {
        let config = self.config.clone();
        let graph = self.build();
        ToolUseLoop::new(graph, config)
    }

    /// 内部辅助 — 分解为 (Model, Executor, Config, Deps)。
    fn into_parts(self) -> (ResolvedModel, ToolExecutor, ToolUseConfig, ToolUseDeps) {
        // 构造优先级队列：本地静态工具永远拥有最高优先级
        let mut sources: Vec<Arc<dyn ToolCatalog>> = Vec::new();

        if !self.static_tools.is_empty() {
            sources.push(Arc::new(StaticCatalog::from_tools(self.static_tools)));
        }

        sources.extend(self.catalogs);

        // 坍缩成最终的单根 Catalog
        let final_catalog: Arc<dyn ToolCatalog> = match sources.len() {
            0 => Arc::new(StaticCatalog::empty()),
            1 => sources.remove(0),
            _ => Arc::new(CompositeCatalog::new(sources)),
        };

        let executor =
            ToolExecutor::with_retry_policy(final_catalog, self.config.retry_policy.clone());

        (self.model, executor, self.config, self.deps)
    }
}
