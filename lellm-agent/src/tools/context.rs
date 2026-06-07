//! 上下文预算管理 — 控制 Agent Loop 中 messages 的 Token 总量。
//!
//! 核心设计：
//! - `ContextBudget` — 纯参数配置，用户可调
//! - `ContextCompactor` — trait，可插拔压缩策略
//! - `LocalCompactor` — 默认实现，滑动窗口 + 本地摘要
//! - **Assistant(tool_call) + ToolResult 是原子块，不可拆分**

use lellm_core::{ContentBlock, Message, text_block};

// ─── 配置 ────────────────────────────────────────────────────────

/// 上下文预算配置。
///
/// 控制 Agent Loop 中消息历史的 Token 上限与压缩行为。
/// 设置为 `None` 时不做任何限制（兼容现有行为）。
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// 消息历史的最大 Token 数（默认 50,000）
    ///
    /// **方案 A (v0.1)**: 固定默认值 50k，适用于大多数模型
    /// **方案 B (v0.2)**: 从 `ResolvedModel.context_window` 自动推导（window * 0.8）
    pub max_tokens: usize,
    /// 达到此占比时触发压缩（默认 0.8 = 80%）
    pub warning_ratio: f32,
    /// 压缩时保留最近多少个 Turn（默认 5）
    pub keep_recent_turns: usize,
    /// 单条工具结果的最大字符数（默认 4096）
    pub max_tool_result_chars: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tokens: 50_000,
            warning_ratio: 0.8,
            keep_recent_turns: 5,
            max_tool_result_chars: 4096,
        }
    }
}

impl ContextBudget {
    /// 判断是否需要压缩。
    pub fn should_compact(&self, current_tokens: usize) -> bool {
        let threshold = (self.max_tokens as f32 * self.warning_ratio) as usize;
        current_tokens > threshold
    }

    /// 截断单条工具结果，防止单条响应撑爆上下文。
    pub fn truncate_tool_result(&self, text: String) -> String {
        if text.chars().count() <= self.max_tool_result_chars {
            return text;
        }
        let truncated: String = text.chars().take(self.max_tool_result_chars).collect();
        format!(
            "{}\n[truncated, original {} chars]",
            truncated,
            text.chars().count()
        )
    }
}

// ─── Token 估算 ──────────────────────────────────────────────────

/// 估算消息列表的总 Token 数（CJK-aware 启发式）。
///
/// 估算规则：
/// - ASCII 字符: 4 chars ≈ 1 token（BPE 常见比例）
/// - CJK 汉字: 2.5 tokens/字
/// - 其他 Unicode（标点、空白等）: 1 token/字
/// - Image 块: 固定 1000 tokens
/// - 安全系数: 1.1x（覆盖 role marker、JSON wrapper 等协议开销）
///
/// v0.1 使用启发式估算，零额外依赖。
/// P2 可替换为 `tiktoken-rs` 等 Provider-specific tokenizer。
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

/// 估算文本的 Token 数（CJK-aware）。
///
/// 估算规则：
/// - ASCII 字符: 4 chars ≈ 1 token（BPE 常见比例）
/// - CJK 汉字: 2.5 tokens/字（1 char = 5 raw → 除以 2 = 2.5）
/// - 其他 Unicode（标点、空白等）: 1 token/字
/// - 最后乘以 1.1x 安全系数，覆盖协议开销（role marker、JSON wrapper 等）
fn estimate_text(s: &str) -> usize {
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

// ─── 压缩结果 ────────────────────────────────────────────────────

/// 压缩操作的结果。
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// 压缩后的消息列表
    pub messages: Vec<Message>,
    /// 压缩前的 Token 数
    pub before_tokens: usize,
    /// 压缩后的 Token 数
    pub after_tokens: usize,
    /// 被移除的消息数量
    pub removed_messages: usize,
}

// ─── 压缩器 trait ────────────────────────────────────────────────

