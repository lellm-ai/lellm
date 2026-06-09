//! weather_agent — 天气查询链
//!
//! 工具链：`resolve_city(address) → http_get(wttr.in) → LLM 解析为 JSON`
//!
//! resolve_city 四级降级：
//! ```text
//! 本地别名表 (O(1), <1ms)
//!     ↓ miss
//! OpenStreetMap Nominatim (免费, 无需 API Key)
//!     ↓ miss / 限流
//! LLM 轻量推理 (temperature=0, max_tokens=20)
//!     ↓ 无法确定
//! "unknown"
//! ```
//!
//! 工具层不硬编码业务 API，仅提供通用 `http_get`。LLM 自行构造 URL 并解析响应。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example weather_agent [地址]
//! ```

#[path = "_shared/shared.rs"]
mod shared;

use lellm_agent::{AgentBuilder, ToolArgs, ToolRegistration, ToolUseLoop, schemars::JsonSchema};
use lellm_core::{ChatRequest, Message, ToolError, ToolErrorKind, text_block};
use lellm_macros::ToolDefinition;
use lellm_provider::LlmProvider;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::GenericProvider;
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

// ─── Tool 1: resolve_city ───────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
struct CityResult {
    city_en: String, // wttr.in 城市名 (kebab-case)，未知 → "unknown"
    source: String,  // alias / nominatim / llm / unknown
    address: String, // 原始请求地址
}

#[derive(JsonSchema, ToolDefinition)]
#[tool(
    name = "resolve_city",
    description = "将地址解析为 wttr.in 城市英文名。四级降级：别名表 → Nominatim → LLM → unknown。始终调用此工具，不要猜测。"
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
            address: address.to_string(),
        };
    }
    if let Some(result) = resolve_via_nominatim(address) {
        return result;
    }
    CityResult {
        city_en: "unknown".to_string(),
        source: "unknown".to_string(),
        address: address.to_string(),
    }
}

// fn resolve_via_nominatim(address: &str) -> Option<CityResult> {
//     let url = format!(
//         "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=1&addressdetails=1",
//         url_encode(address)
//     );
//     let resp = reqwest::blocking::Client::builder()
//         .user_agent("LeLLM-WeatherAgent/0.1")
//         .build()
//         .ok()?
//         .get(&url)
//         .header("Accept-Language", "zh-CN")
//         .send()
//         .ok()?;
//     if !resp.status().is_success() {
//         return None;
//     }
//     let bodies: Vec<serde_json::Value> = resp.json().ok()?;
//     let first = bodies.first()?;
//     let addr = first.get("address")?;
//     let city_en = addr
//         .get("city")
//         .or_else(|| addr.get("town"))
//         .or_else(|| addr.get("county"))
//         .or_else(|| addr.get("village"))
//         .and_then(|v| v.as_str())
//         .map(|s| to_kebab(s))
//         .or_else(|| {
//             first
//                 .get("display_name")
//                 .and_then(|v| v.as_str())
//                 .map(|name| {
//                     name.split(',')
//                         .next()
//                         .unwrap_or("")
//                         .split(' ')
//                         .filter(|w| !w.is_empty())
//                         .collect::<Vec<_>>()
//                         .join("-")
//                 })
//         })
//         .unwrap_or_default();
//     Some(CityResult {
//         city_en,
//         source: "nominatim".to_string(),
//         address: address.to_string(),
//     })
// }

