//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 包含两个示例场景：
//! 1. **产品搜索链**：`search_products` → `check_inventory`（模拟数据）
//! 2. **天气查询链**：`resolve_city` → `fetch_weather`（真实 wttr.in API）
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//! from langchain.agents import create_agent
//!
//! @tool
//! def resolve_city(address: str) -> str:
//!     """从地址中提取城市名称并转为拼音。"""
//!     return {"city": "上海", "pinyin": "shanghai"}
//!
//! @tool
//! def fetch_weather(city_pinyin: str) -> str:
//!     """调用 wttr.in 获取城市天气。"""
//!     return curl("-s", f"wttr.in/{city_pinyin}?format=%c+%t+%h+%w")
//!
//! agent = create_agent(model, tools=[resolve_city, fetch_weather])
//! result = agent.invoke("帮我查一下浦东新区的天气")
//! ```
//!
//! 智能体遵循 ReAct（推理 + 行动）模式，在推理步骤与工具调用之间交替，
//! 并将结果观察反馈到后续决策中，直到能够提供最终答案。
//!
//! 每一步都清晰可观测：
//! - 人类消息 → AI 消息（工具调用） → 工具观察 → AI 消息（工具调用） → ... → 最终答案
//! - 工具执行错误会以 "工具错误" 形式展示，不中断循环
//! - Provider API 错误会以 "API 错误" 形式展示
//!
//! 运行（需设置环境变量）：
//! ```text
//! OPENAI_BASE_URL=https://api.openai.com/v1 OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::{AgentBuilder, AgentEvent, ToolArgs, ToolRegistration, ToolUseLoop};
use lellm_core::{ToolError, ToolErrorKind};
use lellm_macros::ToolDefinition as ToolDefinitionDerive;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::collections::HashMap;
use std::sync::Arc;

// ─── 工具定义 ───────────────────────────────────────────────────

/// 从地址中解析城市
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "resolve_city",
    description = "从中文地址中提取地级市名称，并转换为拼音。输入可以是街道、乡镇、县、区等地址。返回城市中文名和拼音。"
)]
struct ResolveCityArgs {
    /// 中文地址，例如 "上海市浦东新区"、"北京市朝阳区"、"广州市天河区"
    address: String,
}

/// 获取城市天气
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "fetch_weather",
    description = "调用 wttr.in API 获取指定城市的实时天气情况。返回 JSON 结构化数据，包含天气状况、温度、湿度、风速。城市名称使用拼音，例如 'shanghai'、'beijing'。"
)]
struct FetchWeatherArgs {
    /// 城市拼音名称，例如 "shanghai"、"beijing"、"guangzhou"
    city_pinyin: String,
}

/// 搜索产品
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "search_products",
    description = "搜索产品目录，返回匹配的产品列表"
)]
struct SearchProductsArgs {
    /// 搜索关键词
    query: String,
}

/// 检查库存
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "check_inventory", description = "检查指定产品的库存数量")]
struct CheckInventoryArgs {
    /// 产品 ID
    product_id: String,
}

// ─── 常用城市拼音映射表 ──────────────────────────────────────────

