//! 城市解析器 — resolve_city 四级降级逻辑。
//!
//! ```text
//! 第一级: 本地别名表 (O(1), <1ms)
//!     ↓ miss
//! 第二级: 腾讯地图逆地理编码 (需 TENCENT_MAP_KEY)
//!     ↓ miss / 限流
//! 第三级: LLM 轻量推理 (temperature=0, max_tokens=1000)
//!     ↓ 无法确定
//! 第四级: "unknown"
//! ```

mod city_data;

use std::sync::Arc;

use lellm_core::{ChatRequest, text_block};
use lellm_provider::LlmProvider;

// ─── 结果结构 ───────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct CityResult {
    pub city_en: String,  // wttr.in 城市名 (kebab-case)，未知 → "unknown"
    pub source: String,   // alias / tencent / llm / unknown
    pub address: String,  // 原始请求地址
}

// ─── 第二级：腾讯地图 ───────────────────────────────────────────

/// 将地址字符串转为 Weather API 可用的 kebab-case 城市名。
///
/// 匹配优先级：
/// 1. **全匹配** — 输入精确等于映射 key
/// 2. **前缀匹配** — 输入以映射 key 开头（如 "昌吉市天气" → "changji"）
/// 3. **kebab 回退** — 对 ASCII 字符走原有 kebab-case 逻辑
pub fn to_kebab(s: &str) -> String {
    // 1. 全匹配
    if let Some(&(_, pinyin)) = city_data::CITY_PINYIN_MAP.iter().find(|(key, _)| *key == s) {
        return pinyin.to_string();
    }
    // 2. 前缀匹配（map 已按长度降序排列，优先命中更长 key）
    if let Some(longest) = city_data::CITY_PINYIN_MAP
        .iter()
        .filter(|(key, _)| s.starts_with(*key))
        .map(|(key, pinyin)| (key.len(), pinyin))
        .max_by_key(|&(len, _)| len)
    {
        return longest.1.to_string();
    }
    // 3. kebab-case 回退（适用于已为拼音/英文的输入）
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

/// 第二级降级：腾讯地图逆地理编码 → 提取 city 字段 → to_kebab()
fn resolve_via_tencent_map(address: &str) -> Option<CityResult> {
    let api_key = match std::env::var("TENCENT_MAP_KEY") {
        Ok(ak) => ak,
        Err(_) => return None,
    };
    let url = format!(
        "https://apis.map.qq.com/ws/geocoder/v1/?address={}&key={}&output=json",
        urlencoding::encode(address),
        api_key
    );

    tracing::debug!(address, url, "calling tencent map api");

    let resp = match reqwest::blocking::Client::new().get(&url).send() {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(address, error = %e, "tencent map api request failed");
            return None;
        }
    };

    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(address, error = %e, "tencent map api json parse failed");
            return None;
        }
    };

    let status = body.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
    if status != 0 {
        tracing::debug!(
            address,
            status,
            msg = ?body.get("message").and_then(|v| v.as_str()),
            "tencent map api returned error status"
        );
        return None;
    }

    let result = match body.get("result") {
        Some(r) => r,
        None => {
            tracing::debug!(address, "tencent map api missing result");
            return None;
        }
    };
    let address_components = match result.get("address_components") {
        Some(r) => r,
        None => {
            tracing::debug!(address, "tencent map api missing address_components");
            return None;
        }
    };

    // 优先取 city（地级市）
    let city_raw = address_components.get("city").and_then(|v| v.as_str());
    tracing::debug!(address, city_raw, "tencent map api city field");

    let city_en = city_raw.map(|s| to_kebab(s)).unwrap_or_default();
    tracing::debug!(address, city_en, "tencent map api resolved city_en");

    Some(CityResult {
        city_en,
        source: "tencent".to_string(),
        address: address.to_string(),
    })
}

// ─── 第四级：LLM 轻量推理 ───────────────────────────────────────

/// 从 LLM 响应文本中查找 JSON 并提取 city 字段。
///
/// 策略：总是取**最后一个** `"city"` 的值。
/// 因为推理文本中可能包含 prompt 示例（如参考列表），
/// 而模型的实际答案通常在最后输出。
pub fn parse_city_from_json(text: &str) -> Option<String> {
    // 尝试直接解析整个文本
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(city) = val.get("city").and_then(|c| c.as_str()) {
            let city = city.trim().to_string();
            if !city.is_empty() {
                return Some(city);
            }
        }
    }

    // 查找最后一个 "city" 键后的字符串值（逆向搜索，取最后一个匹配）
    if let Some(last_pos) = text.rfind("\"city\"") {
        let after = &text[last_pos + 6..];
        let after = after.trim_start_matches(':').trim_start();
        if let Some(quote) = after.find('"') {
            let after_quote = &after[quote + 1..];
            if let Some(end_quote) = after_quote.find('"') {
                let city = after_quote[..end_quote].trim();
                if !city.is_empty() {
                    return Some(city.to_string());
                }
            }
        }
    }

    None
}

/// 第四级降级：用 LLM 轻量推理地址所属城市。
///
/// 要求 LLM 输出 JSON `{"city":"xxx"}`，结构化解析（rfind 取最后一个 city 值）。
/// max_tokens=1000，推理模型需要足够 tokens 完成思考。
/// 仅当前三级（别名表 + 地图 API）均 miss 时触发。
pub async fn resolve_via_llm(provider: &Arc<dyn LlmProvider>, address: &str) -> Option<CityResult> {
    let prompt = format!(
        "地址「{0}」属于哪个城市？\n\
         输出 wttr.in 可用的城市英文名（全小写，多词用连字符，如 new-york）。\n\
         只输出紧凑的 JSON：{{\"city\": \"城市英文名\"}}，不确定则 {{\"city\": \"unknown\"}}",
        address
    );

    let req = ChatRequest {
        model: "Qwen3.6".to_string(),
        messages: vec![lellm_core::Message::User {
            content: text_block(prompt),
        }],
        tools: None,
        temperature: Some(0.0),
        max_tokens: Some(1000),
        ..Default::default()
    };

    let resp = provider.call(&req).await.ok()?;

    // 从响应中提取全部文本
    let all_text: String = resp
        .content
        .iter()
        .filter_map(|b| {
            if let lellm_core::ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if all_text.is_empty() {
        tracing::debug!("resolve_via_llm: no text in response");
        return None;
    }

    // 从文本中查找 JSON 对象并解析
    let city_text = parse_city_from_json(&all_text)?;
    let city_text = city_text.to_lowercase();

    if city_text.is_empty() || city_text == "unknown" {
        tracing::debug!(city = %city_text, "resolve_via_llm: filtered out");
        return None;
    }

    tracing::debug!(city = %city_text, address = %address, "resolve_via_llm: success");

    Some(CityResult {
        city_en: city_text,
        source: "llm".to_string(),
        address: address.to_string(),
    })
}

// ─── 入口函数 ───────────────────────────────────────────────────

/// resolve_city 四级降级入口。
///
/// 第一、二级为同步阻塞（alias + 腾讯地图），在 `spawn_blocking` 中执行。
/// 第三级（LLM）由调用方异步执行。
pub fn resolve_city(address: &str) -> CityResult {
    if let Some(&city_en) = city_data::CITY_ALIASES.get(address) {
        return CityResult {
            city_en: city_en.to_string(),
            source: "alias".to_string(),
            address: address.to_string(),
        };
    }
    if let Some(result) = resolve_via_tencent_map(address) {
        return result;
    }
    CityResult {
        city_en: "unknown".to_string(),
        source: "unknown".to_string(),
        address: address.to_string(),
    }
}