//fn resolve_via_tencent(address: &str) -> Option<CityResult> {
fn resolve_via_nominatim(address: &str) -> Option<CityResult> {
    let api_key = match std::env::var("TENCENT_MAP_KEY") {
        Ok(ak) => ak,
        Err(e) => {
            tracing::debug!(address, error = %e, "TENCENT_MAP_KEY not set, skipping nominatim");
            return None;
        }
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

pub static CITY_ALIASES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    macro_rules! insert {
        ($city:literal; $($alias:literal),+ $(,)?) => {
            $( m.insert($alias, $city); )*
        };
    }

    insert!("shanghai"; "上海", "浦东", "陆家嘴", "外滩", "静安", "南京路", "虹桥", "松江", "闵行", "徐汇", "黄浦", "潍坊路");
    insert!("beijing"; "北京", "朝阳", "国贸", "三里屯", "中关村", "西单", "王府井", "海淀", "望京", "亦庄");
    insert!("guangzhou"; "广州", "天河", "珠江新城", "白云", "越秀", "海珠", "琶洲", "北京路");
    insert!("shenzhen"; "深圳", "南山", "福田", "罗湖", "宝安", "龙华", "前海", "科技园", "华强北");
    insert!("hangzhou"; "杭州", "西湖", "滨江", "余杭", "未来科技城", "钱江新城", "武林广场");
    insert!("chengdu"; "成都", "春熙路", "太古里", "宽窄巷子", "锦里", "武侯", "高新", "玉林");
    insert!("chongqing"; "重庆", "解放碑", "洪崖洞", "江北嘴", "观音桥", "渝中");
    insert!("wuhan"; "武汉", "光谷", "江汉路", "黄鹤楼", "武昌", "汉口", "东湖");
    insert!("nanjing"; "南京", "新街口", "夫子庙", "玄武湖", "中山陵", "河西", "秦淮");
    insert!("xian"; "西安", "钟楼", "大雁塔", "回民街", "城墙", "高新", "曲江");
    insert!("qingdao"; "青岛", "栈桥", "八大关", "五四广场", "崂山");
    insert!("dalian"; "大连", "星海广场", "中山广场", "星海湾");
    insert!("xiamen"; "厦门", "鼓浪屿", "曾厝垵", "环岛路", "中山路");
    insert!("changsha"; "长沙", "五一广场", "橘子洲", "岳麓山", "天心阁");
    insert!("harbin"; "哈尔滨", "中央大街", "索菲亚教堂", "太阳岛", "冰雪大世界");
    insert!("kunming"; "昆明", "滇池", "翠湖", "石林");
    insert!("sanya"; "三亚", "亚龙湾", "海棠湾", "大东海", "天涯海角");
    insert!("lasa"; "拉萨", "布达拉宫", "大昭寺", "八廓街");
    insert!("tai-pei"; "台北", "101", "西门町", "台北101", "士林夜市");
    insert!("hong-kong"; "香港", "旺角", "铜锣湾", "中环", "尖沙咀", "维港");
    insert!("macau"; "澳门", "大三巴", "威尼斯人", "新葡京");
    insert!("tokyo"; "东京", "东京都", "新宿", "涩谷", "浅草", "秋叶原", "池袋", "银座", "上野", "横滨");
    insert!("osaka"; "大阪", "心斋桥", "道顿堀", "难波", "梅田");
    insert!("seoul"; "首尔", "明洞", "弘大", "江南", "景福宫");
    insert!("new-york"; "纽约", "曼哈顿", "布鲁克林", "时代广场", "中央公园", "华尔街", "第五大道");
    insert!("london"; "伦敦", "皮卡迪利", "牛津街", "塔桥", "大本钟", "西区");
    insert!("paris"; "巴黎", "埃菲尔", "香榭丽舍", "凯旋门", "卢浮宫");
    insert!("singapore"; "新加坡", "滨海湾", "鱼尾狮", "乌节路");
    insert!("bangkok"; "曼谷", "大皇宫", "暹罗", "苏梅");
    insert!("sydney"; "悉尼", "歌剧院", "海港大桥", "邦迪海滩");

    m
});