/// 常见中国城市名 → 拼音映射（覆盖主要地级市）
fn city_to_pinyin(city: &str) -> Option<&'static str> {
    // 使用静态 HashMap 避免重复构建
    use std::sync::OnceLock;
    static CITY_MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

    CITY_MAP
        .get_or_init(|| {
            let mut m = HashMap::new();
            // 直辖市
            m.insert("北京", "beijing");
            m.insert("上海", "shanghai");
            m.insert("天津", "tianjin");
            m.insert("重庆", "chongqing");
            // 广东省
            m.insert("广州", "guangzhou");
            m.insert("深圳", "shenzhen");
            m.insert("东莞", "dongguan");
            m.insert("佛山", "foshan");
            m.insert("珠海", "zhuhai");
            m.insert("中山", "zhongshan");
            // 江苏省
            m.insert("南京", "nanjing");
            m.insert("苏州", "suzhou");
            m.insert("无锡", "wuxi");
            m.insert("常州", "changzhou");
            m.insert("徐州", "xuzhou");
            // 浙江省
            m.insert("杭州", "hangzhou");
            m.insert("宁波", "ningbo");
            m.insert("温州", "wenzhou");
            m.insert("绍兴", "shaoxing");
            // 四川省
            m.insert("成都", "chengdu");
            m.insert("绵阳", "mianyang");
            m.insert("德阳", "deyang");
            // 湖北省
            m.insert("武汉", "wuhan");
            m.insert("宜昌", "yichang");
            m.insert("襄阳", "xiangyang");
            // 湖南省
            m.insert("长沙", "changsha");
            m.insert("株洲", "zhuzhou");
            m.insert("湘潭", "xiangtan");
            // 福建省
            m.insert("福州", "fuzhou");
            m.insert("厦门", "xiamen");
            m.insert("泉州", "quanzhou");
            // 山东省
            m.insert("济南", "jinan");
            m.insert("青岛", "qingdao");
            m.insert("烟台", "yantai");
            // 河南省
            m.insert("郑州", "zhengzhou");
            m.insert("洛阳", "luoyang");
            m.insert("开封", "kaifeng");
            // 陕西省
            m.insert("西安", "xian");
            m.insert("咸阳", "xianyang");
            // 河北省
            m.insert("石家庄", "shijiazhuang");
            m.insert("唐山", "tangshan");
            // 辽宁省
            m.insert("沈阳", "shenyang");
            m.insert("大连", "dalian");
            // 黑龙江省
            m.insert("哈尔滨", "haerbin");
            // 云南省
            m.insert("昆明", "kunming");
            m.insert("大理", "dali");
            // 贵州省
            m.insert("贵阳", "guiyang");
            // 甘肃省
            m.insert("兰州", "lanzhou");
            // 青海省
            m.insert("西宁", "xining");
            // 宁夏
            m.insert("银川", "yinchuan");
            // 内蒙古
            m.insert("呼和浩特", "huhehaote");
            // 山西省
            m.insert("太原", "taiyuan");
            // 安徽省
            m.insert("合肥", "hefei");
            m.insert("芜湖", "wuhu");
            // 江西省
            m.insert("南昌", "nanchang");
            m.insert("九江", "jiujiang");
            // 吉林省
            m.insert("长春", "changchun");
            m.insert("吉林", "jilin");
            // 新疆
            m.insert("乌鲁木齐", "wulumuqi");
            // 西藏
            m.insert("拉萨", "lasa");
            // 海南省
            m.insert("海口", "haikou");
            m
        })
        .get(city)
        .copied()
}

/// 从地址中提取城市名称。
/// 策略：先匹配长名称（如"石家庄市"），再匹配短名称。
fn extract_city_from_address(address: &str) -> Option<&str> {
    use std::sync::OnceLock;
    static CITIES: OnceLock<Vec<&'static str>> = OnceLock::new();

    let cities = CITIES.get_or_init(|| {
        vec![
            "石家庄市", "唐山市", "哈尔滨市", "呼和浩特市", "乌鲁木齐",
            "沈阳市", "大连市", "长春市", "吉林省", "济南市", "青岛市",
            "北京市", "上海市", "天津市", "重庆市",
            "广州市", "深圳市", "东莞市", "佛山市", "珠海市", "中山市",
            "南京市", "苏州市", "无锡市", "常州市", "徐州市",
            "杭州市", "宁波市", "温州市", "绍兴市",
            "成都市", "绵阳市", "德阳市",
            "武汉市", "宜昌市", "襄阳市",
            "长沙市", "株洲市", "湘潭市",
            "福州市", "厦门市", "泉州市",
            "郑州市", "洛阳市", "开封市",
            "西安市", "咸阳市",
            "昆明市", "大理市",
            "贵阳市", "兰州市", "西宁市", "银川市", "拉萨市", "海口市",
            "合肥市", "芜湖市",
            "南昌市", "九江市",
            "烟台市", "沈阳市",
            "长沙市", "株洲市",
        ]
    });

    // 按长度降序匹配，优先匹配长名称
    let mut cities_sorted: Vec<_> = cities.clone();
    cities_sorted.sort_by_key(|c| std::cmp::Reverse(c.len()));

    for city in &cities_sorted {
        if address.contains(city) {
            // 去掉后缀 "市"
            return Some(city.strip_suffix("市").unwrap_or(city));
        }
    }

    // 尝试匹配不带"市"的城市名
    for city in cities {
        let city_name = city.strip_suffix("市").unwrap_or(city);
        if address.contains(city_name) {
            return Some(city_name);
        }
    }

    None
}

// ─── 天气 API 响应结构 ──────────────────────────────────────────

