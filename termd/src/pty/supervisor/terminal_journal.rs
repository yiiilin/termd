use std::collections::VecDeque;

use crate::net::screen::TerminalScreen;

use super::super::{PtySize, PtyTerminalFrame};

pub(super) const TERMINAL_JOURNAL_MAX_EVENTS: usize = 8192;
pub(super) const TERMINAL_ATTACH_TAIL_MAX_BYTES: usize = 128 * 1024;
const TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO: usize = 2;

/// supervisor 侧的权威终端缓存。
///
/// 中文注释：daemon 可以重启、浏览器可以断线，但这个结构跟真实 PTY 在同一个
/// session supervisor 进程里，因此它是恢复 screen snapshot 和 tail 的权威来源。
pub(super) struct SupervisorTerminalCache {
    // 中文注释：session 级终端事件序号，output/resize/exit 共用；从 1 开始递增。
    next_terminal_seq: u64,
    // 中文注释：当前 journal 窗口中第一条事件的 terminal_seq；用于判断客户端 tail 是否过旧。
    journal_base_seq: u64,
    // 中文注释：最近原始终端事件，用于 snapshot 之后补 tail；snapshot 本身不进入 journal。
    journal: VecDeque<TerminalEvent>,
    // 中文注释：权威终端模拟状态，内部保留最近 1000 行热历史。
    screen: TerminalScreen,
    size: PtySize,
}

/// supervisor journal 中的原始终端事件。
///
/// 中文注释：这里保存 session 级 `terminal_seq`，不是 WebSocket packet seq。
/// snapshot 只是一张状态图，不进入 journal；tail 只由这些事件构成。
#[derive(Clone, Debug, Eq, PartialEq)]
enum TerminalEvent {
    Output { seq: u64, bytes: Vec<u8> },
    Resize { seq: u64, size: PtySize },
    Exit { seq: u64, code: Option<i32> },
}

impl TerminalEvent {
    fn terminal_seq(&self) -> u64 {
        match self {
            Self::Output { seq, .. } | Self::Resize { seq, .. } | Self::Exit { seq, .. } => *seq,
        }
    }

    fn to_terminal_frame(&self) -> PtyTerminalFrame {
        match self {
            Self::Output { seq, bytes } => PtyTerminalFrame::Output {
                terminal_seq: *seq,
                data: bytes.clone(),
            },
            Self::Resize { seq, size } => PtyTerminalFrame::Resize {
                terminal_seq: *seq,
                size: *size,
            },
            Self::Exit { seq, code } => PtyTerminalFrame::Exit {
                terminal_seq: *seq,
                code: *code,
            },
        }
    }

    fn replay_cost_bytes(&self) -> usize {
        match self {
            Self::Output { bytes, .. } => bytes.len(),
            Self::Resize { .. } | Self::Exit { .. } => 1,
        }
    }
}

impl SupervisorTerminalCache {
    pub(super) fn new(size: PtySize) -> Self {
        Self {
            next_terminal_seq: 1,
            journal_base_seq: 1,
            journal: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
            size,
        }
    }

