//! StreamEmitter — 数据面发射器。
//!
//! 不直接暴露 Sender，未来可扩展 emit_batch()、emit_throttled()、emit_if_subscribed() 等。

use tokio::sync::mpsc;

use crate::stream_chunk::StreamChunk;

/// 数据面发射器。
///
/// 封装 mpsc::Sender<StreamChunk>，提供统一的发射接口。
pub struct StreamEmitter {
    tx: mpsc::Sender<StreamChunk>,
}

impl Clone for StreamEmitter {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl StreamEmitter {
    /// 创建新的 StreamEmitter。
    pub fn new(tx: mpsc::Sender<StreamChunk>) -> Self {
        Self { tx }
    }

    /// 发射数据面事件。
    pub fn emit(&self, chunk: StreamChunk) {
        // 非阻塞发送，失败则静默丢弃
        let _ = self.tx.try_send(chunk);
    }

    /// 发射数据面事件（异步，可能阻塞）。
    pub async fn emit_async(
        &self,
        chunk: StreamChunk,
    ) -> Result<(), mpsc::error::TrySendError<StreamChunk>> {
        self.tx.send(chunk).await.map_err(|e| match e {
            mpsc::error::SendError(chunk) => mpsc::error::TrySendError::Full(chunk),
        })
    }

    /// 获取底层 Sender 的 clone（用于子组件）。
    pub fn sender(&self) -> mpsc::Sender<StreamChunk> {
        self.tx.clone()
    }
}
