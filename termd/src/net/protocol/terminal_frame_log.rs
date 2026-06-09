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
#[derive(Debug, Clone)]
pub(super) struct SessionTerminalFrameLog {
    frames: VecDeque<TerminalFramePayload>,
    base_seq: u64,
    size: TerminalSize,
    screen: Option<TerminalScreen>,
    has_sequence_gap: bool,
    has_observed_terminal_state: bool,
    full_snapshot_authoritative: bool,
    live_bootstrap_authoritative: bool,
    pending_post_resize_rebuild: bool,
}

impl Default for SessionTerminalFrameLog {
    fn default() -> Self {
        Self {
            frames: VecDeque::new(),
            base_seq: 0,
            size: TerminalSize::new(0, 0),
            screen: None,
            has_sequence_gap: false,
            has_observed_terminal_state: false,
            full_snapshot_authoritative: false,
            // 中文注释：当前 daemon 新建出来的 session，screen 的真实起点就是空终端，
            // 因此连续 live frame 可以把 mirror 自举成权威 full snapshot 基线。
            live_bootstrap_authoritative: true,
            pending_post_resize_rebuild: false,
        }
    }
}

impl SessionTerminalFrameLog {
    pub(super) fn ensure_initialized(&mut self, size: TerminalSize) {
        if self.screen.is_none() {
            self.size = size;
            self.screen = Some(TerminalScreen::new(size.rows, size.cols));
        }
    }

    pub(super) fn sync_size(&mut self, size: TerminalSize) {
        self.size = size;
        if let Some(screen) = &mut self.screen {
            screen.resize(size.rows, size.cols);
        }
        // 中文注释：protocol 层主动同步尺寸通常意味着“runtime 已经 resize 了，但我们
        // 还没拿到能证明 resize 后整屏状态的权威 terminal snapshot/frame”。
        // 此时不能继续把旧 screen 当成新尺寸下的 full snapshot 基线；否则 reopen
        // 会把 resize 前内容按新 rows/cols 重新编码，画面就会错位。
        self.full_snapshot_authoritative = false;
        // 中文注释：尺寸变化后，只有后续连续 live redraw 才能把 mirror 重新变成
        // 权威整屏。恢复 tmux session 不能靠 capture-pane seed 回来，因此这里
        // 要显式记住“等待 resize 后 redraw 重建基线”这个状态。
        self.pending_post_resize_rebuild = true;
    }

    #[cfg(test)]
    pub(super) fn disable_live_bootstrap_authority(&mut self) {
        // 中文注释：恢复出来的 tmux session 在 daemon 看见第一条 live output 前，
        // 实际 screen 早就已经存在。此时哪怕 output seq 连续，也不能把“空 screen
        // + 增量 output”当成权威 full snapshot 基线；否则 reopen 旧 TUI 会把增量
        // 输出叠到错误起点上，切回时就会乱屏。
        self.live_bootstrap_authoritative = false;
    }

    pub(super) fn live_bootstrap_snapshot_cursor(&self) -> Option<(u64, TerminalSize)> {
        if !self.live_bootstrap_authoritative
            || !self.has_observed_terminal_state
            || self.has_sequence_gap
        {
            return None;
        }
        Some((self.base_seq, self.size))
    }

    pub(super) fn pending_post_resize_rebuild(&self) -> bool {
        self.pending_post_resize_rebuild
    }

