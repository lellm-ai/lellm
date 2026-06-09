use super::budget::ContextBudget;
use super::compactor::ContextCompactor;
use super::estimation::{estimate_message, estimate_tokens};
use super::local_compactor::LocalCompactor;
use lellm_core::{ContentBlock, Message, text_block};

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