/// wttr.in 天气返回的结构化数据
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WeatherInfo {
    /// 城市名称（拼音）
    city: String,
    /// 天气状况，如 "小雨"、"晴"、"多云"
    condition: String,
    /// 温度，如 "17°C"
    temperature: String,
    /// 湿度，如 "94%"
    humidity: String,
    /// 风速，如 "7km/h"
    wind_speed: String,
}

/// 调用 wttr.in API 获取天气
fn fetch_weather_from_wttr(city_pinyin: &str) -> Result<WeatherInfo, ToolError> {
    let url = format!(
        "https://wttr.in/{}?format=%c+%t+%h+%w",
        city_pinyin
    );

    let response = reqwest::blocking::get(url)
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Network,
            message: format!("请求 wttr.in 失败: {}", e),
        })?
        .text()
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("读取响应失败: {}", e),
        })?;

    // 解析 wttr.in 返回的文本，格式: "小雨 17°C 94% 7km/h"
    // 用空格分割，但中文和符号之间可能有特殊空格
    let parts: Vec<&str> = response.split_whitespace().collect();

    if parts.is_empty() {
        return Err(ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("wttr.in 返回空数据，城市 '{}' 可能不存在", city_pinyin),
        });
    }

    // 简单解析：第一部分天气，第二部分温度，第三部分湿度，第四部分风速
    let condition = parts.first().unwrap_or(&"").to_string();
    let temperature = parts.get(1).unwrap_or(&"").to_string();
    let humidity = parts.get(2).unwrap_or(&"").to_string();
    let wind_speed = parts.get(3).unwrap_or(&"").to_string();

    Ok(WeatherInfo {
        city: city_pinyin.to_string(),
        condition,
        temperature,
        humidity,
        wind_speed,
    })
}

// ─── 工具注册 ───────────────────────────────────────────────────

/// 注册天气查询链工具（真实 API）
fn register_weather_tools() -> Vec<ToolRegistration> {
    vec![
        ToolRegistration::safe(ResolveCityArgs::tool_definition(), |args| {
            let address = args
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let city = extract_city_from_address(&address).ok_or_else(|| ToolError {
                    kind: ToolErrorKind::InvalidInput,
                    message: format!("无法从地址 '{}' 中提取城市名称，请提供更明确的地址。", address),
                })?;

                let pinyin = city_to_pinyin(city).ok_or_else(|| ToolError {
                    kind: ToolErrorKind::NotFound,
                    message: format!("城市 '{}' 不在支持列表中，请确认城市名称。", city),
                })?;

                Ok(serde_json::json!({
                    "address": address,
                    "city": city,
                    "pinyin": pinyin
                }).to_string())
            }
        }),
        ToolRegistration::safe(FetchWeatherArgs::tool_definition(), |args| {
            let city_pinyin = args
                .get("city_pinyin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                // 使用 spawn_blocking 避免阻塞 tokio 异步运行时
                let join_result =
                    tokio::task::spawn_blocking(move || fetch_weather_from_wttr(&city_pinyin)).await;
                let weather = join_result.map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("任务执行失败: {}", e),
                })??;

                Ok(serde_json::json!({
                    "city": weather.city,
                    "condition": weather.condition,
                    "temperature": weather.temperature,
                    "humidity": weather.humidity,
                    "wind_speed": weather.wind_speed
                }).to_string())
            }
        }),
    ]
}

/// 注册产品搜索链工具（模拟数据）
fn register_product_tools() -> Vec<ToolRegistration> {
    vec![
        ToolRegistration::safe(SearchProductsArgs::tool_definition(), |args| {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let results = match query.to_lowercase().as_str() {
                    q if q.contains("wireless") || q.contains("耳机") => {
                        format!(
                            "找到 5 个匹配\"{}\"的产品：\n1. Sony WH-1000XM5 - 降噪无线耳机，评分 4.8\n2. Apple AirPods Pro - 真无线降噪耳机，评分 4.7\n3. Bose QuietComfort 45 - 降噪头戴式耳机，评分 4.6\n4. Sennheiser Momentum 4 - 无线头戴式耳机，评分 4.5\n5. JBL Tune 760NC - 预算友好型降噪耳机，评分 4.3",
                            query
                        )
                    }
                    _ => {
                        format!("搜索结果：{}\n找到 3 个相关结果，请查看详细信息。", query)
                    }
                };
                Ok(results)
            }
        }),
        ToolRegistration::safe(CheckInventoryArgs::tool_definition(), |args| {
            let product_id = args
                .get("product_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                match product_id.to_lowercase().as_str() {
                    pid if pid.contains("wh-1000xm5") => {
                        Ok("产品 WH-1000XM5：库存 10 件，预计明日发货".to_string())
                    }
                    pid if pid.contains("airpods") => {
                        Ok("产品 AirPods Pro：库存 25 件，预计今日发货".to_string())
                    }
                    _ => Err(ToolError {
                        kind: ToolErrorKind::NotFound,
                        message: format!("产品 {} 未找到。", product_id),
                    }),
                }
            }
        }),
    ]
}

