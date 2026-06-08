//! weather_agent — 天气查询链
//!
//! 工具链：`resolve_city(address) → http_get(wttr.in) → LLM 解析为 JSON`
//!
//! resolve_city 三级降级：本地别名表(O(1)) → Nominatim → unknown
//! 工具层不硬编码业务 API，仅提供通用 `http_get`。LLM 自行构造 URL 并解析响应。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example weather_agent [地址]
//! ```

#[path = "_shared/city_aliases.rs"]
mod city_aliases;
#[path = "_shared/shared.rs"]
mod shared;

use city_aliases::CITY_ALIASES;
use lellm_agent::{AgentBuilder, ToolArgs, ToolRegistration, ToolUseLoop, schemars::JsonSchema};
use lellm_core::{Message, ToolError, ToolErrorKind, text_block};
use lellm_macros::ToolDefinition;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::GenericProvider;
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::sync::Arc;

// ─── Tool 1: resolve_city ───────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
struct CityResult {
    city_en: String, // wttr.in 城市名 (kebab-case)，未知 → "unknown"
    source: String,  // alias / nominatim / unknown
}

#[derive(JsonSchema, ToolDefinition)]
#[tool(
    name = "resolve_city",
    description = "将地址解析为 wttr.in 城市英文名。三级降级：别名表 → Nominatim → unknown。始终调用此工具，不要猜测。"
)]
#[allow(dead_code)]
struct ResolveCityArgs {
    /// 地址或地名（如 "浦东"、"新宿"、"曼哈顿"）
    address: String,
}

fn resolve_city(address: &str) -> CityResult {
    if let Some(&city_en) = CITY_ALIASES.get(address) {
        return CityResult {
            city_en: city_en.to_string(),
            source: "alias".to_string(),
        };
    }
    if let Some(result) = resolve_via_nominatim(address) {
        return result;
    }
    CityResult {
        city_en: "unknown".to_string(),
        source: "unknown".to_string(),
    }
}

fn resolve_via_nominatim(address: &str) -> Option<CityResult> {
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=1&addressdetails=1",
        url_encode(address)
    );
    let resp = reqwest::blocking::Client::builder()
        .user_agent("LeLLM-WeatherAgent/0.1")
        .build()
        .ok()?
        .get(&url)
        .header("Accept-Language", "zh-CN")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bodies: Vec<serde_json::Value> = resp.json().ok()?;
    let first = bodies.first()?;
    let addr = first.get("address")?;
    let city_en = addr
        .get("city")
        .or_else(|| addr.get("town"))
        .or_else(|| addr.get("county"))
        .or_else(|| addr.get("village"))
        .and_then(|v| v.as_str())
        .map(|s| to_kebab(s))
        .or_else(|| {
            first
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(|name| {
                    name.split(',')
                        .next()
                        .unwrap_or("")
                        .split(' ')
                        .filter(|w| !w.is_empty())
                        .collect::<Vec<_>>()
                        .join("-")
                })
        })
        .unwrap_or_default();
    Some(CityResult {
        city_en,
        source: "nominatim".to_string(),
    })
}

fn url_encode(s: &str) -> String {
    let mut r = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                r.push(b as char);
            }
            _ => r.push_str(&format!("%{:02X}", b)),
        }
    }
    r
}

fn to_kebab(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ─── Tool 2: http_get ───────────────────────────────────────────

#[derive(JsonSchema, ToolDefinition)]
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

fn register_weather_tools() -> Vec<ToolRegistration> {
    vec![
        ToolRegistration::safe(ResolveCityArgs::tool_definition(), |args| {
            let address = args
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    serde_json::to_string(&resolve_city(&address)).map_err(|e| ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("序列化失败: {e}"),
                    })
                })
                .await
                .map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("任务失败: {e}"),
                })?;
                result
            }
        }),
        ToolRegistration::safe(HttpGetArgs::tool_definition(), |args| {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                tokio::task::spawn_blocking(move || http_get(&url))
                    .await
                    .map_err(|e| ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("任务失败: {e}"),
                    })?
            }
        }),
    ]
}

// ─── Agent 工厂 ─────────────────────────────────────────────────

fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let prompt = r#"你是天气查询助手。

流程：
1. 提取用户输入中的所有地址
2. 对每个地址调用 resolve_city
3. 对 city_en != "unknown" 调用 http_get(https://wttr.in/{city_en}?format=%c+%t+%h+%w)
4. 输出 JSON

wttr.in 返回格式: "小雨 17°C 94% 7km/h"

输出格式（纯 JSON，禁止解释）：
单地址: {"city":"tokyo","city_source":"新宿","condition":"小雨","temperature":"17°C","humidity":"94%","wind":"7km/h"}
多地址: [{"city":"tokyo","city_source":"新宿",...},{"city":"new-york","city_source":"曼哈顿",...}]

规则：
- 地址推理交给 resolve_city，不要猜测
- unknown 城市跳过天气查询
- 最终回答必须为纯 JSON"#;

    AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system_prompt(prompt.to_string())
    .tools(register_weather_tools())
    .max_iterations(10)
    .max_output_tokens(2000)
    .build()
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=debug,lellm_provider=debug,info".into()),
        )
        .try_init();

    let provider = GenericProvider::from_env(OpenAICompatAdapter::openai())
        .expect("OpenAI provider env error");
    let agent = create_agent(provider);

    println!("=== Weather Agent — resolve_city + http_get ===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    shared::observe_react_loop(stream, &question).await
}