/// 中文地名 → 拼音 kebab-case 静态映射。
///
/// 覆盖自治区、自治州、地级市、常见县/县级市。
/// 前缀匹配使用 `max_by_key` 取最长命中，不依赖排列顺序。
const CITY_PINYIN_MAP: &[(&str, &str)] = &[
    // — 自治区 —
    ("新疆维吾尔自治区", "xinjiang"),
    ("西藏自治区", "xizang"),
    ("内蒙古自治区", "neimenggu"),
    ("广西壮族自治区", "guangxi"),
    ("宁夏回族自治区", "ningxia"),
    // — 新疆 —
    ("巴音郭楞蒙古自治州", "bayinguoleng"),
    ("克孜勒苏柯尔克孜自治州", "kezilesu"),
    ("伊犁哈萨克自治州", "yili"),
    ("博尔塔拉蒙古自治州", "boertala"),
    ("昌吉回族自治州", "changji"),
    ("和田地区", "hetian"),
    ("喀什地区", "kashi"),
    ("阿克苏地区", "akesu"),
    ("塔城地区", "tachen"),
    ("阿勒泰地区", "aletai"),
    ("吐鲁番市", "tulufan"),
    ("哈密市", "hami"),
    ("乌鲁木齐市", "wulumuqi"),
    ("克拉玛依市", "kelamayi"),
    ("奇台县", "qitai"),
    ("巴音郭楞", "bayinguoleng"),
    ("克孜勒苏", "kezilesu"),
    ("博尔塔拉", "boertala"),
    ("昌吉州", "changji"),
    ("昌吉市", "changji"),
    ("哈密", "hami"),
    ("吐鲁番", "tulufan"),
    ("阿勒泰", "aletai"),
    ("塔城", "tachen"),
    ("阿克苏", "akesu"),
    ("喀什", "kashi"),
    ("和田", "hetian"),
    ("伊犁", "yili"),
    ("克拉玛依", "kelamayi"),
    ("乌鲁木齐", "wulumuqi"),
    // — 西藏 —
    ("拉萨市", "lasa"),
    ("日喀则市", "rikaze"),
    ("昌都市", "changdu"),
    ("林芝市", "linzhi"),
    ("山南市", "shannan"),
    ("那曲市", "naqu"),
    ("阿里地区", "ali"),
    ("拉萨", "lasa"),
    ("日喀则", "rikaze"),
    ("昌都", "changdu"),
    ("林芝", "linzhi"),
    ("山南", "shannan"),
    ("那曲", "naqu"),
    ("阿里", "ali"),
    // — 内蒙古 —
    ("呼伦贝尔市", "hulunbeier"),
    ("兴安盟", "xinganmeng"),
    ("锡林郭勒盟", "xilinner"),
    ("阿拉善盟", "alashanmeng"),
    ("通辽市", "tongliao"),
    ("赤峰市", "chifeng"),
    ("乌兰察布市", "ulanqab"),
    ("鄂尔多斯市", "eerduosi"),
    ("巴彦淖尔市", "bayannur"),
    ("乌海市", "wuhai"),
    ("包头市", "baotou"),
    ("呼和浩特市", "huhehaote"),
    ("呼伦贝尔", "hulunbeier"),
    ("兴安盟", "xinganmeng"),
    ("锡林郭勒", "xilinner"),
    ("阿拉善", "alashanmeng"),
    ("通辽", "tongliao"),
    ("赤峰", "chifeng"),
    ("乌兰察布", "ulanqab"),
    ("鄂尔多斯", "eerduosi"),
    ("巴彦淖尔", "bayannur"),
    ("乌海", "wuhai"),
    ("包头", "baotou"),
    ("呼和浩特", "huhehaote"),
    // — 广西 —
    ("南宁市", "nanning"),
    ("柳州市", "liuzhou"),
    ("桂林市", "guilin"),
    ("梧州市", "wuzhou"),
    ("北海市", "beihai"),
    ("崇左市", "chongzuo"),
    ("来宾市", "laibin"),
    ("河池市", "hechi"),
    ("百色市", "baise"),
    ("贺州市", "hezhou"),
    ("玉林市", "yulin"),
    ("防城港市", "fangchenggang"),
    ("钦州市", "qinzhou"),
    ("贵港市", "guigang"),
    ("南宁", "nanning"),
    ("柳州", "liuzhou"),
    ("桂林", "guilin"),
    ("梧州", "wuzhou"),
    ("北海", "beihai"),
    ("崇左", "chongzuo"),
    ("来宾", "laibin"),
    ("河池", "hechi"),
    ("百色", "baise"),
    ("贺州", "hezhou"),
    ("玉林", "yulin"),
    ("防城港", "fangchenggang"),
    ("钦州", "qinzhou"),
    ("贵港", "guigang"),
    // — 宁夏 —
    ("银川市", "yinchuan"),
    ("石嘴山市", "shizuishan"),
    ("吴忠市", "wuzhong"),
    ("固原市", "guyuan"),
    ("中卫市", "zhongwei"),
    ("银川", "yinchuan"),
    ("石嘴山", "shizuishan"),
    ("吴忠", "wuzhong"),
    ("固原", "guyuan"),
    ("中卫", "zhongwei"),
    // — 青海 —
    ("西宁市", "xining"),
    ("海东市", "haidong"),
    ("海北州", "haibei"),
    ("黄南州", "huangnan"),
    ("海南州", "hainan"),
    ("果洛州", "guoluo"),
    ("玉树州", "yushu"),
    ("海西州", "haixi"),
    ("西宁", "xining"),
    ("海东", "haidong"),
    ("海北", "haibei"),
    ("黄南", "huangnan"),
    ("果洛", "guoluo"),
    ("玉树", "yushu"),
    ("海西", "haixi"),
    // — 甘肃 —
    ("兰州市", "lanzhou"),
    ("天水市", "tianshui"),
    ("嘉峪关市", "jiayuguan"),
    ("金昌市", "jinchang"),
    ("白银市", "baiyin"),
    ("武威市", "wuwei"),
    ("张掖市", "zhangye"),
    ("平凉市", "pingliang"),
    ("酒泉市", "jiuquan"),
    ("庆阳市", "qingyang"),
    ("定西市", "dingxi"),
    ("陇南市", "longnan"),
    ("临夏州", "linxia"),
    ("甘南州", "gannan"),
    ("兰州", "lanzhou"),
    ("天水", "tianshui"),
    ("嘉峪关", "jiayuguan"),
    ("金昌", "jinchang"),
    ("白银", "baiyin"),
    ("武威", "wuwei"),
    ("张掖", "zhangye"),
    ("平凉", "pingliang"),
    ("酒泉", "jiuquan"),
    ("庆阳", "qingyang"),
    ("定西", "dingxi"),
    ("陇南", "longnan"),
    ("临夏", "linxia"),
    ("甘南", "gannan"),
    // — 贵州 —
    ("贵阳市", "guiyang"),
    ("六盘水市", "liupanshui"),
    ("遵义市", "zunyi"),
    ("安顺市", "anshun"),
    ("毕节市", "bijie"),
    ("铜仁市", "tongren"),
    ("黔西南州", "qianxinan"),
    ("黔东南州", "qiandongnan"),
    ("黔南州", "qiannan"),
    ("贵阳", "guiyang"),
    ("六盘水", "liupanshui"),
    ("遵义", "zunyi"),
    ("安顺", "anshun"),
    ("毕节", "bijie"),
    ("铜仁", "tongren"),
    ("黔西南", "qianxinan"),
    ("黔东南", "qiandongnan"),
    ("黔南", "qiannan"),
    // — 云南 —
    ("昆明市", "kunming"),
    ("曲靖市", "qujing"),
    ("玉溪市", "yuxi"),
    ("保山市", "baoshan"),
    ("昭通市", "zhaotong"),
    ("丽江市", "lijiang"),
    ("普洱市", "puer"),
    ("临沧市", "lincang"),
    ("楚雄州", "chuxiong"),
    ("红河州", "honghe"),
    ("文山州", "wenshan"),
    ("西双版纳州", "xishuangbanna"),
    ("大理州", "dali"),
    ("德宏州", "dehong"),
    ("怒江州", "nujiang"),
    ("迪庆州", "diqing"),
    ("昆明", "kunming"),
    ("曲靖", "qujing"),
    ("玉溪", "yuxi"),
    ("保山", "baoshan"),
    ("昭通", "zhaotong"),
    ("丽江", "lijiang"),
    ("普洱", "puer"),
    ("临沧", "lincang"),
    ("楚雄", "chuxiong"),
    ("红河", "honghe"),
    ("文山", "wenshan"),
    ("西双版纳", "xishuangbanna"),
    ("大理", "dali"),
    ("德宏", "dehong"),
    ("怒江", "nujiang"),
    ("迪庆", "diqing"),
    // — 四川 —
    ("成都市", "chengdu"),
    ("绵阳市", "mianyang"),
    ("泸州市", "luzhou"),
    ("德阳市", "deyang"),
    ("广元市", "guangyuan"),
    ("遂宁市", "suining"),
    ("内江市", "neijiang"),
    ("乐山市", "leshan"),
    ("南充市", "nanchong"),
    ("宜宾市", "yibin"),
    ("广安市", "guangan"),
    ("达州市", "dazhou"),
    ("巴中市", "bazhong"),
    ("雅安市", "yaan"),
    ("眉山市", "meishan"),
    ("资阳市", "ziyang"),
    ("阿坝州", "aba"),
    ("甘孜州", "ganzi"),
    ("凉山州", "liangshan"),
    ("成都", "chengdu"),
    ("绵阳", "mianyang"),
    ("泸州", "luzhou"),
    ("德阳", "deyang"),
    ("广元", "guangyuan"),
    ("遂宁", "suining"),
    ("内江", "neijiang"),
    ("乐山", "leshan"),
    ("南充", "nanchong"),
    ("宜宾", "yibin"),
    ("广安", "guangan"),
    ("达州", "dazhou"),
    ("巴中", "bazhong"),
    ("雅安", "yaan"),
    ("眉山", "meishan"),
    ("资阳", "ziyang"),
    ("阿坝", "aba"),
    ("甘孜", "ganzi"),
    ("凉山", "liangshan"),
    // — 重庆 —
    ("重庆市", "chongqing"),
    ("重庆", "chongqing"),
    // — 北京 —
    ("北京市", "beijing"),
    ("北京", "beijing"),
    // — 天津 —
    ("天津市", "tianjin"),
    ("天津", "tianjin"),
    // — 上海 —
    ("上海市", "shanghai"),
    ("上海", "shanghai"),
];

