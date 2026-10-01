//! AgentStateMerge 并行合并语义测试（L1 修复）。
//!
//! 覆盖审计报告遗留问题 L1 的三场景 + 累加器/轮次/终态字段的 delta 合并与冲突检测。
//! 纯逻辑测试，无外部调用，每个用例微秒级。

use lellm_agent::runtime::StopReason;
use lellm_agent::runtime::typed_state::{AgentState, AgentStateMerge};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage};
use lellm_graph::WorkflowError;
use lellm_graph::state::workflow_state::MergeStrategy;

// ─── 构造辅助 ────────────────────────────────────────────────────

/// 便捷构造一组 User 纯文本消息。
fn msgs(items: &[&str]) -> Vec<Message> {
    items.iter().map(|s| Message::user_text(s)).collect()
}

/// 便捷构造一个 ChatResponse（单文本块）。
fn resp(text: &str) -> ChatResponse {
    ChatResponse::new(
        vec![ContentBlock::text(text)],
        TokenUsage::default(),
        serde_json::json!(null),
    )
}

/// 便捷构造 AgentState（字段顺序：messages, iterations, tool_calls, out, reas, compact, stop, last）。
fn st(
    messages: Vec<Message>,
    iterations: usize,
    tool_calls: usize,
    output_tokens: usize,
    reasoning_tokens: usize,
    compact_count: usize,
    stop: Option<StopReason>,
    last: Option<ChatResponse>,
) -> AgentState {
    AgentState {
        messages,
        iterations,
        total_tool_calls: tool_calls,
        output_tokens,
        reasoning_tokens,
        compact_count,
        stop_reason: stop,
        last_response: last,
    }
}

