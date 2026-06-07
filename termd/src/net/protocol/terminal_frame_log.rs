use std::collections::VecDeque;

use base64::{Engine as _, engine::general_purpose};
use termd_proto::{SessionId, TerminalFramePayload, TerminalSize};

use crate::net::screen::TerminalScreen;

const TERMINAL_LIVE_FRAME_LOG_MAX_FRAMES: usize = 8192;

/// daemon 内的 session 级 terminal live frame 回放窗口。
///
/// 中文注释：supervisor IPC reader 只能把 live frame 放进一个 daemon 侧缓存；如果每条
/// WebSocket 直接 pop 这个缓存，最先 flush 的连接会独占输出，其他窗口/relay client 就会
/// 丢 tail。这里把 live frame 提升成 session 级 retained log，再由每条连接用自己的
/// `next_terminal_seq` cursor 读取，语义上等价于 supervisor snapshot + tail 模型。
#[derive(Debug, Default, Clone)]
pub(super) struct SessionTerminalFrameLog {
    frames: VecDeque<TerminalFramePayload>,
    base_seq: u64,
    size: TerminalSize,
    screen: Option<TerminalScreen>,
    has_sequence_gap: bool,
}

impl SessionTerminalFrameLog {
    pub(super) fn ensure_initialized(&mut self, size: TerminalSize) {
        if self.screen.is_none() {
            self.size = size;
            self.screen = Some(TerminalScreen::new(size.rows, size.cols));
        }
    }

    fn reset_from_snapshot(&mut self, base_seq: u64, size: TerminalSize, data: &[u8]) {
        if base_seq < self.base_seq {
            return;
        }
        self.frames.clear();
        self.base_seq = base_seq;
        self.size = size;
        self.has_sequence_gap = false;
        let mut screen = TerminalScreen::new(size.rows, size.cols);
        screen.apply(data);
        self.screen = Some(screen);
    }

    pub(super) fn push(&mut self, frame: TerminalFramePayload) {
        if !self.apply_to_mirror(&frame) {
            return;
        }
        if frame_is_live_loggable(&frame) {
            self.frames.push_back(frame);
        }
        while self.frames.len() > TERMINAL_LIVE_FRAME_LOG_MAX_FRAMES {
            self.frames.pop_front();
        }
    }

    pub(super) fn seed_from_frames(&mut self, frames: &[TerminalFramePayload]) {
        for frame in frames {
            self.push(frame.clone());
        }
    }

    fn apply_to_mirror(&mut self, frame: &TerminalFramePayload) -> bool {
        match frame {
            TerminalFramePayload::Snapshot {
                base_seq,
                size,
                data_base64,
                ..
            } => {
                if let Ok(bytes) = general_purpose::STANDARD.decode(data_base64) {
                    self.reset_from_snapshot(*base_seq, *size, &bytes);
                    return true;
                }
                false
            }
            TerminalFramePayload::Output {
                terminal_seq,
                data_base64,
                ..
            } => {
                if *terminal_seq <= self.base_seq {
                    return false;
                }
                if *terminal_seq != self.base_seq.saturating_add(1) {
                    // 中文注释：daemon mirror 只能在 session terminal_seq 连续时产出
                    // 权威恢复数据。发现 gap 后本地 log 不再生成 snapshot/tail，
                    // 后续恢复必须回源 supervisor，已保留的 frame 只用于 cursor/唤醒判断。
                    self.has_sequence_gap = true;
                    self.base_seq = self.base_seq.max(*terminal_seq);
                    return true;
                }
                let Ok(bytes) = general_purpose::STANDARD.decode(data_base64) else {
                    return false;
                };
                if let Some(screen) = &mut self.screen {
                    screen.apply(&bytes);
                } else {
                    let mut screen = TerminalScreen::new(self.size.rows, self.size.cols);
                    screen.apply(&bytes);
                    self.screen = Some(screen);
                }
                self.base_seq = self.base_seq.max(*terminal_seq);
                true
            }
            TerminalFramePayload::Resize {
                terminal_seq, size, ..
            } => {
                if *terminal_seq <= self.base_seq {
                    return false;
                }
                if *terminal_seq != self.base_seq.saturating_add(1) {
                    self.has_sequence_gap = true;
                    self.base_seq = self.base_seq.max(*terminal_seq);
                    return true;
                }
                self.size = *size;
                if let Some(screen) = &mut self.screen {
                    screen.resize(size.rows, size.cols);
                } else {
                    self.screen = Some(TerminalScreen::new(size.rows, size.cols));
                }
                self.base_seq = self.base_seq.max(*terminal_seq);
                true
            }
            TerminalFramePayload::Exit { terminal_seq, .. } => {
                if *terminal_seq <= self.base_seq {
                    return false;
                }
                if *terminal_seq != self.base_seq.saturating_add(1) {
                    self.has_sequence_gap = true;
                    self.base_seq = self.base_seq.max(*terminal_seq);
                    return true;
                }
                self.base_seq = self.base_seq.max(*terminal_seq);
                true
            }
            TerminalFramePayload::Batch { frames, .. } => {
                let mut applied = false;
                for frame in frames {
                    applied |= self.apply_to_mirror(frame);
                }
                applied
            }
        }
    }

