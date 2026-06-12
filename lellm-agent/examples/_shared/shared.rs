//! 示例共享工具 — ReAct 循环观测器等。

use lellm_agent::{AgentEvent, AgentStream};
use lellm_provider::ProviderEvent;

/// 当前 ReAct 轮次的中间状态
#[derive(Debug, Default)]
pub struct RoundState {
    pub reasoning: String,
    pub step_start: Option<std::time::Instant>,
}

/// 观测并打印 ReAct 循环事件
pub async fn observe_react_loop(
    mut stream: AgentStream,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let total_start = std::time::Instant::now();
    println!("================================ 人类消息 =================================");
    println!("{question}\n");

    let mut iteration: usize = 0;
    let mut round = RoundState::default();
    let mut step_times: Vec<(usize, f64)> = Vec::new();

    while let Some(event) = stream.recv().await {
        match event {
            AgentEvent::Provider(ProviderEvent::Start { model: _ }) => {
                iteration += 1;
                round = RoundState {
                    reasoning: String::new(),
                    step_start: Some(std::time::Instant::now()),
                };
            }

            AgentEvent::Provider(ProviderEvent::Token { token }) => {
                round.reasoning.push_str(&token);
                print!("{token}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(ProviderEvent::ThinkingDelta { thinking, .. }) => {
                round.reasoning.push_str(&thinking);
            }

            AgentEvent::Provider(ProviderEvent::ResponseComplete { tool_calls, usage }) => {
                let elapsed = round
                    .step_start
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                step_times.push((iteration, elapsed));

                if tool_calls.is_empty() {
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
                    for tc in &tool_calls {
                        println!("  {} ({})", tc.name, tc.id);
                        println!("  参数: {}", tc.arguments);
                    }
                    println!();
                }
            }

            AgentEvent::ToolStart { .. } => {}

            AgentEvent::ToolEnd { result, .. } => {
                println!(
                    "=============================== 工具观察 ================================"
                );
                match result {
                    Ok(ref output) => {
                        if let Some(s) = output.as_str() {
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| s.to_string())
                                );
                            } else {
                                println!("{}", s);
                            }
                        } else {
                            println!("{}", output);
                        }
                    }
                    Err(err) => println!("❌ 工具错误 [{}] {}", err.kind, err.message),
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
                println!("🔄 重试 {tool_call_id} (第 {attempt}/{max_attempts} 次): {reason}");
                println!();
            }

            AgentEvent::ContextCompacted {
                before_tokens,
                after_tokens,
                removed_messages,
            } => {
                println!("============================ 上下文压缩 ==============================");
                println!(
                    "📦 压缩: {before_tokens} → {after_tokens} tokens (移除 {removed_messages} 条)"
                );
                println!();
            }

            AgentEvent::LoopEnd { result } => {
                let total = total_start.elapsed();
                println!();
                println!(
                    "================================ 最终结果 ================================="
                );
                for block in &result.response.content {
                    if let Some(text) = block.as_text() {
                        println!("{text}");
                    }
                }
                println!();
                println!("--- 执行摘要 ---");
                println!("停止原因: {:?}", result.stop_reason);
                println!("迭代次数: {}", result.iterations);
                println!("工具调用总数: {}", result.tool_calls_executed);
                println!(
                    "Token: prompt={}, completion={}, total={}",
                    result.response.usage.prompt_tokens,
                    result.response.usage.completion_tokens,
                    result.response.usage.total_tokens,
                );
                println!();
                println!("--- 耗时明细 ---");
                for (i, t) in &step_times {
                    println!("  第 {} 轮: {:.2}s", i, t);
                }
                println!("总耗时: {:.2}s", total.as_secs_f64());
                return Ok(());
            }

            AgentEvent::LoopError { error, iterations } => {
                let total = total_start.elapsed();
                println!();
                println!("================================ 错误 =================================");
                println!("❌ 失败（第 {iterations} 轮）: {error}");
                println!("总耗时: {:.2}s", total.as_secs_f64());
                return Err(format!("Agent 执行失败: {error}").into());
            }
        }
    }

    eprintln!("[WARN] Stream 意外结束");
    Ok(())
}