/// 提取每条消息的首个文本块内容，用于比较（Message 无 PartialEq 的直接断言需求）。
fn texts(m: &[Message]) -> Vec<String> {
    m.iter()
        .map(|msg| match msg {
            Message::User { content }
            | Message::Assistant { content }
            | Message::System { content } => content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text(tb) => Some(tb.text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        })
        .collect()
}

/// 经公开入口调用合并。
fn merge(base: &AgentState, branches: Vec<AgentState>) -> Result<AgentState, WorkflowError> {
    AgentStateMerge::merge(base, branches)
}

// ─── 整体边界 ────────────────────────────────────────────────────

#[test]
fn empty_branches_returns_base() {
    let base = st(
        msgs(&["m1"]),
        3,
        5,
        10,
        2,
        1,
        Some(StopReason::Complete),
        Some(resp("r")),
    );
    let result = merge(&base, vec![]).expect("空分支应返回 base");
    assert_eq!(result.iterations, 3);
    assert_eq!(result.total_tool_calls, 5);
    assert_eq!(result.output_tokens, 10);
    assert_eq!(texts(&result.messages), vec!["m1"]);
    assert_eq!(result.stop_reason, Some(StopReason::Complete));
    assert!(result.last_response.is_some());
}

#[test]
fn single_branch_returns_that_branch() {
    let base = st(msgs(&["m1"]), 1, 0, 0, 0, 0, None, None);
    let b = st(
        msgs(&["m1", "m2"]),
        2,
        3,
        40,
        5,
        1,
        Some(StopReason::Complete),
        Some(resp("r")),
    );
    let result = merge(&base, vec![b.clone()]).expect("单分支应返回该分支");
    assert_eq!(result.iterations, 2);
    assert_eq!(result.total_tool_calls, 3);
    assert_eq!(result.output_tokens, 40);
    assert_eq!(result.reasoning_tokens, 5);
    assert_eq!(result.compact_count, 1);
    assert_eq!(texts(&result.messages), vec!["m1", "m2"]);
    assert_eq!(result.stop_reason, Some(StopReason::Complete));
    assert_eq!(result.last_response, Some(resp("r")));
}

// ─── 场景 1：A 改字段，B 不动 ────────────────────────────────────

#[test]
fn scenario1_single_branch_changes_counter_others_unchanged() {
    // base=5，A→7（+2），B 未动 → 取 A 的值 7
    let base = st(vec![], 0, 5, 0, 0, 0, None, None);
    let a = st(vec![], 0, 7, 0, 0, 0, None, None);
    let b = st(vec![], 0, 5, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单分支变更应取该值");
    assert_eq!(result.total_tool_calls, 7);
}

#[test]
fn scenario1_single_branch_sets_stop_reason_others_unchanged() {
    // A 设 stop_reason，B 未动 → 取 A
    let base = st(vec![], 0, 0, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, Some(StopReason::Complete), None);
    let b = st(vec![], 0, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单分支变更应取该值");
    assert_eq!(result.stop_reason, Some(StopReason::Complete));
}

// ─── 场景 2：两分支各追加消息（核心 bug：不得重复公共历史）──────

#[test]
fn scenario2_both_append_messages_no_duplication() {
    let base = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None);
    let a = st(msgs(&["m1", "m2", "m3"]), 0, 0, 0, 0, 0, None, None);
    let b = st(msgs(&["m1", "m2", "m4"]), 0, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("两分支追加应合并");
    // 公共历史 m1/m2 只出现一次，后缀按注册顺序拼接
    assert_eq!(texts(&result.messages), vec!["m1", "m2", "m3", "m4"]);
}

// ─── 场景 3：删除 / 清空 ─────────────────────────────────────────

#[test]
fn scenario3a_one_branch_clears_others_untouched_takes_clear() {
    // A 清空（replace 到空），B 未动 → 取清空
    let base = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, None, None);
    let b = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单分支清空其余未动应取清空");
    assert!(result.messages.is_empty());
}

#[test]
fn scenario3b_one_branch_clears_other_appends_conflict() {
    // A 清空（replace），B 追加（非空后缀）→ 冲突
    let base = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, None, None);
    let b = st(msgs(&["m1", "m2", "m3"]), 0, 0, 0, 0, 0, None, None);
    assert!(
        merge(&base, vec![a, b]).is_err(),
        "replace 与非空追加并存应冲突"
    );
}

// ─── messages：replace 冲突 + 完整前缀判定 ───────────────────────

#[test]
fn messages_two_replaces_conflict() {
    // 两分支都 replace（压缩到不同摘要）→ 冲突
    let base = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None);
    let a = st(msgs(&["sum_a"]), 0, 0, 0, 0, 0, None, None);
    let b = st(msgs(&["sum_b"]), 0, 0, 0, 0, 0, None, None);
    assert!(merge(&base, vec![a, b]).is_err(), "两分支都 replace 应冲突");
}

#[test]
fn messages_append_requires_full_prefix_not_just_length() {
    // b 与 a 同长度，但前缀 [x] != base [m1] → b 是 replace，不是 append
    let base = st(msgs(&["m1"]), 0, 0, 0, 0, 0, None, None);
    let a = st(msgs(&["m1", "m2"]), 0, 0, 0, 0, 0, None, None); // 真追加
    let b = st(msgs(&["x", "m2"]), 0, 0, 0, 0, 0, None, None); // 前缀不匹配 → replace
    assert!(
        merge(&base, vec![a, b]).is_err(),
        "前缀不匹配应判为 replace 并冲突"
    );
}

// ─── 累加器：求和 / reset 冲突 / 溢出 ────────────────────────────

