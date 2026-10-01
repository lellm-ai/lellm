//! 构建时结构校验与诊断。

use std::fmt;

// ─── BuildError ──────────────────────────────────────────────

/// 构建时结构校验错误。
///
/// 仅验证图的结构性正确性：节点存在、边引用有效、入口/出口存在、Fallback 不指向自身。
/// **绝不产生 Warning。** Warning 迁移至 `GraphDiagnostics`。
#[derive(Debug, Clone)]
pub enum BuildError {
    /// 节点 ID 重复（后者覆盖前者）
    DuplicateNode { id: String },
    /// 边引用了不存在的节点
    MissingNode { from: String, to: String },
    /// 未指定入口节点
    MissingEntryPoint,
    /// 未指定出口节点
    MissingExitPoint,
    /// 边定义无效
    InvalidEdgeDefinition {
        from: String,
        to: String,
        reason: String,
    },
    /// Fallback 边配置无效（如指向自身 = retry，不是 fallback）
    InvalidFallback { node: String, reason: String },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateNode { id } => write!(f, "duplicate node id: '{}'", id),
            Self::MissingNode { from, to } => {
                write!(
                    f,
                    "edge references non-existent node: '{}' (in {}→{})",
                    to, from, to
                )
            }
            Self::MissingEntryPoint => write!(f, "entry point not set"),
            Self::MissingExitPoint => write!(f, "exit point not set"),
            Self::InvalidEdgeDefinition { from, to, reason } => {
                write!(f, "invalid edge {}→{}: {}", from, to, reason)
            }
            Self::InvalidFallback { node, reason } => {
                write!(f, "invalid fallback for node '{}': {}", node, reason)
            }
        }
    }
}

/// 构建错误集合 — 支持多错误收集。
#[derive(Debug, Clone, Default)]
pub struct BuildErrors(pub Vec<BuildError>);

impl BuildErrors {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(&mut self, e: BuildError) {
        self.0.push(e);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &BuildError> {
        self.0.iter()
    }
}

impl fmt::Display for BuildErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            write!(f, "no errors")
        } else {
            writeln!(f, "{} error(s):", self.0.len())?;
            for e in &self.0 {
                writeln!(f, "  - {e}")?;
            }
            Ok(())
        }
    }
}

impl std::error::Error for BuildError {}
impl std::error::Error for BuildErrors {}

// ─── GraphDiagnostics ────────────────────────────────────────

/// 诊断严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// 信息性 — 值得注意但不一定有問題
    Info,
    /// 警告 — 潜在风险，建议检查
    Warning,
}

impl fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "info"),
            Self::Warning => write!(f, "warning"),
        }
    }
}

/// 诊断分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCategory {
    /// 环检测
    Cycle,
    /// Fallback 参与循环
    FallbackInCycle,
    /// 不可达路径
    Unreachable,
    /// 条件边重叠
    ConditionOverlap,
    /// End 节点有出边
    EndNodeOutgoing,
    /// 其他
    Other,
}

impl std::fmt::Display for DiagnosticCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle => write!(f, "cycle"),
            Self::FallbackInCycle => write!(f, "fallback-in-cycle"),
            Self::Unreachable => write!(f, "unreachable"),
            Self::ConditionOverlap => write!(f, "condition-overlap"),
            Self::EndNodeOutgoing => write!(f, "end-node-outgoing"),
            Self::Other => write!(f, "other"),
        }
    }
}

/// 单条诊断信息。
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub category: DiagnosticCategory,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] ({}): {}",
            self.severity, self.category, self.message
        )
    }
}

/// 图诊断结果 — 由 `graph.analyze()` 产生。
///
/// 检查风险性问题：环检测、Fallback 参与循环、不可达路径、条件边重叠、End 节点有出边。
#[derive(Debug, Clone, Default)]
pub struct GraphDiagnostics {
    pub warnings: Vec<Diagnostic>,
    pub infos: Vec<Diagnostic>,
}

impl GraphDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_warning(&mut self, category: DiagnosticCategory, message: impl Into<String>) {
        self.warnings.push(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            category,
            message: message.into(),
        });
    }

    pub fn add_info(&mut self, category: DiagnosticCategory, message: impl Into<String>) {
        self.infos.push(Diagnostic {
            severity: DiagnosticSeverity::Info,
            category,
            message: message.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty() && self.infos.is_empty()
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

impl fmt::Display for GraphDiagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.warnings.is_empty() {
            writeln!(f, "{} warning(s):", self.warnings.len())?;
            for w in &self.warnings {
                writeln!(f, "  - {w}")?;
            }
        }
        if !self.infos.is_empty() {
            writeln!(f, "{} info(s):", self.infos.len())?;
            for i in &self.infos {
                writeln!(f, "  - {i}")?;
            }
        }
        if self.is_empty() {
            write!(f, "no issues found")
        } else {
            Ok(())
        }
    }
}