/// 将地址字符串转为 Weather API 可用的 kebab-case 城市名。
///
/// 匹配优先级：
/// 1. **全匹配** — 输入精确等于映射 key
/// 2. **前缀匹配** — 输入以映射 key 开头（如 "昌吉市天气" → "changji"）
/// 3. **kebab 回退** — 对 ASCII 字符走原有 kebab-case 逻辑
fn to_kebab(s: &str) -> String {
    // 1. 全匹配
    if let Some(&(_, pinyin)) = CITY_PINYIN_MAP.iter().find(|(key, _)| *key == s) {
        return pinyin.to_string();
    }
    // 2. 前缀匹配（map 已按长度降序排列，优先命中更长 key）
    if let Some(longest) = CITY_PINYIN_MAP
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

// ─── LLM 城市解析器 ──────────────────────────────────────────────

/// 第四级降级：用 LLM 轻量推理地址所属城市。
///
/// 要求 LLM 输出 JSON `{"city":"xxx"}`，结构化解析（rfind 取最后一个 city 值）。
/// max_tokens=1000，推理模型需要足够 tokens 完成思考。
/// 仅当前三级（别名表 + 地图 API）均 miss 时触发。
async fn resolve_via_llm(provider: &Arc<dyn LlmProvider>, address: &str) -> Option<CityResult> {
    let prompt = format!(
        "地址「{0}」属于哪个城市？\n\
         输出 wttr.in 可用的城市英文名（全小写，多词用连字符，如 new-york）。\n\
         只输出紧凑的 JSON：{{\"city\": \"城市英文名\"}}，不确定则 {{\"city\": \"unknown\"}}",
        address
    );

    let req = ChatRequest {
        model: "Qwen3.6".to_string(),
        messages: vec![Message::User {
            content: text_block(prompt),
        }],
        tools: None,
        temperature: Some(0.0),
        max_tokens: Some(1000),
        ..Default::default()
    };

    let resp = provider.call(&req).await.ok()?;

    // 从响应中提取全部文本（含 reasoning_content 回退）
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

/// 从 LLM 响应文本中查找 JSON 并提取 city 字段。
///
/// 策略：总是取**最后一个** `"city"` 的值。
/// 因为推理文本中可能包含 prompt 示例（如参考列表），
/// 而模型的实际答案通常在最后输出。
fn parse_city_from_json(text: &str) -> Option<String> {
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

// ─── 工具注册 ────────────────────────────────────────────────────

fn register_weather_tools(llm_provider: Option<Arc<dyn LlmProvider>>) -> Vec<ToolRegistration> {
    vec![
        ToolRegistration::safe(ResolveCityArgs::tool_definition(), move |args| {
            let address = args
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let provider = llm_provider.clone();
            async move {
                // 第一、二级：alias + nominatim（阻塞线程）
                let address_for_blocking = address.clone();
                let mut result =
                    tokio::task::spawn_blocking(move || resolve_city(&address_for_blocking))
                        .await
                        .map_err(|e| ToolError {
                            kind: ToolErrorKind::Internal,
                            message: format!("任务失败: {e}"),
                        })?;

                // 第三级 miss → 第四级：LLM 轻量推理
                if result.source == "unknown" {
                    tracing::debug!(address = %address, "alias+nominatim miss, trying LLM fallback");
                    if let Some(ref p) = provider {
                        if let Some(city) = resolve_via_llm(p, &address).await {
                            tracing::debug!(city = %city.city_en, "LLM fallback success");
                            result = city;
                        }
                    }
                }

                serde_json::to_string(&result).map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("序列化失败: {e}"),
                })
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
    // 共享 provider：主 Agent Loop + resolve_city 第四级降级各持一份 Arc
    let shared_provider: Arc<dyn LlmProvider> = Arc::new(provider);

    let prompt = r#"你是天气查询助手。

流程：
1. 提取用户输入中的所有地址
2. 对每个地址调用 resolve_city
3. 对 city_en != "unknown" 调用 http_get(https://wttr.in/{city_en}?format=%c+%t+%h+%w)
4. 解析 wttr.in 返回的文本，提取天气数据，输出 JSON

wttr.in 返回格式: "🌧️ +17°C 94% ↖11km/h"
你需要转换以下字段：

1. condition（emoji → 中文）：
   - 🌧️/🌦️/🌧 → 小雨/中雨/大雨
   - ☀️/🌤 → 晴/多云
   - 🌥/⛅ → 多云/阴
   - 🌨/❄️ → 雪/大雪
   - 🌪/🌫 → 沙尘暴/雾
   - 其他 emoji 自行翻译为对应的中文天气描述

2. temperature（格式修正）：
   - "+23°C" → "23°C"（去掉 + 号）
   - "-5°C" → "零下5°C"（负数加"零下"）

3. wind（方向箭头 → 中文）：
   - "→" → "东风", "←" → "西风", "↑" → "南风", "↓" → "北风"
   - "↗" → "东南风", "↘" → "西南风", "↙" → "西北风", "↖" → "东北风"
   - "↖11km/h" → "东北风11km/h"
   - 无箭头（如 "7km/h"）→ 保持原样

输出格式（纯 JSON，禁止 pretty 格式化，禁止任何解释文字）：
单地址: {"city":"tokyo","address":"新宿","condition":"小雨","temperature":"17°C","humidity":"94%","wind":"东风7km/h"}
多地址: [{"city":"tokyo","address":"新宿","condition":"小雨","temperature":"17°C","humidity":"94%","wind":"东风7km/h"},{"city":"new-york","address":"曼哈顿","condition":"晴","temperature":"25°C","humidity":"60%","wind":"西风12km/h"}]

规则：
- 地址推理交给 resolve_city，不要猜测
- unknown 城市跳过天气查询
- condition 必须是中文文字（如"小雨"、"晴"、"多云"、"沙尘暴"），禁止使用 emoji
- temperature 禁止出现 + 号，负数显示"零下xx°C"
- wind 必须包含中文方向描述（如"东北风11km/h"）
- 最终回答必须为纯 JSON，不要包含 markdown 代码块标记或任何解释"#;

    AgentBuilder::new(ResolvedModel {
        provider: shared_provider.clone(),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system_prompt(prompt.to_string())
    .tools(register_weather_tools(Some(shared_provider)))
    .max_iterations(10)
    .max_output_tokens(3000)
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

    let provider =
        GenericProvider::from_env(OpenAICompatAdapter::llama()).expect("LLaMA provider env error");
    let agent = create_agent(provider);

    println!("=== Weather Agent — resolve_city(四级降级) + http_get ===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    shared::observe_react_loop(stream, &question).await
}