// ─── 创建 Agent ─────────────────────────────────────────────────

/// 从环境变量创建真实 Provider
fn create_provider() -> GenericProvider<OpenAICompatAdapter> {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let api_key = std::env::var("OPENAI_API_KEY").expect("请设置 OPENAI_API_KEY 环境变量");
    let timeout = std::env::var("OPENAI_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    GenericProvider::new(
        OpenAICompatAdapter::openai(),
        ProviderConfig::bearer(&base_url, api_key)
            .expect("Invalid base URL")
            .with_timeout(std::time::Duration::from_secs(timeout)),
    )
}

/// 创建天气查询 Agent（真实 API 链）
fn create_weather_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个天气查询助手。\
             用户会告诉你一个地址，你需要先调用 resolve_city 解析出城市名称和拼音，\
             再调用 fetch_weather 获取该城市的天气信息。\
             最后用自然语言总结天气情况。"
                .to_string(),
        )
        .tools(register_weather_tools())
        .max_iterations(10)
        .build()
}

/// 创建产品搜索 Agent（模拟数据链）
fn create_product_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个有帮助的助手。你可以使用搜索产品、检查库存工具来帮助用户。\
             在回答前，请先使用工具获取所需信息，再给出最终答案。"
                .to_string(),
        )
        .tools(register_product_tools())
        .max_iterations(10)
        .build()
}

// ─── ReAct 循环观测器 ───────────────────────────────────────────

/// 当前 ReAct 轮次的中间状态。
#[derive(Debug, Default)]
struct RoundState {
    /// 本轮 LLM 输出的推理文本（Token 累积）
    reasoning: String,
    /// ResponseComplete 携带的工具调用
    pending_tool_calls: Vec<lellm_core::ToolCall>,
    /// 已收集的工具结果（按执行顺序）
    tool_observations: Vec<(String, Result<String, lellm_core::ToolError>)>,
    /// 当前正在执行的工具名称
    current_tool_name: Option<String>,
}