/// 上下文压缩器 — 可插拔策略。
///
/// 未来可替换为：
/// - `LLMCompactor` — 使用轻量模型生成摘要
/// - `VectorMemoryCompactor` — 基于向量相似度保留关键消息
pub trait ContextCompactor: Send + Sync {
    /// 对消息列表执行压缩。
    ///
    /// **关键约束：**
    /// Assistant(tool_call) + 对应的 ToolResult 是原子块，不可拆分。
    /// 压缩后的历史必须保持 Tool Calling 协议的完整性。
    fn compact(&self, messages: &[Message], budget: &ContextBudget) -> CompactionResult;
}

// ─── 本地压缩器（默认实现）───

/// 本地滑动窗口压缩器 — v0.1 默认实现。
///
/// 策略：
/// 1. 保留 System 消息（始终）
/// 2. 按 Turn 分组：Assistant + 对应的所有 ToolResult = 一个原子 Turn
/// 3. 保留最近 N 个 Turn（N = keep_recent_turns）
/// 4. 超出的 Turns 压缩为一条 Summary 消息
///
/// 输出结构：System + Summary + 最近 N 个 Turn
#[derive(Debug, Default)]
pub struct LocalCompactor;

impl LocalCompactor {
    pub fn new() -> Self {
        Self
    }
}

impl ContextCompactor for LocalCompactor {
    fn compact(&self, messages: &[Message], budget: &ContextBudget) -> CompactionResult {
        let before_tokens = estimate_tokens(messages);
        let before_count = messages.len();

        // 1. 分离 System 消息
        let (system_msgs, conversation): (Vec<_>, Vec<_>) = messages
            .iter()
            .partition(|m| matches!(m, Message::System { .. }));

        // 2. 按 Turn 分组
        let turns = extract_turns(conversation);

        // 3. 判断是否需要压缩
        let keep = budget.keep_recent_turns;
        if turns.len() <= keep {
            // 不需要压缩，原样返回
            return CompactionResult {
                messages: messages.to_vec(),
                before_tokens,
                after_tokens: before_tokens,
                removed_messages: 0,
            };
        }

        // 4. 保留最近 keep 个 Turn
        let recent_turns: Vec<_> = turns.iter().skip(turns.len() - keep).collect();
        let old_turns: Vec<_> = turns.iter().take(turns.len() - keep).collect();

        // 5. 对旧 Turns 生成本地摘要
        let summary = summarize_turns(&old_turns);

        // 6. 组装结果
        let mut result = system_msgs.into_iter().cloned().collect::<Vec<_>>();

        if !summary.is_empty() {
            result.push(Message::User {
                content: text_block(format!("[Previous conversation summary]\n{summary}")),
            });
        }

        for turn in recent_turns {
            for msg in turn {
                result.push((*msg).clone());
            }
        }

        let after_tokens = estimate_tokens(&result);
        let removed = before_count.saturating_sub(result.len());

        CompactionResult {
            messages: result,
            before_tokens,
            after_tokens,
            removed_messages: removed,
        }
    }
}

/// 从消息列表中提取 Turn。
///
/// 一个 Turn = Assistant 消息 + 其对应的所有 ToolResult。
/// 这是 **不可拆分的原子块**。
///
/// User 消息作为 Turn 之间的分隔符，附着到下一个 Turn。
fn extract_turns(messages: Vec<&Message>) -> Vec<Vec<&Message>> {
    let mut turns: Vec<Vec<&Message>> = Vec::new();

    let mut current_turn: Vec<&Message> = Vec::new();

    for msg in messages {
        match msg {
            Message::Assistant { .. } => {
                // 如果已有 turn，先保存
                if !current_turn.is_empty() {
                    turns.push(current_turn);
                    current_turn = Vec::new();
                }
                current_turn.push(msg);
            }
            Message::ToolResult { .. } => {
                // 附着到当前 turn（紧跟 Assistant）
                current_turn.push(msg);
            }
            Message::User { .. } => {
                // User 消息：如果当前 turn 为空，作为 turn 的起始；
                // 否则保存到当前 turn（Assistant 可能回复多条 User 消息）
                if current_turn.is_empty() {
                    current_turn.push(msg);
                } else {
                    // User 出现在 ToolResult 之后 → 新轮次的起点，
                    // 开始新的 turn
                    turns.push(current_turn);
                    current_turn = vec![msg];
                }
            }
            Message::System { .. } => {
                // System 不应出现在 conversation 中，忽略
            }
        }
    }

    if !current_turn.is_empty() {
        turns.push(current_turn);
    }

    turns
}

