//! weather_agent — 天气查询链
//!
//! 工具链：`resolve_city(address) → http_get(wttr.in) → LLM 解析为 JSON`
//!
//! resolve_city 四级降级详见 [`city_resolver`] 模块。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example weather_agent [地址]
//! ```

#[path = "_shared/shared.rs"]
mod shared;

#[path = "_shared/city_resolver.rs"]
mod city_resolver;

use lellm_agent::{AgentBuilder, ToolUseLoop, schemars::JsonSchema, serde::Deserialize};
use lellm_core::{Message, Prompt, ToolError, ToolErrorKind, text_block};
use lellm_derive::Tool;
use lellm_provider::LlmProvider;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::CodecProvider;
use lellm_provider::providers::openai_compat::OpenAICompatCodec;
use std::sync::Arc;

// ─── Tool 1: resolve_city ───────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(
    name = "resolve_city",
    description = "将地址解析为 wttr.in 城市英文名。四级降级：别名表 → 腾讯地图 → LLM → unknown。始终调用此工具，不要猜测。"
)]
#[allow(dead_code)]
struct ResolveCityArgs {
    /// 地址或地名（如 "浦东"、"新宿"、"曼哈顿"）
    address: String,
}

// ─── Tool 2: http_get ───────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(
    name = "http_get",
    description = "发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。"
)]
#[allow(dead_code)]
struct HttpGetArgs {
    /// 完整的请求 URL
    url: String,
}

fn http_get(url: &str) -> Result<String, ToolError> {
    reqwest::blocking::get(url)
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Network,
            message: format!("请求失败: {e}"),
        })?
        .text()
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("读取响应失败: {e}"),
        })
}

// ─── 工具注册 ────────────────────────────────────────────────────

fn register_weather_tools(
    llm_provider: Option<Arc<dyn LlmProvider>>,
) -> Vec<lellm_agent::ExecutableTool> {
    vec![
        // resolve_city：四级降级
        ResolveCityArgs::safe(move |args| {
            let provider = llm_provider.clone();
            async move {
                let address = args.address;
                let address_for_blocking = address.clone();
                let mut result = tokio::task::spawn_blocking(move || {
                    city_resolver::resolve_city(&address_for_blocking)
                })
                .await
                .map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("任务失败: {e}"),
                })?;

                // 第三级 miss → 第四级：LLM 轻量推理
                if result.source == "unknown" {
                    tracing::debug!(address = %address, "alias+tencent miss, trying LLM fallback");
                    if let Some(ref p) = provider {
                        if let Some(city) = city_resolver::resolve_via_llm(p, &address).await {
                            tracing::debug!(city = %city.city_en, "LLM fallback success");
                            result = city;
                        }
                    }
                }

                Ok(serde_json::json!(serde_json::to_string(&result).map_err(
                    |e| ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("序列化失败: {e}"),
                    }
                )?))
            }
        }),
        // http_get：通用 HTTP 请求
        HttpGetArgs::safe(|args| async move {
            let body = tokio::task::spawn_blocking(move || http_get(&args.url))
                .await
                .map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("任务失败: {e}"),
                })??;
            Ok(serde_json::json!(body))
        }),
    ]
}

// ─── 分层 System Prompt — 最大化前缀缓存 ────────────────────────

/// 构建分层 Prompt，全部 cached — 用户查询通过 user message 传递。
///
/// 缓存策略：
/// - L1 核心身份：永不变化
/// - L2 工具指南：极少变化
/// - L3 字段规则：极少变化
/// - L4 输出格式：极少变化（最后一个 cached layer → 获得 cache_control 断点）
///
/// 用户查询作为 user message 传递，不混入 system prompt。
/// 这样 system prompt 可以 100% 被前缀缓存。
fn build_system_prompt() -> Prompt {
    Prompt::new()
        // L1 — 核心身份（永不变化）
        .stable("你是天气查询助手。")
        // L2 — 工具使用指南（极少变化）
        .stable(
            "流程：
1. 提取用户输入中的所有地址
2. 对每个地址调用 resolve_city
3. 对 city_en != \"unknown\" 调用 http_get(https://wttr.in/{city_en}?format=%c+%t+%h+%w)
4. 解析 wttr.in 返回的文本，提取天气数据，输出 JSON",
        )
        // L3 — 字段转换规则（极少变化）
        .stable(
            "wttr.in 返回格式: \"🌧️ +17°C 94% ↖11km/h\"
你需要转换以下字段：

1. condition（emoji → 中文）：
   - 🌧️/🌦️/🌧 → 小雨/中雨/大雨
   - ☀️/🌤 → 晴/多云
   - 其他 emoji 自行翻译为对应的中文天气描述

2. temperature（格式修正）：
   - \"+23°C\" → \"23°C\"（去掉 + 号）
   - \"-5°C\" → \"零下5°C\"（负数加\"零下\"）

3. wind（方向箭头 → 中文）：
   - \"→\" → \"东风\", \"←\" → \"西风\", \"↑\" → \"南风\", \"↓\" → \"北风\"
   - \"↗\" → \"东南风\", \"↘\" → \"西南风\", \"↙\" → \"西北风\", \"↖\" → \"东北风\"
   - \"↖11km/h\" → \"东北风11km/h\"
   - 无箭头（如 \"7km/h\"）→ 保持原样",
        )
        // L4 — 输出格式规则（极少变化，最后一个 stable → 获得断点 ✓）
        .stable(
            "输出格式（纯紧凑JSON，禁止任何解释文字）：
单地址: {\"city\":\"tokyo\",\"address\":\"新宿\",\"condition\":\"小雨\",\"temperature\":\"17°C\",\"humidity\":\"94%\",\"wind\":\"东风7km/h\"}
多地址: [{...},{...}]

最终回答必须为纯 JSON，不要包含 markdown 代码块标记或任何解释",
        )
}

// ─── Agent 工厂 ─────────────────────────────────────────────────

fn create_agent(provider: CodecProvider<OpenAICompatCodec>) -> ToolUseLoop {
    // 共享 provider：主 Agent Loop + resolve_city 第四级降级各持一份 Arc
    let shared_provider: Arc<dyn LlmProvider> = Arc::new(provider);

    AgentBuilder::new(ResolvedModel {
        provider: shared_provider.clone(),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system(build_system_prompt())
    .tools(register_weather_tools(Some(shared_provider)))
    // ToolCachePolicy::Auto（默认）— 工具定义自动获得 cache_control 断点
    .max_iterations(10)
    .max_output_tokens(8000)
    //.reasoning(lellm_core::ReasoningConfig::Disabled)
    .compile()
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=trace,lellm_provider=trace,info".into()),
        )
        .try_init();

    let provider =
        CodecProvider::load(OpenAICompatCodec::llama()).expect("LLaMA provider env error");

    println!("=== Weather Agent — resolve_city(四级降级) + http_get ===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    // System prompt 全部缓存（无动态层），查询通过 user message 传递
    let agent = create_agent(provider);

    let stream = agent.invoke_stream(vec![Message::user(text_block(question.clone()))]);
    shared::observe_react_loop(stream, &question).await
}
