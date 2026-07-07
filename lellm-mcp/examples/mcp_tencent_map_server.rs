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
//! # HTTP 模式 (默认)
//! TENCENT_MAP_KEY=your_api_key cargo run --example mcp_tencent_map_server --features server -p lellm-mcp
//!
//! # SSE 模式
//! TENCENT_MAP_KEY=your_api_key MCP_TRANSPORT=sse cargo run --example mcp_tencent_map_server --features server -p lellm-mcp
//!
//! # 默认监听 0.0.0.0:3100
//! # HTTP 模式: POST /mcp
//! # SSE 模式: GET /sse + POST /messages/{session_id}
//! ```

use lellm_mcp::SimpleMcp;

/// 城市拼音映射表 — 中文城市名 → wttr.in 可用的 kebab-case 拼音
const CITY_PINYIN_MAP: [(&str, &str); 30] = [
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
    ("昌吉", "changji"),
];

/// POI 别名表 — 常见地标/区域 → wttr.in 城市名
/// 腾讯地图 Geocoder API 对 POI 名称返回 348 错误，需要本地回退。
const POI_ALIASES: [(&str, &str); 54] = [
    // 上海
    ("陆家嘴", "shanghai"),
    ("外滩", "shanghai"),
    ("静安", "shanghai"),
    ("南京路", "shanghai"),
    ("虹桥", "shanghai"),
    ("松江", "shanghai"),
    ("闵行", "shanghai"),
    ("徐汇", "shanghai"),
    ("黄浦", "shanghai"),
    ("浦东", "shanghai"),
    // 北京
    ("朝阳", "beijing"),
    ("国贸", "beijing"),
    ("三里屯", "beijing"),
    ("中关村", "beijing"),
    ("西单", "beijing"),
    ("王府井", "beijing"),
    ("海淀", "beijing"),
    ("望京", "beijing"),
    ("亦庄", "beijing"),
    ("天安门", "beijing"),
    // 广州
    ("天河", "guangzhou"),
    ("珠江新城", "guangzhou"),
    ("白云", "guangzhou"),
    ("越秀", "guangzhou"),
    ("海珠", "guangzhou"),
    ("琶洲", "guangzhou"),
    ("北京路", "guangzhou"),
    // 深圳
    ("南山", "shenzhen"),
    ("福田", "shenzhen"),
    ("罗湖", "shenzhen"),
    ("宝安", "shenzhen"),
    ("龙华", "shenzhen"),
    ("前海", "shenzhen"),
    ("科技园", "shenzhen"),
    ("华强北", "shenzhen"),
    // 杭州
    ("西湖", "hangzhou"),
    ("滨江", "hangzhou"),
    ("余杭", "hangzhou"),
    ("未来科技城", "hangzhou"),
    ("钱江新城", "hangzhou"),
    ("武林广场", "hangzhou"),
    // 成都
    ("春熙路", "chengdu"),
    ("太古里", "chengdu"),
    ("宽窄巷子", "chengdu"),
    ("锦里", "chengdu"),
    ("武侯", "chengdu"),
    ("玉林", "chengdu"),
    // 国外常见
    ("新宿", "tokyo"),
    ("东京", "tokyo"),
    ("涩谷", "tokyo"),
    ("大阪", "osaka"),
    ("京都", "kyoto"),
    ("首尔", "seoul"),
    ("曼谷", "bangkok"),
];

/// 将中文城市名转为 wttr.in 可用的 kebab-case 拼音。
/// 1. 精确匹配映射表
/// 2. 前缀匹配映射表（按 key 长度降序，优先匹配更长 key）
/// 3. 回退：ASCII kebab-case 转换
fn to_kebab(city: &str) -> String {
    // 1. 精确匹配
    if let Some(&(_, pinyin)) = CITY_PINYIN_MAP.iter().find(|(key, _)| *key == city) {
        return pinyin.to_string();
    }
    // 2. 前缀匹配（按长度降序，"广州" 匹配 "广州市"）
    if let Some(longest) = CITY_PINYIN_MAP
        .iter()
        .filter(|(key, _)| city.starts_with(*key) && key.len() >= 2)
        .max_by_key(|(key, _)| key.len())
    {
        return longest.1.to_string();
    }
    // 3. 回退：ASCII kebab-case
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
    mcp.tool_with_schema(
        "resolve_city",
        "将中文地址解析为城市信息。输入地址字符串，返回 wttr.in 可用的 kebab-case 城市名、原始城市名和省份。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "string",
                    "description": "要解析的中文地址，例如「陆家嘴」、「天安门」"
                }
            },
            "required": ["address"]
        }),
        {
            let key = api_key.clone();
            move |args: serde_json::Value| {
                let address = args["address"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let api_key = key.clone();

                async move {
                    // 第一级：本地 POI 别名表（前缀匹配，长优先）
                    if let Some(&(_, city_en)) = POI_ALIASES.iter().find(|(key, _)| *key == &address)
                    {
                        return Ok(serde_json::json!({
                            "city_en": city_en,
                            "city": address,
                            "province": "",
                            "source": "alias"
                        }));
                    }
                    if let Some(longest) = POI_ALIASES
                        .iter()
                        .filter(|(key, _)| address.starts_with(*key))
                        .max_by_key(|(key, _)| key.len())
                    {
                        return Ok(serde_json::json!({
                            "city_en": longest.1,
                            "city": address,
                            "province": "",
                            "source": "alias"
                        }));
                    }

                    // 第二级：腾讯地图 Geocoder API
                    let addr = urlencoding::encode(&address);
                    let url = format!(
                        "https://apis.map.qq.com/ws/geocoder/v1/?address={}&key={}&output=json",
                        addr, api_key
                    );

                    tracing::debug!(url = %url, "calling tencent map api");

                    if let Ok(resp) = reqwest::get(&url).await {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            let status = body.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
                            if status == 0 {
                                if let Some(result) = body.get("result") {
                                    let components = result.get("address_components").cloned().unwrap_or_else(|| serde_json::json!({}));

                                    let city_raw = components.get("city").and_then(|v| v.as_str()).unwrap_or("");
                                    let province = components.get("province").and_then(|v| v.as_str()).unwrap_or("");
                                    let city_en = to_kebab(city_raw);

                                    return Ok(serde_json::json!({
                                        "city_en": city_en,
                                        "city": city_raw,
                                        "province": province,
                                        "source": "tencent"
                                    }));
                                }
                            } else {
                                tracing::debug!(status, "tencent map api error");
                            }
                        }
                    }

                    // 第三级：API 失败/不可用，回退到 kebab-case 转换
                    let city_en = to_kebab(&address);
                    Ok(serde_json::json!({
                        "city_en": city_en,
                        "city": address,
                        "province": "",
                        "source": "fallback"
                    }))
                }
            }
        },
    );

    let port: u16 = std::env::var("MCP_SERVER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3100);

    // 根据环境变量选择传输模式
    let transport = std::env::var("MCP_TRANSPORT")
        .ok()
        .unwrap_or_else(|| "http".to_string());

    println!("=== Tencent Map MCP Server ===");
    println!("Listening on 0.0.0.0:{}", port);
    println!("Tools: resolve_city");

    match transport.as_str() {
        "sse" => {
            println!("Mode: SSE (GET /sse, POST /messages/{{session_id}})");
            println!();
            mcp.run_sse(port).await?;
        }
        _ => {
            println!("Mode: HTTP (POST /mcp)");
            println!();
            mcp.run_http(port).await?;
        }
    }

    Ok(())
}
