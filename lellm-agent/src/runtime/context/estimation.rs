//! Token 估算 — CJK-aware 启发式，零额外依赖。
//!
//! 估算规则：
//! - ASCII 字符: 4 chars ≈ 1 token（BPE 常见比例）
//! - CJK 汉字: 2.5 tokens/字
//! - 其他 Unicode（标点、空白等）: 1 token/字
//! - Image 块: 固定 1000 tokens
//! - 安全系数: 1.1x（覆盖 role marker、JSON wrapper 等协议开销）
//!
//! v0.1 使用启发式估算，零额外依赖。
//! P2 可替换为 `tiktoken-rs` 等 Provider-specific tokenizer。

use lellm_core::{ContentBlock, Message};

/// 估算消息列表的总 Token 数（CJK-aware 启发式）。
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// 估算单条消息的 Token 数（含 role 和结构开销）。
pub fn estimate_message(msg: &Message) -> usize {
    let mut total: usize = 0;

    // Message 类型本身的开销（role + 结构标记）
    total += 4;

    match msg {
        Message::System { content }
        | Message::User { content }
        | Message::Assistant { content } => {
            for block in content {
                total += estimate_block(block);
            }
        }
        Message::ToolResult {
            tool_call_id,
            is_error: _,
            content,
        } => {
            // tool_call_id 字段开销
            total += estimate_text(tool_call_id);
            for block in content {
                total += estimate_block(block);
            }
        }
    }

    total
}

fn estimate_block(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text(t) => estimate_text(&t.text),
        ContentBlock::Thinking(th) => {
            estimate_text(&th.thinking)
                + th.redacted.as_ref().map(|r| estimate_text(r)).unwrap_or(0)
        }
        ContentBlock::Image { .. } => 1000,
        ContentBlock::ToolCall(tc) => {
            // id + name + arguments
            6 + estimate_text(&tc.id) + estimate_text(&tc.name) + estimate_json_value(&tc.arguments)
        }
    }
}

fn estimate_json_value(value: &serde_json::Value) -> usize {
    // 将 JSON 序列化为字符串后估算
    estimate_text(&serde_json::to_string(value).unwrap_or_default())
}

/// 估算文本的 Token 数（CJK-aware，含 1.1x 安全系数）。
///
/// 估算规则：
/// - ASCII 字符: 4 chars ≈ 1 token（BPE 常见比例）
/// - CJK 汉字: 2.5 token/字（1 char = 5 raw, 除以 2 = 2.5）
/// - 其他 Unicode（标点、空白等）: 1 token/字
/// - 最后乘以 1.1x 安全系数，覆盖协议开销（role marker、JSON wrapper 等）
///
/// 可用于流式 delta 的增量 token 累计，作为输出预算的保险丝。
pub fn estimate_text(s: &str) -> usize {
    let mut ascii_count: usize = 0;
    let mut cjk_count: usize = 0;
    let mut other_count: usize = 0;

    for ch in s.chars() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else if ch.is_alphabetic() || ch.is_numeric() {
            // CJK 汉字按 2.5 token/字 → 1 char = 5 raw, 除以 2 = 2.5
            cjk_count += 5;
        } else {
            // 标点、空白等按 1 token
            other_count += 1;
        }
    }

    // 1.1x 安全系数 — 覆盖 role marker、JSON wrapper 等协议开销
    let raw = (ascii_count.saturating_div(4)) + (cjk_count.saturating_div(2)) + other_count;
    (raw as f32 * 1.1).ceil() as usize
}
