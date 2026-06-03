//! SSE Parser — bytes → SseFrame 协议解析。
//!
//! 职责：换行、空行检测、截断缓冲、拼包。
//! 完全不知道 ToolCall、Usage、TextDelta 等业务概念。

use super::sse_frame::SseFrame;

/// SSE 协议解析器 — 维护行缓冲区 + 帧状态，处理 bytes_stream() 的截断问题。
pub struct SseParser {
    /// 不完整的行尾部
    buffer: String,
    /// 当前帧的 event 字段（跨 chunk 持久化）
    frame_event: Option<String>,
    /// 当前帧的 data 字段（跨 chunk 持久化）
    frame_data: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            frame_event: None,
            frame_data: None,
        }
    }

    /// 喂入新的字节块，返回解析出的完整 SseFrame 列表。
    ///
    /// 不完整的帧保留在状态中，等待下一块数据补齐。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let text = String::from_utf8_lossy(chunk);
        self.buffer.push_str(&text);

        let buffer = std::mem::take(&mut self.buffer);
        let mut lines: Vec<String> = Vec::new();
        let mut rest = buffer.as_str();
        while let Some(pos) = rest.find('\n') {
            lines.push(rest[..pos].to_string());
            rest = &rest[pos + 1..];
        }
        self.buffer = rest.to_string();

        self.parse_frames(lines)
    }

    /// 按 SSE 帧解析：event: / data: / 空行
    /// frame_event 和 frame_data 在 struct 上维护，以支持跨 chunk 的帧拼接。
    fn parse_frames(&mut self, lines: Vec<String>) -> Vec<SseFrame> {
        let mut frames = Vec::new();

        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // 空行 = SSE 帧边界，提交事件
                if let Some(data) = self.frame_data.take() {
                    frames.push(SseFrame {
                        event: self.frame_event.take(),
                        data,
                    });
                }
                continue;
            }

            if let Some((key, value)) = Self::parse_sse_line(trimmed) {
                match key {
                    "event" => {
                        self.frame_event = Some(value.to_string());
                    }
                    "data" => {
                        if value.is_empty() {
                            continue;
                        }
                        self.frame_data
                            .get_or_insert_with(String::new)
                            .push_str(value);
                    }
                    _ => {} // id:, retry: 等忽略
                }
            }
        }

        frames
    }

    /// 解析一行 SSE 字段，返回 (key, value)。
    fn parse_sse_line(line: &str) -> Option<(&str, &str)> {
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim();
            let value = line[pos + 1..].trim_start_matches(' ');
            Some((key, value))
        } else {
            None
        }
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_frame() {
        let mut parser = SseParser::new();
        let frames = parser.feed(b"event: message_delta\ndata: {\"text\": \"hello\"}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, Some("message_delta".into()));
        assert_eq!(frames[0].data, "{\"text\": \"hello\"}");
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut parser = SseParser::new();

        // 第一块：完整的 event 和 data 行，但没有空行终止
        let frames1 = parser.feed(b"event: delta\ndata: {\"t");
        assert_eq!(frames1.len(), 0);

        // 第二块：补齐 data 并添加空行终止
        let frames2 = parser.feed(b": \"hello\"}\n\n");
        assert_eq!(frames2.len(), 1);
        assert_eq!(frames2[0].event, Some("delta".into()));
        assert_eq!(frames2[0].data, "{\"t: \"hello\"}");
    }

    #[test]
    fn multi_line_data() {
        let mut parser = SseParser::new();
        let frames = parser.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(frames.len(), 1);
        // 多行 data 拼接
        assert_eq!(frames[0].data, "line1line2");
    }

    #[test]
    fn data_only_no_event() {
        let mut parser = SseParser::new();
        let frames = parser.feed(b"data: [DONE]\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].data, "[DONE]");
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let mut parser = SseParser::new();
        let frames = parser.feed(b"data: f1\n\ndata: f2\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "f1");
        assert_eq!(frames[1].data, "f2");
    }
}