    pub(super) fn record_output(&mut self, bytes: &[u8]) -> PtyTerminalFrame {
        // supervisor 是 PTY 真正存活的一侧；屏幕快照必须在这里维护，不能只放在 daemon 内存中。
        self.screen.apply(bytes);
        let event = TerminalEvent::Output {
            seq: self.allocate_terminal_seq(),
            bytes: bytes.to_vec(),
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    pub(super) fn resize(&mut self, size: PtySize) -> PtyTerminalFrame {
        self.size = size;
        self.screen.resize(size.rows, size.cols);
        let event = TerminalEvent::Resize {
            seq: self.allocate_terminal_seq(),
            size,
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    pub(super) fn record_exit(&mut self, code: Option<i32>) -> PtyTerminalFrame {
        let event = TerminalEvent::Exit {
            seq: self.allocate_terminal_seq(),
            code,
        };
        let frame = event.to_terminal_frame();
        self.push_journal(event);
        frame
    }

    pub(super) fn snapshot_output(&self) -> Vec<u8> {
        self.screen.snapshot_bytes()
    }

    pub(super) fn terminal_snapshot_or_tail(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        let current_seq = self.current_terminal_seq();
        if let Some(last_terminal_seq) = last_terminal_seq {
            if last_terminal_seq == current_seq {
                return (current_seq, Vec::new());
            }
            if last_terminal_seq < current_seq
                && last_terminal_seq.saturating_add(1) >= self.journal_base_seq
            {
                let tail_events = self
                    .journal
                    .iter()
                    .filter(|event| event.terminal_seq() > last_terminal_seq)
                    .collect::<Vec<_>>();
                if self.should_replay_attach_tail(&tail_events) {
                    let frames = tail_events
                        .into_iter()
                        .map(TerminalEvent::to_terminal_frame)
                        .collect();
                    return (current_seq, frames);
                }
            }
        }

        (
            current_seq,
            vec![PtyTerminalFrame::Snapshot {
                base_seq: current_seq,
                size: self.size,
                data: self.snapshot_output(),
            }],
        )
    }

    pub(super) fn size(&self) -> PtySize {
        self.size
    }

    #[cfg(test)]
    pub(super) fn journal_len(&self) -> usize {
        self.journal.len()
    }

    fn should_replay_attach_tail(&self, tail_events: &[&TerminalEvent]) -> bool {
        if tail_events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Resize { .. }))
        {
            // 中文注释：resize 会改变后续字节的换行和光标解释；跨 resize 的恢复必须
            // 使用当前 screen snapshot，不能让客户端按旧尺寸 replay tail。
            return false;
        }
        let tail_bytes = tail_events
            .iter()
            .map(|event| event.replay_cost_bytes())
            .sum::<usize>();
        if tail_bytes <= TERMINAL_ATTACH_TAIL_MAX_BYTES {
            return true;
        }

        // 中文注释：客户端 last_terminal_seq 很旧但仍落在 journal 内时，逐事件 tail
        // 可能比当前 screen snapshot 大很多。此时返回权威 snapshot 更符合 attach 语义，
        // 也避免几千个小 output frame 在 WebSocket 层膨胀成数百 KB 的单次发送。
        let snapshot_bytes = self.snapshot_output().len();
        tail_bytes <= snapshot_bytes.saturating_mul(TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO)
    }

    fn current_terminal_seq(&self) -> u64 {
        self.next_terminal_seq.saturating_sub(1)
    }

    fn allocate_terminal_seq(&mut self) -> u64 {
        let seq = self.next_terminal_seq;
        self.next_terminal_seq = self.next_terminal_seq.saturating_add(1).max(seq + 1);
        seq
    }

    fn push_journal(&mut self, event: TerminalEvent) {
        self.journal.push_back(event);
        while self.journal.len() > TERMINAL_JOURNAL_MAX_EVENTS {
            self.journal.pop_front();
        }
        self.journal_base_seq = self
            .journal
            .front()
            .map(TerminalEvent::terminal_seq)
            .unwrap_or(self.next_terminal_seq);
    }
}

/// daemon 侧的 supervisor 终端镜像缓存。
///
/// 中文注释：它不是权威状态源，只是 supervisor 权威状态的 read replica。supervisor
/// IPC 重连时用 `AttachSync` 的 snapshot/base_seq 重置；live frame 到达 daemon 后必须先
/// 喂给这个 mirror，再进入 pending 队列和协议层 room fanout。
pub(super) struct SupervisorTerminalMirror {
    current_terminal_seq: u64,
    journal_base_seq: u64,
    journal: VecDeque<TerminalEvent>,
    screen: TerminalScreen,
    size: PtySize,
}

impl SupervisorTerminalMirror {
    pub(super) fn new(size: PtySize) -> Self {
        Self {
            current_terminal_seq: 0,
            journal_base_seq: 1,
            journal: VecDeque::new(),
            screen: TerminalScreen::new(size.rows, size.cols),
            size,
        }
    }

    pub(super) fn reset_from_snapshot(&mut self, size: PtySize, base_seq: u64, bytes: &[u8]) {
        if base_seq < self.current_terminal_seq {
            return;
        }
        self.current_terminal_seq = base_seq;
        self.journal_base_seq = base_seq.saturating_add(1);
        self.journal.clear();
        self.size = size;
        self.screen = TerminalScreen::new(size.rows, size.cols);
        self.screen.apply(bytes);
    }

    pub(super) fn apply_snapshot_and_tail(
        &mut self,
        size: PtySize,
        base_seq: u64,
        bytes: &[u8],
        frames: &[PtyTerminalFrame],
    ) {
        self.reset_from_snapshot(size, base_seq, bytes);
        for frame in frames {
            self.apply_frame(frame);
        }
    }

    pub(super) fn apply_frame(&mut self, frame: &PtyTerminalFrame) -> bool {
        match frame {
            PtyTerminalFrame::Snapshot {
                base_seq,
                size,
                data,
            } => {
                if *base_seq < self.current_terminal_seq {
                    return false;
                }
                self.reset_from_snapshot(*size, *base_seq, data);
                true
            }
            PtyTerminalFrame::Output { terminal_seq, data } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.screen.apply(data);
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Output {
                    seq: *terminal_seq,
                    bytes: data.clone(),
                });
                true
            }
            PtyTerminalFrame::Resize { terminal_seq, size } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.size = *size;
                self.screen.resize(size.rows, size.cols);
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Resize {
                    seq: *terminal_seq,
                    size: *size,
                });
                true
            }
            PtyTerminalFrame::Exit { terminal_seq, code } => {
                if *terminal_seq <= self.current_terminal_seq {
                    return false;
                }
                self.current_terminal_seq = *terminal_seq;
                self.push_journal(TerminalEvent::Exit {
                    seq: *terminal_seq,
                    code: *code,
                });
                true
            }
        }
    }

    pub(super) fn terminal_snapshot_or_tail(
        &self,
        last_terminal_seq: Option<u64>,
    ) -> (u64, Vec<PtyTerminalFrame>) {
        let current_seq = self.current_terminal_seq;
        if let Some(last_terminal_seq) = last_terminal_seq {
            if last_terminal_seq == current_seq {
                return (current_seq, Vec::new());
            }
            if last_terminal_seq < current_seq
                && last_terminal_seq.saturating_add(1) >= self.journal_base_seq
            {
                let tail_events = self
                    .journal
                    .iter()
                    .filter(|event| event.terminal_seq() > last_terminal_seq)
                    .collect::<Vec<_>>();
                if self.should_replay_attach_tail(&tail_events) {
                    return (
                        current_seq,
                        tail_events
                            .into_iter()
                            .map(TerminalEvent::to_terminal_frame)
                            .collect(),
                    );
                }
            }
        }

        (
            current_seq,
            vec![PtyTerminalFrame::Snapshot {
                base_seq: current_seq,
                size: self.size,
                data: self.screen.snapshot_bytes(),
            }],
        )
    }

    fn push_journal(&mut self, event: TerminalEvent) {
        self.journal.push_back(event);
        while self.journal.len() > TERMINAL_JOURNAL_MAX_EVENTS {
            self.journal.pop_front();
        }
        self.journal_base_seq = self
            .journal
            .front()
            .map(TerminalEvent::terminal_seq)
            .unwrap_or_else(|| self.current_terminal_seq.saturating_add(1));
    }

    fn should_replay_attach_tail(&self, tail_events: &[&TerminalEvent]) -> bool {
        if tail_events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Resize { .. }))
        {
            // 中文注释：daemon mirror 是 supervisor snapshot 的副本，也必须保持同样的
            // resize rebase 语义，避免 supervisor 重连前后恢复规则不一致。
            return false;
        }
        let tail_bytes = tail_events
            .iter()
            .map(|event| event.replay_cost_bytes())
            .sum::<usize>();
        if tail_bytes <= TERMINAL_ATTACH_TAIL_MAX_BYTES {
            return true;
        }

        let snapshot_bytes = self.screen.snapshot_bytes().len();
        tail_bytes <= snapshot_bytes.saturating_mul(TERMINAL_ATTACH_TAIL_SNAPSHOT_RATIO)
    }
}
