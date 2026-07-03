//! #[tool] 函数宏集成测试 — 基础功能
//!
//! 覆盖：Args struct 生成, name/description, doc comment,
//! 各种参数类型, 无参数, LazyLock schema 缓存

#![allow(dead_code, unused_variables)]

use lellm_agent::ToolArgs;
use lellm_core::ToolResult;
use lellm_derive::tool;

// ============================================================================
// 1. 最基础的 #[tool] 函数 — 无额外配置
// ============================================================================

#[tool]
fn add_numbers(a: i32, b: i32) -> ToolResult {
    Ok(serde_json::json!(a + b))
}

#[test]
fn test_basic_tool_args_trait() {
    assert_eq!(AddNumbersArgs::NAME, "add_numbers");
    assert_eq!(AddNumbersArgs::DESCRIPTION, "");
}

#[test]
fn test_basic_tool_factory() {
    let reg = add_numbers_tool();
    assert_eq!(reg.definition().name, "add_numbers");
}

#[test]
fn test_basic_tool_schema() {
    let def = AddNumbersArgs::tool_definition();
    let properties = def
        .parameters
        .get("properties")
        .unwrap()
        .as_object()
        .unwrap();

    assert_eq!(properties["a"]["type"], "integer");
    assert_eq!(properties["b"]["type"], "integer");

    let required = def.parameters.get("required").unwrap().as_array().unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("a")));
    assert!(required.iter().any(|v| v.as_str() == Some("b")));
}

// ============================================================================
// 2. name 和 description 覆盖
// ============================================================================

#[tool(name = "custom_add", description = "自定义加法器")]
fn my_add(x: i64, y: i64) -> ToolResult {
    Ok(serde_json::json!(x + y))
}

#[test]
fn test_tool_name_override() {
    assert_eq!(MyAddArgs::NAME, "custom_add");
    assert_eq!(MyAddArgs::DESCRIPTION, "自定义加法器");

    let reg = my_add_tool();
    assert_eq!(reg.definition().name, "custom_add");
    assert_eq!(reg.definition().description, "自定义加法器");
}

// ============================================================================
// 3. doc comment 提取（作为 description）
// ============================================================================

/// 计算两个数的差值
#[tool]
fn subtract(minuend: i32, subtrahend: i32) -> ToolResult {
    Ok(serde_json::json!(minuend - subtrahend))
}

#[test]
fn test_tool_doc_comment_description() {
    assert_eq!(SubtractArgs::DESCRIPTION, "计算两个数的差值");
    assert_eq!(SubtractArgs::NAME, "subtract");
}

/// 这个描述应被覆盖
#[tool(description = "显式描述优先")]
fn _multiply(a: i32, b: i32) -> ToolResult {
    Ok(serde_json::json!(a * b))
}

#[test]
fn test_tool_description_attr_overrides_doc() {
    assert_eq!(MultiplyArgs::DESCRIPTION, "显式描述优先");
}

// ============================================================================
// 4. 各种参数类型
// ============================================================================

#[tool(name = "typed_params", description = "测试各种类型")]
fn typed_function(
    text: String,
    count: u32,
    flag: bool,
    score: f64,
    optional_text: Option<String>,
    optional_number: Option<i64>,
    tags: Vec<String>,
) -> ToolResult {
    Ok(serde_json::json!(format!("{}-{}", text, count)))
}

#[test]
fn test_various_param_types_schema() {
    let def = TypedFunctionArgs::tool_definition();
    let properties = def
        .parameters
        .get("properties")
        .unwrap()
        .as_object()
        .unwrap();

    assert_eq!(properties["text"]["type"], "string");
    assert_eq!(properties["count"]["type"], "integer");
    assert_eq!(properties["flag"]["type"], "boolean");
    assert_eq!(properties["score"]["type"], "number");

    let opt_type = &properties["optional_text"]["type"];
    assert!(
        *opt_type == "string" || *opt_type == serde_json::json!(["string", "null"]),
        "unexpected Option<String> type: {}",
        opt_type
    );
    assert_eq!(properties["tags"]["type"], "array");

    let required = def.parameters.get("required").unwrap().as_array().unwrap();
    let required_set: std::collections::HashSet<&str> =
        required.iter().filter_map(|v| v.as_str()).collect();

    assert!(required_set.contains("text"));
    assert!(required_set.contains("count"));
    assert!(required_set.contains("flag"));
    assert!(required_set.contains("score"));
    assert!(required_set.contains("tags"));
    assert!(!required_set.contains("optional_text"));
    assert!(!required_set.contains("optional_number"));
}

// ============================================================================
// 5. 无参数函数
// ============================================================================

/// 获取当前时间戳
#[tool]
fn get_timestamp() -> ToolResult {
    Ok(serde_json::json!("1234567890"))
}

#[test]
fn test_no_param_tool() {
    assert_eq!(GetTimestampArgs::NAME, "get_timestamp");
    assert_eq!(GetTimestampArgs::DESCRIPTION, "获取当前时间戳");

    let reg = get_timestamp_tool();
    assert_eq!(reg.definition().name, "get_timestamp");

    let def = GetTimestampArgs::tool_definition();
    let properties = def.parameters.get("properties");
    assert!(
        properties.is_none()
            || properties
                .unwrap()
                .as_object()
                .map_or(true, |obj| obj.is_empty())
    );
}

// ============================================================================
// 6. LazyLock schema 缓存 — 多次调用返回相同值
// ============================================================================

#[test]
fn test_lazylock_schema_cache() {
    let schema1 = AddNumbersArgs::schema();
    let schema2 = AddNumbersArgs::schema();
    let schema3 = AddNumbersArgs::schema();

    assert_eq!(schema1, schema2);
    assert_eq!(schema2, schema3);

    let def_schema = AddNumbersArgs::tool_definition().parameters;
    assert_eq!(schema1, def_schema);
}
