//! PTY 逻辑终端缓存的轻量模型。
//!
//! 这里不是完整终端模拟器，只维护 daemon 重新 attach 时需要的最近 1000 行逻辑内容。
//! 样式变化会落到 cell 上，不单独消耗行数；实时连接仍接收原始 PTY 字节。

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

const DEFAULT_MAX_CACHED_LINES: usize = 1000;

#[derive(Debug, Clone)]
pub(crate) struct TerminalScreen {
    rows: usize,
    cols: usize,
    max_cached_lines: usize,
    lines: VecDeque<TerminalLine>,
    next_line_index: u64,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    current_style: CellStyle,
    parser: ParserState,
    utf8_pending: Vec<u8>,
    normal_screen: Option<SavedScreen>,
    requires_snapshot_redraw: bool,
    scroll_top: usize,
    scroll_bottom: usize,
}

#[derive(Debug, Clone)]
struct TerminalLine {
    #[cfg_attr(not(test), allow(dead_code))]
    index: u64,
    cells: Vec<TerminalCell>,
    touched: bool,
}

#[derive(Debug, Clone)]
struct SavedScreen {
    lines: VecDeque<TerminalLine>,
    next_line_index: u64,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    current_style: CellStyle,
    scroll_top: usize,
    scroll_bottom: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalCell {
    character: char,
    style: CellStyle,
    wide_continuation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CellStyle {
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strike: bool,
    foreground: Option<SgrColor>,
    background: Option<SgrColor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SgrColor {
    Basic(u16),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone)]
enum ParserState {
    Ground,
    Escape,
    Osc,
    OscEscape,
    Csi {
        private: bool,
        params: Vec<Option<u16>>,
        current: Option<u16>,
    },
}

impl TerminalLine {
    fn blank(index: u64, cols: usize, touched: bool) -> Self {
        Self {
            index,
            cells: vec![TerminalCell::blank(); cols],
            touched,
        }
    }

    fn resize(&mut self, cols: usize) {
        self.cells.resize(cols, TerminalCell::blank());
    }

    fn clear_with_style(&mut self, style: &CellStyle) {
        self.cells.fill(TerminalCell::styled_blank(style));
        self.touched = true;
    }

    fn is_blank(&self) -> bool {
        self.cells.iter().all(|cell| cell.is_blank_default())
    }

    #[cfg(test)]
    fn plain_text(&self) -> String {
        self.cells
            .iter()
            .filter(|cell| !cell.wide_continuation)
            .map(|cell| cell.character)
            .collect::<String>()
            .trim_end_matches(' ')
            .to_owned()
    }

    fn display_width(&self) -> usize {
        self.cells
            .iter()
            .rposition(|cell| {
                cell.character != ' ' || !cell.style.is_default() || cell.wide_continuation
            })
            .map(|index| index + 1)
            .unwrap_or(0)
    }
}

impl TerminalCell {
    fn blank() -> Self {
        Self {
            character: ' ',
            style: CellStyle::default(),
            wide_continuation: false,
        }
    }

    fn styled_blank(style: &CellStyle) -> Self {
        Self {
            character: ' ',
            style: style.clone(),
            wide_continuation: false,
        }
    }

    fn wide_continuation(style: &CellStyle) -> Self {
        Self {
            character: ' ',
            style: style.clone(),
            wide_continuation: true,
        }
    }

    fn is_blank_default(&self) -> bool {
        self.character == ' ' && self.style.is_default() && !self.wide_continuation
    }
}

impl CellStyle {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn sgr_bytes(&self, reset_first: bool) -> Vec<u8> {
        if self.is_default() {
            return b"\x1b[0m".to_vec();
        }

        let mut codes = Vec::new();
        if reset_first {
            codes.push("0".to_owned());
        }
        if self.bold {
            codes.push("1".to_owned());
        }
        if self.faint {
            codes.push("2".to_owned());
        }
        if self.italic {
            codes.push("3".to_owned());
        }
        if self.underline {
            codes.push("4".to_owned());
        }
        if self.blink {
            codes.push("5".to_owned());
        }
        if self.inverse {
            codes.push("7".to_owned());
        }
        if self.hidden {
            codes.push("8".to_owned());
        }
        if self.strike {
            codes.push("9".to_owned());
        }
        if let Some(color) = &self.foreground {
            codes.extend(color.sgr_codes(false));
        }
        if let Some(color) = &self.background {
            codes.extend(color.sgr_codes(true));
        }

        format!("\x1b[{}m", codes.join(";")).into_bytes()
    }

    fn apply_sgr(&mut self, params: &[Option<u16>]) {
        let mut values = if params.is_empty() {
            vec![0]
        } else {
            params.iter().map(|value| value.unwrap_or(0)).collect()
        };
        if values.is_empty() {
            values.push(0);
        }

        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.faint = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strike = true,
                22 => {
                    self.bold = false;
                    self.faint = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strike = false,
                30..=37 | 90..=97 => self.foreground = Some(SgrColor::Basic(values[index])),
                39 => self.foreground = None,
                40..=47 | 100..=107 => self.background = Some(SgrColor::Basic(values[index])),
                49 => self.background = None,
                38 | 48 => {
                    if let Some((color, consumed)) = parse_extended_color(&values[index + 1..]) {
                        if values[index] == 38 {
                            self.foreground = Some(color);
                        } else {
                            self.background = Some(color);
                        }
                        index += consumed;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

impl SgrColor {
    fn sgr_codes(&self, background: bool) -> Vec<String> {
        match self {
            Self::Basic(code) => vec![code.to_string()],
            Self::Indexed(index) => vec![
                if background { "48" } else { "38" }.to_owned(),
                "5".to_owned(),
                index.to_string(),
            ],
            Self::Rgb(red, green, blue) => vec![
                if background { "48" } else { "38" }.to_owned(),
                "2".to_owned(),
                red.to_string(),
                green.to_string(),
                blue.to_string(),
            ],
        }
    }
}

impl TerminalScreen {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self::with_max_cached_lines(rows, cols, DEFAULT_MAX_CACHED_LINES)
    }

    pub(crate) fn with_max_cached_lines(rows: u16, cols: u16, max_cached_lines: usize) -> Self {
        let rows = usize::from(rows.max(1));
        let cols = usize::from(cols.max(1));
        let max_cached_lines = max_cached_lines.max(rows).max(1);
        let mut lines = VecDeque::with_capacity(max_cached_lines.min(rows));
        for index in 0..rows {
            lines.push_back(TerminalLine::blank(index as u64, cols, false));
        }
        Self {
            rows,
            cols,
            max_cached_lines,
            lines,
            next_line_index: rows as u64,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            current_style: CellStyle::default(),
            parser: ParserState::Ground,
            utf8_pending: Vec::new(),
            normal_screen: None,
            requires_snapshot_redraw: false,
            scroll_top: 0,
            scroll_bottom: rows,
        }
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let rows = usize::from(rows.max(1));
        let cols = usize::from(cols.max(1));
        self.rows = rows;
        self.cols = cols;
        self.max_cached_lines = self.max_cached_lines.max(rows);
        for line in &mut self.lines {
            line.resize(cols);
        }
        while self.lines.len() < rows {
            self.push_blank_line_with_style(false, &self.current_style.clone());
        }
        self.trim_cached_lines();
        if let Some(normal_screen) = &mut self.normal_screen {
            normal_screen.resize(rows, cols, self.max_cached_lines);
        }
        self.cursor_row = self.cursor_row.min(self.rows - 1);
        self.cursor_col = self.cursor_col.min(self.cols - 1);
        self.scroll_top = self.scroll_top.min(self.rows - 1);
        self.scroll_bottom = self.scroll_bottom.min(self.rows).max(self.scroll_top + 1);
    }

    pub(crate) fn apply(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.apply_byte(*byte);
        }
    }

    pub(crate) fn snapshot_bytes(&self) -> Vec<u8> {
        let Some((start, end)) = self.snapshot_line_bounds() else {
            return Vec::new();
        };

        let mut output = Vec::new();
        for (index, row) in self.lines.iter().enumerate().skip(start).take(end - start) {
            output.extend_from_slice(&row_ansi_bytes(row));
            if index + 1 < end {
                output.extend_from_slice(b"\r\n");
            }
        }
        output.extend_from_slice(
            format!(
                "\x1b[{};{}H",
                self.cursor_row + 1,
                self.cursor_col.min(self.cols - 1) + 1
            )
            .as_bytes(),
        );
        output
    }

    pub(crate) fn cell_count(&self) -> usize {
        self.lines.len().saturating_mul(self.cols)
    }

    #[cfg(test)]
    pub(crate) fn visible_lines(&self) -> Vec<String> {
        self.visible_slice()
            .iter()
            .map(|line| line.plain_text())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn cached_plain_lines(&self) -> Vec<String> {
        self.snapshot_lines()
            .iter()
            .map(|line| line.plain_text())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn cached_line_range(&self) -> Option<(u64, u64)> {
        let lines = self.snapshot_lines();
        Some((lines.first()?.index, lines.last()?.index + 1))
    }

    fn visible_start(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    #[cfg(test)]
    fn visible_slice(&self) -> Vec<&TerminalLine> {
        self.lines.iter().skip(self.visible_start()).collect()
    }

    #[cfg(test)]
    fn snapshot_lines(&self) -> Vec<&TerminalLine> {
        let Some((start, end)) = self.snapshot_line_bounds() else {
            return Vec::new();
        };

        self.lines.iter().skip(start).take(end - start).collect()
    }

    fn snapshot_line_bounds(&self) -> Option<(usize, usize)> {
        let Some(first_visible) = self
            .lines
            .iter()
            .position(|line| line.touched || !line.is_blank())
        else {
            return None;
        };
        let last_visible = self
            .lines
            .iter()
            .rposition(|line| line.touched || !line.is_blank())
            .unwrap_or(first_visible);
        let visible_start = self.visible_start();

        // 如果内容只存在于当前 viewport 的中下部，也要从 viewport 顶部开始回放。
        // 这样绝对光标定位或局部刷新后的第 N 行不会被压到第 1 行。
        let start = if first_visible >= visible_start {
            visible_start
        } else {
            first_visible
        };
        Some((start, last_visible + 1))
    }

    fn visible_line_mut(&mut self, row: usize) -> &mut TerminalLine {
        let index = self.visible_start() + row.min(self.rows - 1);
        self.lines
            .get_mut(index)
            .expect("visible terminal line should exist")
    }

    fn push_blank_line_with_style(&mut self, touched: bool, style: &CellStyle) {
        let index = self.next_line_index;
        self.next_line_index = self.next_line_index.saturating_add(1);
        self.lines.push_back(TerminalLine {
            index,
            cells: vec![TerminalCell::styled_blank(style); self.cols],
            touched,
        });
        self.trim_cached_lines();
    }

    fn trim_cached_lines(&mut self) {
        let max_lines = if self.normal_screen.is_some() {
            self.rows
        } else {
            self.max_cached_lines
        };
        while self.lines.len() > max_lines {
            self.lines.pop_front();
        }
    }

    fn apply_byte(&mut self, byte: u8) {
        let state = std::mem::replace(&mut self.parser, ParserState::Ground);
        match state {
            ParserState::Ground => self.apply_ground_byte(byte),
            ParserState::Escape => self.apply_escape_byte(byte),
            ParserState::Osc => {
                if byte == 0x07 {
                    self.parser = ParserState::Ground;
                } else if byte == 0x1b {
                    self.parser = ParserState::OscEscape;
                } else {
                    self.parser = ParserState::Osc;
                }
            }
            ParserState::OscEscape => {
                self.parser = if byte == b'\\' {
                    ParserState::Ground
                } else {
                    ParserState::Osc
                };
            }
            ParserState::Csi {
                mut private,
                mut params,
                mut current,
            } => {
                if byte == b'?' {
                    private = true;
                    self.parser = ParserState::Csi {
                        private,
                        params,
                        current,
                    };
                } else if byte.is_ascii_digit() {
                    let digit = u16::from(byte - b'0');
                    current = Some(
                        current
                            .unwrap_or(0)
                            .saturating_mul(10)
                            .saturating_add(digit),
                    );
                    self.parser = ParserState::Csi {
                        private,
                        params,
                        current,
                    };
                } else if byte == b';' {
                    params.push(current.take());
                    self.parser = ParserState::Csi {
                        private,
                        params,
                        current,
                    };
                } else if (0x40..=0x7e).contains(&byte) {
                    params.push(current.take());
                    self.parser = ParserState::Ground;
                    self.dispatch_csi(private, &params, byte as char);
                } else {
                    self.parser = ParserState::Csi {
                        private,
                        params,
                        current,
                    };
                }
            }
        }
    }

    fn apply_ground_byte(&mut self, byte: u8) {
        match byte {
            0x1b => {
                self.flush_invalid_utf8();
                self.parser = ParserState::Escape;
            }
            b'\r' => {
                self.flush_invalid_utf8();
                self.cursor_col = 0;
            }
            b'\n' => {
                self.flush_invalid_utf8();
                self.line_feed();
                self.cursor_col = 0;
            }
            0x08 => {
                self.flush_invalid_utf8();
                self.requires_snapshot_redraw = true;
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            b'\t' => {
                self.flush_invalid_utf8();
                self.requires_snapshot_redraw = true;
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            0x00..=0x1f | 0x7f => self.flush_invalid_utf8(),
            0x20..=0x7e => {
                self.flush_invalid_utf8();
                self.print_char(byte as char);
            }
            _ => self.push_utf8_byte(byte),
        }
    }

    fn apply_escape_byte(&mut self, byte: u8) {
        self.parser = match byte {
            b'[' => ParserState::Csi {
                private: false,
                params: Vec::new(),
                current: None,
            },
            b']' => ParserState::Osc,
            b'7' => {
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
                ParserState::Ground
            }
            b'8' => {
                if let Some((row, col)) = self.saved_cursor {
                    self.cursor_row = row.min(self.rows - 1);
                    self.cursor_col = col.min(self.cols - 1);
                }
                ParserState::Ground
            }
            b'c' => {
                self.requires_snapshot_redraw = true;
                self.clear_screen();
                self.cursor_row = 0;
                self.cursor_col = 0;
                ParserState::Ground
            }
            _ => ParserState::Ground,
        };
    }

    fn push_utf8_byte(&mut self, byte: u8) {
        self.utf8_pending.push(byte);
        match std::str::from_utf8(&self.utf8_pending) {
            Ok(text) => {
                let chars = text.chars().collect::<Vec<_>>();
                self.utf8_pending.clear();
                for character in chars {
                    self.print_char(character);
                }
            }
            Err(error) if error.error_len().is_none() && self.utf8_pending.len() <= 4 => {}
            Err(_) => {
                self.utf8_pending.clear();
                self.print_char('\u{fffd}');
            }
        }
    }

    fn flush_invalid_utf8(&mut self) {
        if !self.utf8_pending.is_empty() {
            self.utf8_pending.clear();
            self.print_char('\u{fffd}');
        }
    }

    fn dispatch_csi(&mut self, private: bool, params: &[Option<u16>], action: char) {
        if action != 'm' {
            self.requires_snapshot_redraw = true;
        }
        match action {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(param_or(params, 0, 1)),
            'B' => self.cursor_row = (self.cursor_row + param_or(params, 0, 1)).min(self.rows - 1),
            'C' => self.cursor_col = (self.cursor_col + param_or(params, 0, 1)).min(self.cols - 1),
            'D' => self.cursor_col = self.cursor_col.saturating_sub(param_or(params, 0, 1)),
            'G' => self.cursor_col = one_based_param(params, 0).min(self.cols - 1),
            'H' | 'f' => {
                self.cursor_row = one_based_param(params, 0).min(self.rows - 1);
                self.cursor_col = one_based_param(params, 1).min(self.cols - 1);
            }
            'J' => self.erase_display(param_or(params, 0, 0)),
            'K' => self.erase_line(param_or(params, 0, 0)),
            'm' => self.current_style.apply_sgr(params),
            'r' => self.set_scroll_region(params),
            'S' => self.scroll_up(param_or(params, 0, 1)),
            'T' => self.scroll_down(param_or(params, 0, 1)),
            'd' => self.cursor_row = one_based_param(params, 0).min(self.rows - 1),
            'h' if private && has_param(params, 1049) => self.enter_alternate_screen(),
            'l' if private && has_param(params, 1049) => self.leave_alternate_screen(),
            _ => {}
        }
    }

    fn print_char(&mut self, character: char) {
        let width = terminal_character_width(character);
        if width == 0 {
            return;
        }
        if self.cursor_col >= self.cols || (width > 1 && self.cursor_col + width > self.cols) {
            self.cursor_col = 0;
            self.line_feed();
        }
        let cursor_col = self.cursor_col;
        let current_style = self.current_style.clone();
        let line = self.visible_line_mut(self.cursor_row);
        line.touched = true;
        line.cells[cursor_col] = TerminalCell {
            character,
            style: current_style,
            wide_continuation: false,
        };
        for offset in 1..width {
            if cursor_col + offset < line.cells.len() {
                line.cells[cursor_col + offset] =
                    TerminalCell::wide_continuation(&line.cells[cursor_col].style);
            }
        }
        self.cursor_col += width;
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 == self.scroll_bottom {
            self.scroll_up_region(1);
        } else if self.cursor_row + 1 >= self.rows {
            self.scroll_up_region(1);
        } else {
            self.cursor_row += 1;
            self.visible_line_mut(self.cursor_row).touched = true;
        }
    }

    fn scroll_up(&mut self, count: usize) {
        self.scroll_up_region(count);
    }

    fn scroll_up_region(&mut self, count: usize) {
        let current_style = self.current_style.clone();
        let top = self.scroll_top.min(self.rows - 1);
        let bottom = self.scroll_bottom.min(self.rows).max(top + 1);
        for _ in 0..count.min(bottom - top) {
            if self.normal_screen.is_none() && top == 0 {
                // 滚动区域从首行开始时，真实终端会把滚出的行放入 scrollback。
                // 这里通过在区域末尾插入空白行来保留旧首行，并让固定底部区域保持原位。
                let insert_at = self.visible_start() + bottom;
                self.insert_visible_line(insert_at, &current_style);
            } else {
                let start = self.visible_start() + top;
                let insert_at = self.visible_start() + bottom - 1;
                let _ = self.lines.remove(start);
                self.insert_visible_line(insert_at, &current_style);
            }
        }
        self.cursor_row = self.cursor_row.min(self.rows - 1);
    }

    fn scroll_down(&mut self, count: usize) {
        let current_style = self.current_style.clone();
        let top = self.scroll_top.min(self.rows - 1);
        let bottom = self.scroll_bottom.min(self.rows).max(top + 1);
        for _ in 0..count.min(bottom - top) {
            let start = self.visible_start() + top;
            let end = self.visible_start() + bottom;
            let _ = self.lines.remove(end - 1);
            self.insert_visible_line(start, &current_style);
        }
        self.cursor_row = self.cursor_row.min(self.rows - 1);
    }

    fn insert_visible_line(&mut self, index: usize, style: &CellStyle) {
        let line = TerminalLine {
            index: self.next_line_index,
            cells: vec![TerminalCell::styled_blank(style); self.cols],
            touched: true,
        };
        self.next_line_index = self.next_line_index.saturating_add(1);
        self.lines.insert(index.min(self.lines.len()), line);
        self.trim_cached_lines();
    }

    fn clear_screen(&mut self) {
        let visible_start = self.visible_start();
        let current_style = self.current_style.clone();
        if self.normal_screen.is_none() {
            // 普通屏的清屏会开启一个新的可见页；旧 viewport 保留为 scrollback，
            // 这样 Codex/CLI 这类普通屏全屏重绘不会把刚输出的历史内容从回放缓存里抹掉。
            for _ in 0..self.rows {
                self.push_blank_line_with_style(true, &current_style);
            }
            return;
        }
        for line in self.lines.iter_mut().skip(visible_start) {
            line.clear_with_style(&current_style);
        }
    }

    fn clear_cached_lines(&mut self) {
        let current_style = self.current_style.clone();
        self.lines.clear();
        for _ in 0..self.rows {
            self.push_blank_line_with_style(false, &current_style);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn enter_alternate_screen(&mut self) {
        if self.normal_screen.is_none() {
            // alternate screen 没有普通 scrollback；保存普通屏幕后重建干净的可见屏幕。
            self.normal_screen = Some(SavedScreen {
                lines: self.lines.clone(),
                next_line_index: self.next_line_index,
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
                saved_cursor: self.saved_cursor,
                current_style: self.current_style.clone(),
                scroll_top: self.scroll_top,
                scroll_bottom: self.scroll_bottom,
            });
        }
        self.reset_screen_buffer();
    }

    fn leave_alternate_screen(&mut self) {
        if let Some(saved) = self.normal_screen.take() {
            self.lines = saved.lines;
            self.next_line_index = saved.next_line_index;
            self.cursor_row = saved.cursor_row.min(self.rows - 1);
            self.cursor_col = saved.cursor_col.min(self.cols - 1);
            self.saved_cursor = saved.saved_cursor;
            self.current_style = saved.current_style;
            self.scroll_top = saved.scroll_top.min(self.rows - 1);
            self.scroll_bottom = saved.scroll_bottom.min(self.rows).max(self.scroll_top + 1);
        } else {
            self.reset_screen_buffer();
        }
    }

    fn reset_screen_buffer(&mut self) {
        self.lines.clear();
        for index in 0..self.rows {
            self.lines
                .push_back(TerminalLine::blank(index as u64, self.cols, false));
        }
        self.next_line_index = self.rows as u64;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_cursor = None;
        self.current_style = CellStyle::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
    }

    fn set_scroll_region(&mut self, params: &[Option<u16>]) {
        let top = one_based_param(params, 0).min(self.rows - 1);
        let bottom = param_or(params, 1, self.rows).min(self.rows).max(top + 1);
        self.scroll_top = top;
        self.scroll_bottom = bottom;
        // DECSTBM 会把光标移动到 home；后续输出通常会再显式定位。
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn erase_display(&mut self, mode: usize) {
        let current_style = self.current_style.clone();
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.cursor_row + 1..self.rows {
                    self.visible_line_mut(row).clear_with_style(&current_style);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.visible_line_mut(row).clear_with_style(&current_style);
                }
                self.erase_line(1);
            }
            2 => self.clear_screen(),
            3 => self.clear_cached_lines(),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let current_style = self.current_style.clone();
        match mode {
            0 => {
                let cursor_col = self.cursor_col;
                let cols = self.cols;
                let line = self.visible_line_mut(self.cursor_row);
                for col in cursor_col..cols {
                    line.cells[col] = TerminalCell::styled_blank(&current_style);
                }
                line.touched = true;
                self.cursor_col = cursor_col.min(cols - 1);
            }
            1 => {
                let cursor_col = self.cursor_col.min(self.cols - 1);
                let line = self.visible_line_mut(self.cursor_row);
                for col in 0..=cursor_col {
                    line.cells[col] = TerminalCell::styled_blank(&current_style);
                }
                line.touched = true;
                self.cursor_col = cursor_col;
            }
            2 => self
                .visible_line_mut(self.cursor_row)
                .clear_with_style(&current_style),
            _ => {}
        }
    }
}

impl SavedScreen {
    fn resize(&mut self, rows: usize, cols: usize, max_cached_lines: usize) {
        for line in &mut self.lines {
            line.resize(cols);
        }
        while self.lines.len() < rows {
            let index = self.next_line_index;
            self.next_line_index = self.next_line_index.saturating_add(1);
            self.lines
                .push_back(TerminalLine::blank(index, cols, false));
        }
        while self.lines.len() > max_cached_lines {
            self.lines.pop_front();
        }
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
    }
}

fn param_or(params: &[Option<u16>], index: usize, default: usize) -> usize {
    params
        .get(index)
        .and_then(|value| *value)
        .map(usize::from)
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn one_based_param(params: &[Option<u16>], index: usize) -> usize {
    param_or(params, index, 1).saturating_sub(1)
}

fn has_param(params: &[Option<u16>], expected: u16) -> bool {
    params.iter().any(|value| *value == Some(expected))
}

fn parse_extended_color(values: &[u16]) -> Option<(SgrColor, usize)> {
    match values {
        [5, index, ..] => Some((SgrColor::Indexed(u8::try_from(*index).ok()?), 2)),
        [2, red, green, blue, ..] => Some((
            SgrColor::Rgb(
                u8::try_from(*red).ok()?,
                u8::try_from(*green).ok()?,
                u8::try_from(*blue).ok()?,
            ),
            4,
        )),
        _ => None,
    }
}

fn row_ansi_bytes(row: &TerminalLine) -> Vec<u8> {
    let display_width = row.display_width();
    if display_width == 0 {
        return Vec::new();
    }

    let mut output = Vec::new();
    let mut active_style = CellStyle::default();
    for cell in row.cells.iter().take(display_width) {
        if cell.wide_continuation {
            continue;
        }
        if cell.style != active_style {
            output.extend_from_slice(&cell.style.sgr_bytes(!active_style.is_default()));
            active_style = cell.style.clone();
        }
        let mut buffer = [0_u8; 4];
        output.extend_from_slice(cell.character.encode_utf8(&mut buffer).as_bytes());
    }
    if !active_style.is_default() {
        output.extend_from_slice(&CellStyle::default().sgr_bytes(false));
    }
    output
}

fn terminal_character_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_return_updates_current_line_snapshot() {
        let mut screen = TerminalScreen::new(3, 20);

        screen.apply(b"state: pending\rstate: done   ");

        assert_eq!(screen.visible_lines()[0], "state: done");
        assert!(!screen.visible_lines()[0].contains("pending"));
    }

    #[test]
    fn csi_cursor_position_can_update_one_visible_line() {
        let mut screen = TerminalScreen::new(3, 20);

        screen.apply(b"alpha\nbeta\ngamma\x1b[2;1Hbravo\x1b[K");

        assert_eq!(screen.visible_lines(), vec!["alpha", "bravo", "gamma"]);
    }

    #[test]
    fn visible_screen_keeps_only_current_rows() {
        let mut screen = TerminalScreen::new(2, 20);

        screen.apply(b"line-1\nline-2\nline-3");

        assert_eq!(screen.visible_lines(), vec!["line-2", "line-3"]);
    }

    #[test]
    fn wide_cjk_characters_wrap_by_terminal_cell_width() {
        let mut screen = TerminalScreen::new(3, 10);

        // 中文等宽字符在真实终端里占 2 列；如果按 char 数算 1 列，长中文行会少滚动几行。
        screen.apply("一二三四五六\nnext".as_bytes());

        assert_eq!(screen.visible_lines(), vec!["一二三四五", "六", "next"]);
    }

    #[test]
    fn terminal_cache_keeps_last_logical_lines_in_ring() {
        let mut screen = TerminalScreen::with_max_cached_lines(2, 20, 5);

        screen.apply(b"one\ntwo\nthree\nfour\nfive\nsix");

        assert_eq!(
            screen.cached_plain_lines(),
            vec!["two", "three", "four", "five", "six"]
        );
        assert_eq!(screen.cached_line_range(), Some((1, 6)));
    }

    #[test]
    fn default_terminal_cache_keeps_one_thousand_logical_lines() {
        let mut screen = TerminalScreen::new(2, 20);

        for index in 0..1005 {
            if index > 0 {
                screen.apply(b"\n");
            }
            screen.apply(format!("line-{index}").as_bytes());
        }

        let lines = screen.cached_plain_lines();
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines.first().map(String::as_str), Some("line-5"));
        assert_eq!(lines.last().map(String::as_str), Some("line-1004"));
        assert_eq!(screen.cached_line_range(), Some((5, 1005)));
    }

    #[test]
    fn style_rewrites_do_not_consume_cached_line_slots() {
        let mut screen = TerminalScreen::with_max_cached_lines(3, 20, 5);

        screen.apply(b"\x1b[31mred\x1b[0m");
        let range_after_text = screen.cached_line_range();
        for _ in 0..20 {
            screen.apply(b"\r\x1b[32mgreen\x1b[0m\x1b[K");
        }

        assert_eq!(screen.cached_line_range(), range_after_text);
        assert_eq!(screen.cached_plain_lines(), vec!["green"]);
        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(snapshot.contains("\x1b[32mgreen\x1b[0m"));
        assert!(!snapshot.contains("red"));
    }

    #[test]
    fn snapshot_preserves_sgr_styles_for_cached_lines() {
        let mut screen = TerminalScreen::with_max_cached_lines(2, 20, 5);

        screen.apply(b"\x1b[31mred\x1b[0m\nnormal");

        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(snapshot.contains("\x1b[31mred\x1b[0m"));
        assert!(snapshot.contains("normal"));
    }

    #[test]
    fn snapshot_preserves_visible_row_coordinates_for_tui_output() {
        let mut screen = TerminalScreen::new(6, 20);

        // 全屏 TUI 会用绝对光标位置刷新局部区域；attach 回放不能把第 4 行内容压到第 1 行。
        screen.apply(b"\x1b[4;1Hstatus line");

        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(
            snapshot.contains("\r\n\r\n\r\nstatus line"),
            "snapshot should preserve absolute row coordinates: {snapshot:?}"
        );
    }

    #[test]
    fn scroll_region_preserves_fixed_bottom_prompt_and_keeps_history() {
        let mut screen = TerminalScreen::with_max_cached_lines(6, 20, 12);

        screen.apply(b"top-1\ntop-2\ntop-3\ntop-4\nprompt-a\nprompt-b\x1b[1;4r\x1b[4;1H\nnext-top");

        let lines = screen.cached_plain_lines();
        assert!(
            lines.iter().any(|line| line == "top-1"),
            "scrollback should keep the line scrolled out of the top region: {lines:?}"
        );
        assert_eq!(
            screen.visible_lines(),
            vec![
                "top-2", "top-3", "top-4", "next-top", "prompt-a", "prompt-b"
            ]
        );
    }

    #[test]
    fn alternate_screen_snapshot_drops_previous_scrollback() {
        let mut screen = TerminalScreen::new(6, 20);

        screen.apply(b"shell scrollback\nbefore tui\n\x1b[?1049h\x1b[3;1HTUI");

        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(
            snapshot.contains("\r\n\r\nTUI"),
            "alternate screen snapshot should preserve TUI row coordinates: {snapshot:?}"
        );
        assert!(
            !snapshot.contains("shell scrollback") && !snapshot.contains("before tui"),
            "alternate screen snapshot should not replay previous shell scrollback: {snapshot:?}"
        );
    }

    #[test]
    fn erase_display_preserves_current_background_style_in_snapshot() {
        let mut screen = TerminalScreen::new(3, 8);

        // 很多 TUI 会先设置背景色再清屏，空白区域也应保持这个背景色。
        screen.apply(b"\x1b[48;5;22m\x1b[2J\x1b[2;3HOK");

        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(
            snapshot.contains("\x1b[48;5;22m        \x1b[0m"),
            "snapshot should replay styled blank cells: {snapshot:?}"
        );
        assert!(snapshot.contains("\x1b[48;5;22m  OK"));
    }

    #[test]
    fn erase_line_preserves_current_background_style_in_snapshot() {
        let mut screen = TerminalScreen::new(2, 8);

        // 清到行尾时，未写入文字的尾部空白也属于当前 SGR 背景。
        screen.apply(b"\x1b[48;5;28mAB\x1b[K");

        let snapshot = String::from_utf8(screen.snapshot_bytes()).unwrap();
        assert!(
            snapshot.contains("\x1b[48;5;28mAB      \x1b[0m"),
            "snapshot should keep styled erased line tail: {snapshot:?}"
        );
    }
}