#[test]
fn accumulator_multiple_increments_sum_deltas() {
    // base=5，A→7(+2)，B→8(+3) → 5 + 2 + 3 = 10
    let base = st(vec![], 0, 5, 0, 0, 0, None, None);
    let a = st(vec![], 0, 7, 0, 0, 0, None, None);
    let b = st(vec![], 0, 8, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("多分支递增应求和");
    assert_eq!(result.total_tool_calls, 10);
}

#[test]
fn accumulator_multi_change_with_reset_conflict() {
    // 用户反例：base=10，A 清零到 0，B 增到 12 → 冲突（不得算出 2）
    let base = st(vec![], 0, 10, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, None, None);
    let b = st(vec![], 0, 12, 0, 0, 0, None, None);
    assert!(
        merge(&base, vec![a, b]).is_err(),
        "多分支含 reset 应冲突，不得差值求和"
    );
}

#[test]
fn accumulator_single_reset_takes_that_value() {
    // base=10，A 清零到 0，B 未动 → 单分支变更取 A = 0
    let base = st(vec![], 0, 10, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, None, None);
    let b = st(vec![], 0, 10, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单分支 reset 应取该值");
    assert_eq!(result.total_tool_calls, 0);
}

#[test]
fn accumulator_overflow_returns_error() {
    // base=0，A=usize::MAX，B=1 → 0 + (MAX-0) + (1-0) 溢出 → 显式错误，不回绕
    let base = AgentState {
        output_tokens: 0,
        ..Default::default()
    };
    let a = AgentState {
        output_tokens: usize::MAX,
        ..Default::default()
    };
    let b = AgentState {
        output_tokens: 1,
        ..Default::default()
    };
    assert!(
        merge(&base, vec![a, b]).is_err(),
        "求和溢出应返回错误，不回绕"
    );
}

// ─── iterations：取 max / 单分支 reset ───────────────────────────

#[test]
fn iterations_multiple_increments_take_max() {
    // base=1，A→2，B→3 → max(1,2,3)=3（非求和 4）
    let base = st(vec![], 1, 0, 0, 0, 0, None, None);
    let a = st(vec![], 2, 0, 0, 0, 0, None, None);
    let b = st(vec![], 3, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("多分支递增取 max");
    assert_eq!(result.iterations, 3);
}

#[test]
fn iterations_single_decrease_takes_that_value() {
    // 与累加器一致：单分支变更（含降低）取该值
    let base = st(vec![], 5, 0, 0, 0, 0, None, None);
    let a = st(vec![], 2, 0, 0, 0, 0, None, None);
    let b = st(vec![], 5, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单分支变更取该值");
    assert_eq!(result.iterations, 2);
}

// ─── stop_reason / last_response：成组，单写者保留 / 多写者冲突 ──

#[test]
fn terminal_pair_multi_writer_conflict() {
    // 两分支都写 stop_reason → 冲突
    let base = st(vec![], 0, 0, 0, 0, 0, None, None);
    let a = st(vec![], 0, 0, 0, 0, 0, Some(StopReason::Complete), None);
    let b = st(vec![], 0, 0, 0, 0, 0, Some(StopReason::Cancelled), None);
    assert!(merge(&base, vec![a, b]).is_err(), "两分支都写终态应冲突");
}

#[test]
fn terminal_pair_grouped_no_stitching() {
    // A 写 (stop=Complete, last=resp_a)，B 只写 last=resp_b → 组被两分支变更 → 冲突
    // 不得拼出「A 的 stop_reason + B 的 last_response」这种从未出现的终态
    let base = st(vec![], 0, 0, 0, 0, 0, None, None);
    let a = st(
        vec![],
        0,
        0,
        0,
        0,
        0,
        Some(StopReason::Complete),
        Some(resp("a")),
    );
    let b = st(vec![], 0, 0, 0, 0, 0, None, Some(resp("b")));
    assert!(
        merge(&base, vec![a, b]).is_err(),
        "成组字段被两分支变更应冲突，不得拼接"
    );
}

#[test]
fn terminal_pair_single_writer_takes_both_fields() {
    // A 写 (stop, last)，B 未动 → 取 A 的两者（不得 A 的 stop 配 base 的 last）
    let base = st(vec![], 0, 0, 0, 0, 0, None, None);
    let a = st(
        vec![],
        0,
        0,
        0,
        0,
        0,
        Some(StopReason::Complete),
        Some(resp("a")),
    );
    let b = st(vec![], 0, 0, 0, 0, 0, None, None);
    let result = merge(&base, vec![a, b]).expect("单写者应取该分支两者");
    assert_eq!(result.stop_reason, Some(StopReason::Complete));
    assert_eq!(result.last_response, Some(resp("a")));
}
