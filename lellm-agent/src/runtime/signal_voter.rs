//! 负信号投票机制 — 累积负信号触发升级。

/// 负信号类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeSignal {
    ToolError,
    ToolTimeout,
    RepeatedToolCall,
    LowConfidence,
}

/// 信号投票器 — 累积负信号，达到阈值时触发升级
pub struct SignalVoter {
    signals: Vec<NegativeSignal>,
    threshold: usize,
}

impl SignalVoter {
    pub fn new(threshold: usize) -> Self {
        Self {
            signals: Vec::new(),
            threshold,
        }
    }

    pub fn vote(&mut self, signal: NegativeSignal) -> bool {
        self.signals.push(signal);
        self.signals.len() >= self.threshold
    }

    pub fn signal_count(&self) -> usize {
        self.signals.len()
    }

    pub fn reset(&mut self) {
        self.signals.clear();
    }
}

impl Default for SignalVoter {
    fn default() -> Self {
        Self::new(5)
    }
}