    pub(super) fn has_from(&self, next_terminal_seq: u64) -> bool {
        self.frames.iter().any(|frame| {
            frame
                .terminal_seq()
                .is_some_and(|seq| seq >= next_terminal_seq)
        })
    }

    pub(super) fn snapshot_or_tail(
        &self,
        session_id: SessionId,
        last_terminal_seq: Option<u64>,
    ) -> Option<Vec<TerminalFramePayload>> {
        self.snapshot_or_tail_limited(session_id, last_terminal_seq, None)
    }

    pub(super) fn snapshot_or_tail_limited(
        &self,
        session_id: SessionId,
        last_terminal_seq: Option<u64>,
        max_frames: Option<usize>,
    ) -> Option<Vec<TerminalFramePayload>> {
        if last_terminal_seq.is_none() {
            // 中文注释：`None` 是客户端明确请求权威 full snapshot 的语义。
            // daemon mirror 只保留 live tail 和当前 screen，不能代表 tmux 的完整
            // scrollback；这里必须让 protocol 回源 runtime/tmux capture。
            return None;
        }
        if self.has_sequence_gap {
            return None;
        }
        let screen = self.screen.as_ref()?;
        let current_seq = self.base_seq;
        if let Some(last_terminal_seq) = last_terminal_seq {
            if last_terminal_seq == current_seq {
                return Some(Vec::new());
            }
            if last_terminal_seq < current_seq {
                let mut tail = self
                    .frames
                    .iter()
                    .filter(|frame| {
                        frame
                            .terminal_seq()
                            .is_some_and(|seq| seq > last_terminal_seq)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let first_seq = tail.first().and_then(TerminalFramePayload::terminal_seq);
                if first_seq == Some(last_terminal_seq.saturating_add(1)) {
                    if terminal_frame_list_crosses_resize(&tail) {
                        return Some(vec![TerminalFramePayload::Snapshot {
                            session_id,
                            base_seq: current_seq,
                            size: self.size,
                            data_base64: general_purpose::STANDARD.encode(screen.snapshot_bytes()),
                        }]);
                    }
                    if let Some(max_frames) = max_frames {
                        tail.truncate(max_frames);
                    }
                    return Some(tail);
                }
            }
        }

        Some(vec![TerminalFramePayload::Snapshot {
            session_id,
            base_seq: current_seq,
            size: self.size,
            data_base64: general_purpose::STANDARD.encode(screen.snapshot_bytes()),
        }])
    }
}

pub(super) fn terminal_frame_list_crosses_resize(frames: &[TerminalFramePayload]) -> bool {
    frames.iter().any(|frame| match frame {
        TerminalFramePayload::Resize { .. } => true,
        TerminalFramePayload::Batch { frames, .. } => terminal_frame_list_crosses_resize(frames),
        TerminalFramePayload::Snapshot { .. }
        | TerminalFramePayload::Output { .. }
        | TerminalFramePayload::Exit { .. } => false,
    })
}

pub(super) fn terminal_frame_covered_seq(frame: &TerminalFramePayload) -> Option<u64> {
    match frame {
        TerminalFramePayload::Snapshot { base_seq, .. } => Some(*base_seq),
        TerminalFramePayload::Output { terminal_seq, .. }
        | TerminalFramePayload::Resize { terminal_seq, .. }
        | TerminalFramePayload::Exit { terminal_seq, .. } => Some(*terminal_seq),
        TerminalFramePayload::Batch { frames, .. } => {
            frames.iter().filter_map(terminal_frame_covered_seq).max()
        }
    }
}

fn frame_is_live_loggable(frame: &TerminalFramePayload) -> bool {
    match frame {
        TerminalFramePayload::Output { .. }
        | TerminalFramePayload::Resize { .. }
        | TerminalFramePayload::Exit { .. } => true,
        TerminalFramePayload::Batch { frames, .. } => frames.iter().any(frame_is_live_loggable),
        TerminalFramePayload::Snapshot { .. } => false,
    }
}
