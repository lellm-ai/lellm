//! 连接状态机。

/// 连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Initializing,
    Ready,
    Closed,
}

impl ConnectionState {
    /// 当前状态是否允许发送请求。
    pub fn allows_request(self) -> bool {
        matches!(self, ConnectionState::Ready)
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::Connecting => write!(f, "Connecting"),
            ConnectionState::Initializing => write!(f, "Initializing"),
            ConnectionState::Ready => write!(f, "Ready"),
            ConnectionState::Closed => write!(f, "Closed"),
        }
    }
}
