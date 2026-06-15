//! 本地滑动窗口压缩器 — v0.1 默认实现。
//!
//! 策略：
//! 1. 保留 System 消息（始终）
//! 2. 按 Turn 分组：Assistant + 对应的所有 ToolResult = 一个原子 Turn
//! 3. 保留最近 N 个 Turn（N = keep_recent_turns）
//! 4. 超出的 Turns 压缩为一条 Summary 消息
//!
//! 输出结构：System + Summary + 最近 N 个 Turn

use lellm_core::{Message, text_block};

use super::budget::ContextBudget;
use super::compactor::{CompactionResult, ContextCompactor};

/// 本地滑动窗口压缩器 — v0.1 默认实现。
#[derive(Debug, Default)]
pub struct LocalCompactor;

impl LocalCompactor {
    pub fn new() -> Self {
        Self
    }
}

impl ContextCompactor for LocalCompactor {
    fn compact(&self, messages: &[Message], budget: &ContextBudget) -> CompactionResult {
        let before_tokens = super::estimation::estimate_tokens(messages);
        let before_count = messages.len();

        // 1. 分离 System 消息
        let (system_msgs, conversation): (Vec<_>, Vec<_>) = messages
            .iter()
            .partition(|m| matches!(m, Message::System { .. }));

        // 2. 按 Turn 分组
        let turns = extract_turns(&conversation);

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
            result.push(Message::System {
                content: text_block(format!("[Previous conversation summary]\n{summary}")),
            });
        }

        for turn in recent_turns {
            for msg in turn {
                result.push((*msg).clone());
            }
        }

        let after_tokens = super::estimation::estimate_tokens(&result);
        let removed = before_count.saturating_sub(result.len());

        if removed > 0 {
            tracing::debug!(
                before_tokens,
                after_tokens,
                removed_messages = removed,
                before_count,
                after_count = result.len(),
                "LocalCompactor: context compressed"
            );
        }

        CompactionResult {
            messages: result,
            before_tokens,
            after_tokens,
            removed_messages: removed,
        }
    }
}

// ─── Turn 提取 ─────────────────────────────────────────────────────

/// 从消息列表中提取 Turn。
///
/// 一个 Turn = Assistant 消息 + 其对应的所有 ToolResult。
/// 这是 **不可拆分的原子块**。
///
/// User 消息作为 Turn 之间的分隔符，附着到下一个 Turn。
fn extract_turns<'a>(messages: &[&'a Message]) -> Vec<Vec<&'a Message>> {
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

// ─── 摘要生成 ──────────────────────────────────────────────────────

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
                        lines.push(format!("  Assistant: {}", summary));
                    }

                    if !tool_calls.is_empty() {
                        for tc in &tool_calls {
                            let args_summary = truncate_chars(&tc.arguments.to_string(), 100);
                            lines.push(format!("  Tool({}): {}", tc.name, args_summary));
                        }
                    }
                }
                Message::ToolResult {
                    is_error, content, ..
                } => {
                    let status = if *is_error { "ERROR" } else { "OK" };
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
                    lines.push(format!("  User: {}", summary));
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
    use lellm_core::ContentBlock;

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
        let turns = extract_turns(&messages);

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
        let turns = extract_turns(&messages);

        // Turn 1: User (standalone, no following Assistant)
        // Turn 2: Assistant(tool_call) + ToolResult (atomic block)
        // Turn 3: Assistant(final answer)
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].len(), 1); // User
        assert_eq!(turns[1].len(), 2); // Assistant + ToolResult
        assert_eq!(turns[2].len(), 1); // Assistant (final)
    }
}