/// 对旧 Turns 生成本地摘要。
///
/// 策略：提取每个 Turn 的关键信息
/// - Assistant 有文本 → 保留前 200 字符
/// - Assistant 有 tool_call → 记录工具名和参数概要
/// - ToolResult → 仅记录成功/失败，截取前 100 字符
fn summarize_turns(turns: &[&Vec<&Message>]) -> String {
    let mut lines = Vec::new();

    for (idx, turn) in turns.iter().enumerate() {
        let _prefix = format!("Turn {}:", idx + 1);

        for msg in *turn {
            match msg {
                Message::Assistant { content } => {
                    let texts: Vec<_> = content.iter().filter_map(|b| b.as_text()).collect();
                    let tool_calls = msg.extract_tool_calls();

                    if !texts.is_empty() {
                        let text = texts.join(" ");
                        let summary = truncate_chars(&text, 200);
                        lines.push(format!("  🤖 Assistant: {}", summary));
                    }

                    if !tool_calls.is_empty() {
                        for tc in &tool_calls {
                            let args_summary = truncate_chars(&tc.arguments.to_string(), 100);
                            lines.push(format!("  🔧 Called {}: {}", tc.name, args_summary));
                        }
                    }
                }
                Message::ToolResult {
                    is_error, content, ..
                } => {
                    let status = if *is_error { "❌" } else { "✅" };
                    let text: String = content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let summary = truncate_chars(&text, 100);
                    lines.push(format!("  {} Result: {}", status, summary));
                }
                Message::User { content } => {
                    let text: String = content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let summary = truncate_chars(&text, 200);
                    lines.push(format!("  👤 User: {}", summary));
                }
                _ => {}
            }
        }
    }

    if lines.is_empty() {
        return String::new();
    }

    // 统计摘要
    let total_turns = turns.len();
    format!("[Compressed {} turns]\n{}", total_turns, lines.join("\n"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{}… ({} chars)", truncated, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_ascii() {
        // "Hello World" ≈ 11/4 ≈ 2 tokens + 4 overhead = ~6
        let msg = Message::User {
            content: text_block("Hello World".to_string()),
        };
        let tokens = estimate_message(&msg);
        assert!(tokens >= 4 && tokens <= 10);
    }

    #[test]
    fn test_estimate_tokens_chinese() {
        // "你好世界" ≈ 4 * 1.5 = 6 tokens + 4 overhead = ~10
        let msg = Message::User {
            content: text_block("你好世界".to_string()),
        };
        let tokens = estimate_message(&msg);
        assert!(tokens >= 6 && tokens <= 15);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        let tokens = estimate_tokens(&[]);
        assert_eq!(tokens, 0);
    }

    #[test]
    fn test_truncate_tool_result_short() {
        let budget = ContextBudget::default();
        let short = "short result".to_string();
        assert_eq!(budget.truncate_tool_result(short.clone()), short);
    }

    #[test]
    fn test_truncate_tool_result_long() {
        let budget = ContextBudget {
            max_tool_result_chars: 10,
            ..Default::default()
        };
        let long = "0123456789ABCDEFG".to_string();
        let result = budget.truncate_tool_result(long);
        assert!(result.starts_with("0123456789"));
        assert!(result.contains("[truncated"));
        assert!(result.contains("original 17 chars"));
    }

    #[test]
    fn test_extract_turns_atomic() {
        let assistant = Message::Assistant {
            content: vec![ContentBlock::ToolCall(lellm_core::ToolCall {
                id: "call_1".into(),
                name: "test".into(),
                arguments: serde_json::json!({}),
            })],
        };
        let tool_result = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            is_error: false,
            content: text_block("ok".to_string()),
        };

        let messages = vec![&assistant, &tool_result];
        let turns = extract_turns(messages);

        // Assistant + ToolResult 应在同一个 Turn 中
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].len(), 2);
    }

    #[test]
    fn test_extract_turns_multiple() {
        let user = Message::User {
            content: text_block("hello".to_string()),
        };
        let assistant = Message::Assistant {
            content: vec![ContentBlock::ToolCall(lellm_core::ToolCall {
                id: "call_1".into(),
                name: "test".into(),
                arguments: serde_json::json!({}),
            })],
        };
        let tool_result = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            is_error: false,
            content: text_block("ok".to_string()),
        };
        let assistant2 = Message::Assistant {
            content: text_block("final answer".to_string()),
        };

        let messages = vec![&user, &assistant, &tool_result, &assistant2];
        let turns = extract_turns(messages);

        // Turn 1: User (standalone, no following Assistant)
        // Turn 2: Assistant(tool_call) + ToolResult (atomic block)
        // Turn 3: Assistant(final answer)
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].len(), 1); // User
        assert_eq!(turns[1].len(), 2); // Assistant + ToolResult
        assert_eq!(turns[2].len(), 1); // Assistant (final)
    }

    #[test]
    fn test_should_compact() {
        let budget = ContextBudget {
            max_tokens: 1000,
            warning_ratio: 0.8,
            ..Default::default()
        };
        assert!(!budget.should_compact(500)); // 50%
        assert!(!budget.should_compact(799)); // 79.9%
        assert!(!budget.should_compact(800)); // 80% exactly, threshold is strict >
        assert!(budget.should_compact(801)); // 80.1%
        assert!(budget.should_compact(900)); // 90%
    }

    #[test]
    fn test_local_compactor_no_op_when_under_limit() {
        let budget = ContextBudget {
            keep_recent_turns: 5,
            ..Default::default()
        };

        // 只有 2 个 turn，不需要压缩
        let messages = vec![
            Message::User {
                content: text_block("turn 1 user".to_string()),
            },
            Message::Assistant {
                content: text_block("turn 1 assistant".to_string()),
            },
            Message::User {
                content: text_block("turn 2 user".to_string()),
            },
            Message::Assistant {
                content: text_block("turn 2 assistant".to_string()),
            },
        ];

        let compactor = LocalCompactor::new();
        let result = compactor.compact(&messages, &budget);

        // 不需要压缩，原样返回
        assert_eq!(result.messages.len(), messages.len());
        assert_eq!(result.removed_messages, 0);
    }

    #[test]
    fn test_local_compactor_compresses_old_turns() {
        let budget = ContextBudget {
            keep_recent_turns: 1,
            ..Default::default()
        };

        let mut messages = Vec::new();
        // 创建 3 个 turns（6 条消息）
        for i in 1..=3 {
            messages.push(Message::User {
                content: text_block(format!("user {}", i)),
            });
            messages.push(Message::Assistant {
                content: text_block(format!("assistant {}", i)),
            });
        }

        let compactor = LocalCompactor::new();
        let result = compactor.compact(&messages, &budget);

        // 移除了旧 turns（6 条 → summary + 2 条 = 3 条）
        assert!(result.removed_messages > 0);
        assert!(result.messages.len() < messages.len());
        // 摘要消息应存在
        assert!(result
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.iter().any(|b| b.as_text().map(|t| t.contains("Compressed")).unwrap_or(false)))));
    }
}
