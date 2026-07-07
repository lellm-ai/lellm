//! MCP Tencent Map Server — 本地 MCP 服务器，封装腾讯地图逆地理编码 API
//!
//! 暴露 `resolve_city` 工具：输入地址，返回城市信息（kebab-case 城市名）。
//! 逻辑参考 `lellm-agent/examples/_shared/city_resolver.rs` 中的 `resolve_via_tencent_map`。
//!
//! 前置条件：
//! 1. 在腾讯位置服务申请 API Key: https://lbs.qq.com/service/webService/webServiceGuide/overview
//! 2. 设置环境变量: export TENCENT_MAP_KEY=your_api_key
//!
//! 运行：
//! ```bash
//! TENCENT_MAP_KEY=your_api_key cargo run --example mcp_tencent_map_server --features server -p lellm-mcp
//! # 默认监听 0.0.0.0:3100
//! ```

use lellm_mcp::SimpleMcp;

/// 城市拼音映射表 — 常见中文城市名 → wttr.in 可用的 kebab-case 拼音
/// 用于补充腾讯地图返回的城市名转换。
const CITY_PINYIN_MAP: [(&str, &str); 29] = [
    ("上海", "shanghai"),
    ("北京", "beijing"),
    ("广州", "guangzhou"),
    ("深圳", "shenzhen"),
    ("杭州", "hangzhou"),
    ("成都", "chengdu"),
    ("重庆", "chongqing"),
    ("武汉", "wuhan"),
    ("南京", "nanjing"),
    ("西安", "xian"),
    ("青岛", "qingdao"),
    ("大连", "dalian"),
    ("苏州", "suzhou"),
    ("无锡", "wuxi"),
    ("宁波", "ningbo"),
    ("厦门", "xiamen"),
    ("福州", "fuzhou"),
    ("长沙", "changsha"),
    ("郑州", "zhengzhou"),
    ("济南", "jinan"),
    ("沈阳", "shenyang"),
    ("哈尔滨", "harbin"),
    ("长春", "changchun"),
    ("天津", "tianjin"),
    ("合肥", "hefei"),
    ("南昌", "nanchang"),
    ("昆明", "kunming"),
    ("贵阳", "guiyang"),
    ("兰州", "lanzhou"),
];

/// 将中文城市名转为 wttr.in 可用的 kebab-case 拼音。
/// 1. 查找映射表
/// 2. 回退：ASCII kebab-case 转换
fn to_kebab(city: &str) -> String {
    if let Some(&(_, pinyin)) = CITY_PINYIN_MAP.iter().find(|(key, _)| *key == city) {
        return pinyin.to_string();
    }
    // 回退：将非字母数字字符替换为 -，转小写
    city.to_lowercase()
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let api_key = std::env::var("TENCENT_MAP_KEY").expect("请设置环境变量 TENCENT_MAP_KEY");

    let mut mcp = SimpleMcp::new("Tencent Map Server");

    // 注册 resolve_city 工具
    mcp.tool(
        "resolve_city",
        "将中文地址解析为城市信息。输入地址字符串，返回 wttr.in 可用的 kebab-case 城市名、原始城市名和省份。",
        {
            let key = api_key.clone();
            move |args: serde_json::Value| {
                let address = args["address"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let api_key = key.clone();

                async move {
                    let addr = urlencoding::encode(&address);
                    let url = format!(
                        "https://apis.map.qq.com/ws/geocoder/v1/?address={}&key={}&output=json",
                        addr, api_key
                    );

                    tracing::debug!(url = %url, "calling tencent map api");

                    let resp = match reqwest::get(&url).await {
                        Ok(r) => r,
                        Err(e) => {
                            return Ok(serde_json::json!({
                                "city_en": "unknown",
                                "city": address,
                                "province": "",
                                "source": "error",
                                "error": e.to_string()
                            }));
                        }
                    };

                    let body: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            return Ok(serde_json::json!({
                                "city_en": "unknown",
                                "city": address,
                                "province": "",
                                "source": "error",
                                "error": format!("JSON parse error: {}", e)
                            }));
                        }
                    };

                    let status = body.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
                    if status != 0 {
                        let msg = body
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        return Ok(serde_json::json!({
                            "city_en": "unknown",
                            "city": address,
                            "province": "",
                            "source": "error",
                            "error": format!("API status {}: {}", status, msg)
                        }));
                    }

                    let result = match body.get("result") {
                        Some(r) => r,
                        None => {
                            return Ok(serde_json::json!({
                                "city_en": "unknown",
                                "city": address,
                                "province": "",
                                "source": "error",
                                "error": "missing result field"
                            }));
                        }
                    };

                    let components = result.get("address_components").cloned().unwrap_or_else(|| serde_json::json!({}));

                    let city_raw = components
                        .get("city")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let province = components
                        .get("province")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let city_en = to_kebab(city_raw);

                    Ok(serde_json::json!({
                        "city_en": city_en,
                        "city": city_raw,
                        "province": province,
                        "source": "tencent"
                    }))
                }
            }
        },
    );

    let port: u16 = std::env::var("MCP_SERVER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3100);

    println!("=== Tencent Map MCP Server ===");
    println!("Listening on 0.0.0.0:{}", port);
    println!("Tools: resolve_city");
    println!();

    mcp.run_http(port).await?;

    Ok(())
}