/// 以 LangChain ReAct 格式实时观测 Agent 执行过程。
///
/// 事件流顺序（由 `execute_stream` 保证）：
/// ```text
/// Provider(Start) → Provider(Token)* → Provider(ResponseComplete{tool_calls})
///   → [ToolStart → ToolEnd] * N  (N = tool_calls.len())
///   → Provider(Start) → ... (下一轮)
/// → LoopEnd | LoopError
/// ```
async fn observe_react_loop(
    mut stream: lellm_agent::AgentStream,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("================================ 人类消息 =================================");
    println!("{}", question);
    println!();

    let mut iteration: usize = 0;
    let mut round = RoundState::default();

    while let Some(event) = stream.recv().await {
        match event {
            // ─── Provider 事件 ───────────────────────────────────────
            AgentEvent::Provider(lellm_provider::ProviderEvent::Start { model }) => {
                iteration += 1;
                round = RoundState::default();
                eprintln!("[DEBUG] >>> 第 {} 轮 — 调用 {}", iteration, model);
            }

            AgentEvent::Provider(lellm_provider::ProviderEvent::Token { token }) => {
                round.reasoning.push_str(&token);
                print!("{}", token);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(
                lellm_provider::ProviderEvent::ThinkingDelta { thinking, .. },
            ) => {
                round.reasoning.push_str(&thinking);
                print!("[思考] {}", thinking);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(
                lellm_provider::ProviderEvent::ResponseComplete {
                    tool_calls,
                    usage,
                },
            ) => {
                round.pending_tool_calls = tool_calls;

                if round.pending_tool_calls.is_empty() {
                    eprintln!("\n[DEBUG] >>> 第 {} 轮 — 最终回答", iteration);
                    let _ = usage;
                } else {
                    println!();
                    println!(
                        "================================== AI 消息 =================================="
                    );
                    if !round.reasoning.is_empty() {
                        println!("推理: {}", round.reasoning);
                    }
                    println!("工具调用：");
                    for tc in &round.pending_tool_calls {
                        println!("  {} ({})", tc.name, tc.id);
                        println!("  参数: {}", tc.arguments);
                    }
                    println!();
                    eprintln!(
                        "[DEBUG] >>> 第 {} 轮 — {} 个工具调用",
                        iteration,
                        round.pending_tool_calls.len()
                    );
                }
            }

            // ─── 工具事件 ────────────────────────────────────────────
            AgentEvent::ToolStart { name, .. } => {
                round.current_tool_name = Some(name);
            }

            AgentEvent::ToolEnd { result, .. } => {
                let tool_name = round
                    .current_tool_name
                    .take()
                    .unwrap_or_else(|| "unknown".to_string());

                let observation = match &result {
                    Ok(output) => (tool_name, Ok(output.clone())),
                    Err(err) => (tool_name, Err(err.clone())),
                };
                round.tool_observations.push(observation);

                println!(
                    "=============================== 工具观察 ================================"
                );
                match &round.tool_observations.last().unwrap().1 {
                    Ok(output) => {
                        // 尝试格式化 JSON 输出
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&value).unwrap_or(output.clone())
                            );
                        } else {
                            println!("{}", output);
                        }
                    }
                    Err(err) => {
                        println!("❌ 工具错误 [{}] {}", err.kind, err.message);
                    }
                }
                println!();
            }

            AgentEvent::Retry {
                tool_call_id,
                attempt,
                max_attempts,
                reason,
            } => {
                println!(
                    "=============================== 工具观察 ================================"
                );
                println!(
                    "🔄 重试工具 {} (第 {}/{} 次): {}",
                    tool_call_id, attempt, max_attempts, reason
                );
                println!();
            }

            // ─── 终态事件 ────────────────────────────────────────────
            AgentEvent::LoopEnd { result } => {
                println!();
                println!("--- 执行摘要 ---");
                println!("停止原因: {:?}", result.stop_reason);
                println!("迭代次数: {}", result.iterations);
                println!("工具调用总数: {}", result.tool_calls_executed);
                println!(
                    "Token 消耗: prompt={}, completion={}, total={}",
                    result.response.usage.prompt_tokens,
                    result.response.usage.completion_tokens,
                    result.response.usage.total_tokens,
                );
                return Ok(());
            }

            AgentEvent::LoopError { error, iterations } => {
                println!();
                println!("================================ 错误 =================================");
                println!("❌ Agent 执行失败（第 {} 轮）: {}", iterations, error);
                println!();
                return Err(format!("Agent 执行失败: {}", error).into());
            }
        }
    }

    eprintln!("[WARN] Stream 意外结束，未收到终止事件");
    Ok(())
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("错误：请设置 OPENAI_API_KEY 环境变量");
        eprintln!("用法：OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real");
        std::process::exit(1);
    }

    let provider = create_provider();

    // 命令行参数选择场景：
    //   - 无参数 或 "product" → 产品搜索链（模拟数据）
    //   - "weather" → 天气查询链（真实 wttr.in API）
    //   - 其他文本 → 天气查询链，文本作为地址输入
    let scenario = std::env::args().nth(1);
    match scenario {
        Some(arg) if arg == "product" => {
            let agent = create_product_agent(provider);
            println!("=== LeLLM Agent — 产品搜索链（模拟数据）===\n");
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block(
                    "找出当前最受欢迎的无线耳机并检查其库存".to_string(),
                ),
            }]);
            observe_react_loop(stream, "找出当前最受欢迎的无线耳机并检查其库存").await
        }
        Some(arg) if arg == "weather" => {
            let agent = create_weather_agent(provider);
            println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block("帮我查一下浦东新区的天气".to_string()),
            }]);
            observe_react_loop(stream, "帮我查一下浦东新区的天气").await
        }
        Some(address) => {
            let agent = create_weather_agent(provider);
            println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");
            let question = format!("帮我查一下{}的天气", address);
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block(question.clone()),
            }]);
            observe_react_loop(stream, &question).await
        }
        None => {
            // 默认：天气查询链
            let agent = create_weather_agent(provider);
            println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block("帮我查一下浦东新区的天气".to_string()),
            }]);
            observe_react_loop(stream, "帮我查一下浦东新区的天气").await
        }
    }
}