    fn reset_from_snapshot(&mut self, base_seq: u64, size: TerminalSize, data: &[u8]) {
        if base_seq < self.base_seq {
            return;
        }
        self.frames.clear();
        self.base_seq = base_seq;
        self.size = size;
        self.has_sequence_gap = false;
        self.has_observed_terminal_state = true;
        self.full_snapshot_authoritative = true;
        // 中文注释：一旦 mirror 接受过外部 snapshot（runtime/daemon fallback）seed，
        // 后续 full snapshot 就不能再假设“空屏 + 连续 live output”足够重放出
        // 当前整屏。否则 protocol 层若改用 raw-output history，会和这个 snapshot
        // 基线发生正文/seq 失配。
        self.live_bootstrap_authoritative = false;
        self.pending_post_resize_rebuild = false;
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
                let had_observed_terminal_state = self.has_observed_terminal_state;
                let was_full_snapshot_authoritative = self.full_snapshot_authoritative;
                let was_pending_post_resize_rebuild = self.pending_post_resize_rebuild;
                if *terminal_seq <= self.base_seq {
                    return false;
                }
                if *terminal_seq != self.base_seq.saturating_add(1) {
                    // 中文注释：daemon mirror 只能在 session terminal_seq 连续时产出
                    // 权威恢复数据。发现 gap 后本地 log 不再生成 snapshot/tail，
                    // 后续恢复必须回源 supervisor，已保留的 frame 只用于 cursor/唤醒判断。
                    self.has_sequence_gap = true;
                    self.live_bootstrap_authoritative = false;
                    self.full_snapshot_authoritative = false;
                    self.pending_post_resize_rebuild = false;
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
                self.has_observed_terminal_state = true;
                if was_full_snapshot_authoritative
                    || (!had_observed_terminal_state && self.live_bootstrap_authoritative)
                    || was_pending_post_resize_rebuild
                {
                    self.full_snapshot_authoritative = true;
                    self.pending_post_resize_rebuild = false;
                }
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
                    self.live_bootstrap_authoritative = false;
                    self.full_snapshot_authoritative = false;
                    self.pending_post_resize_rebuild = false;
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
                self.has_observed_terminal_state = true;
                // 中文注释：resize 事件只说明终端几何尺寸发生变化，不代表 resize 后的
                // 当前整屏已经被完整重画。这里必须先撤销权威 full snapshot，等下一批
                // 连续 output/redraw 到达后再把 mirror 重新升级为权威基线。
                self.full_snapshot_authoritative = false;
                self.pending_post_resize_rebuild = true;
                true
            }
            TerminalFramePayload::Exit { terminal_seq, .. } => {
                let was_full_snapshot_authoritative = self.full_snapshot_authoritative;
                let was_pending_post_resize_rebuild = self.pending_post_resize_rebuild;
                if *terminal_seq <= self.base_seq {
                    return false;
                }
                if *terminal_seq != self.base_seq.saturating_add(1) {
                    self.has_sequence_gap = true;
                    self.live_bootstrap_authoritative = false;
                    self.full_snapshot_authoritative = false;
                    self.pending_post_resize_rebuild = false;
                    self.base_seq = self.base_seq.max(*terminal_seq);
                    return true;
                }
                self.base_seq = self.base_seq.max(*terminal_seq);
                self.has_observed_terminal_state = true;
                self.pending_post_resize_rebuild = false;
                if was_full_snapshot_authoritative && !was_pending_post_resize_rebuild {
                    self.full_snapshot_authoritative = true;
                } else {
                    self.full_snapshot_authoritative = false;
                }
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
        if !self.has_observed_terminal_state {
            // 中文注释：daemon 可能已经为某个 session 分配了空 screen/尺寸，但只要
            // 还没见过任何 terminal frame，就不能宣称自己知道“当前画面”。
            // 否则 reopen 一个已有输出但尚未被 daemon 消费的 session，会错误返回空快照。
            return None;
        }
        if last_terminal_seq.is_none() {
            // 中文注释：`None` 表示“我要一份完整当前画面”。这里不再一刀切地
            // 回源 runtime；只要 daemon mirror 已经被证明是权威 screen，就可以
            // 直接从 mirror 产出 snapshot，避免 tmux capture-pane 把全屏 TUI
            // 错当成纯文本导致 reopen 乱屏。
            return self.snapshot_from_mirror(session_id);
        }
        if self.has_sequence_gap {
            return None;
        }
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
                        // 中文注释：tail 一旦跨过 resize，前端必须重新拿完整 screen。
                        // 但如果当前 mirror 还没被证明是完整画面，就必须回源 runtime。
                        return self.snapshot_from_mirror(session_id);
                    }
                    if let Some(max_frames) = max_frames {
                        tail.truncate(max_frames);
                    }
                    return Some(tail);
                }
            }
        }

        self.snapshot_from_mirror(session_id)
    }

    fn snapshot_from_mirror(&self, session_id: SessionId) -> Option<Vec<TerminalFramePayload>> {
        if !self.full_snapshot_authoritative || self.has_sequence_gap {
            return None;
        }
        let screen = self.screen.as_ref()?;
        Some(vec![TerminalFramePayload::Snapshot {
            session_id,
            base_seq: self.base_seq,
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

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose};

    use super::*;

    fn output_frame(
        session_id: SessionId,
        terminal_seq: u64,
        bytes: &[u8],
    ) -> TerminalFramePayload {
        TerminalFramePayload::Output {
            session_id,
            terminal_seq,
            data_base64: general_purpose::STANDARD.encode(bytes),
        }
    }

    fn snapshot_frame(
        session_id: SessionId,
        base_seq: u64,
        size: TerminalSize,
        bytes: &[u8],
    ) -> TerminalFramePayload {
        TerminalFramePayload::Snapshot {
            session_id,
            base_seq,
            size,
            data_base64: general_purpose::STANDARD.encode(bytes),
        }
    }

    #[test]
    fn restored_tmux_log_stays_non_authoritative_after_resize_without_redraw() {
        let session_id = SessionId::new();
        let mut log = SessionTerminalFrameLog::default();
        log.ensure_initialized(TerminalSize::new(24, 80));
        log.disable_live_bootstrap_authority();

        log.sync_size(TerminalSize::new(40, 120));

        assert!(
            log.snapshot_or_tail(session_id, None).is_none(),
            "仅同步尺寸、没有收到 resize 后 redraw 时，restored tmux mirror 不能直接宣布自己掌握完整当前画面"
        );
    }

    #[test]
    fn restored_tmux_log_rebuilds_authority_after_resize_then_continuous_output() {
        let session_id = SessionId::new();
        let mut log = SessionTerminalFrameLog::default();
        log.ensure_initialized(TerminalSize::new(24, 80));
        log.disable_live_bootstrap_authority();
        log.sync_size(TerminalSize::new(40, 120));

        log.push(output_frame(
            session_id,
            1,
            b"\x1b[2J\x1b[Hresize-redraw-ready",
        ));

        let frames = log
            .snapshot_or_tail(session_id, None)
            .expect("resize 后连续 redraw 应重建权威 full snapshot");
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            TerminalFramePayload::Snapshot { base_seq, size, .. } => {
                assert_eq!(*base_seq, 1);
                assert_eq!(*size, TerminalSize::new(40, 120));
            }
            other => panic!("expected rebuilt full snapshot, got {other:?}"),
        }
    }

    #[test]
    fn restored_tmux_log_does_not_rebuild_authority_across_sequence_gap() {
        let session_id = SessionId::new();
        let mut log = SessionTerminalFrameLog::default();
        log.ensure_initialized(TerminalSize::new(24, 80));
        log.disable_live_bootstrap_authority();
        log.sync_size(TerminalSize::new(40, 120));

        log.push(output_frame(
            session_id,
            2,
            b"\x1b[2J\x1b[Hgap-redraw-should-not-promote",
        ));

        assert!(
            log.snapshot_or_tail(session_id, None).is_none(),
            "resize 后如果 terminal_seq 断档，mirror 不能误把不完整 redraw 升成权威整屏"
        );
    }

    #[test]
    fn snapshot_seed_disables_live_bootstrap_history_cursor() {
        let session_id = SessionId::new();
        let mut log = SessionTerminalFrameLog::default();
        let size = TerminalSize::new(24, 80);
        log.ensure_initialized(size);

        log.push(snapshot_frame(
            session_id,
            3,
            size,
            b"seeded-from-runtime-snapshot",
        ));

        assert!(
            log.live_bootstrap_snapshot_cursor().is_none(),
            "一旦 daemon log 接受过外部 snapshot seed，就不能再把 raw-output history 当成 live-bootstrap 权威快照源"
        );
    }
}
