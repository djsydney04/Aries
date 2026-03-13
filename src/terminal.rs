use crate::model::detect_help_signal;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub const DEFAULT_TERMINAL_ROWS: u16 = 24;
pub const DEFAULT_TERMINAL_COLS: u16 = 80;
const TERMINAL_SCROLLBACK: usize = 10_000;
const ATTENTION_PATTERNS: [(&str, &str); 11] = [
    ("approve this command", "command approval requested"),
    ("approval required", "command approval requested"),
    ("requires approval", "command approval requested"),
    ("allow this action", "command approval requested"),
    ("waiting for your input", "terminal is waiting for input"),
    ("waiting on user input", "terminal is waiting for input"),
    ("press enter to continue", "terminal is waiting for input"),
    ("press any key to continue", "terminal is waiting for input"),
    ("continue? [y/n]", "terminal is waiting for confirmation"),
    ("continue? (y/n)", "terminal is waiting for confirmation"),
    ("confirm? [y/n]", "terminal is waiting for confirmation"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneActivity {
    pub attention: Option<String>,
    pub attention_changed: bool,
    pub rang_bell: bool,
}

pub struct PaneTerminal {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    scrollback: usize,
    attention: Option<String>,
    last_bell_count: usize,
    render_cache: Vec<Line<'static>>,
    render_dirty: bool,
}

impl PaneTerminal {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), TERMINAL_SCROLLBACK),
            rows: rows.max(1),
            cols: cols.max(1),
            scrollback: 0,
            attention: None,
            last_bell_count: 0,
            render_cache: Vec::new(),
            render_dirty: true,
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if self.rows == rows && self.cols == cols {
            return false;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        self.parser.set_scrollback(self.scrollback);
        self.render_dirty = true;
        true
    }

    pub fn process_output(&mut self, data: &[u8], help_token: &str) -> PaneActivity {
        self.parser.process(data);
        self.render_dirty = true;

        let bell_count =
            self.parser.screen().audible_bell_count() + self.parser.screen().visual_bell_count();
        let rang_bell = bell_count > self.last_bell_count;
        self.last_bell_count = bell_count;

        let should_scan_screen =
            self.attention.is_some() || output_chunk_has_attention_hint(data, help_token);
        let mut next_attention = if should_scan_screen {
            terminal_attention_reason(&self.contents(), help_token)
        } else {
            None
        };
        if next_attention.is_none() && rang_bell {
            next_attention = Some("terminal requested attention".to_string());
        }

        let attention_changed = next_attention != self.attention;
        self.attention = next_attention.clone();

        PaneActivity {
            attention: next_attention,
            attention_changed,
            rang_bell,
        }
    }

    pub fn clear_attention(&mut self) -> bool {
        if self.attention.is_none() {
            return false;
        }
        self.attention = None;
        true
    }

    pub fn attention(&self) -> Option<&str> {
        self.attention.as_deref()
    }

    pub fn reset_scrollback(&mut self) {
        if self.scrollback == 0 {
            return;
        }
        self.scrollback = 0;
        self.parser.set_scrollback(0);
        self.render_dirty = true;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scrollback = self.scrollback.saturating_add(lines);
        self.parser.set_scrollback(self.scrollback);
        self.render_dirty = true;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scrollback = self.scrollback.saturating_sub(lines);
        self.parser.set_scrollback(self.scrollback);
        self.render_dirty = true;
    }

    pub fn render_lines(&mut self, base_style: Style) -> &[Line<'static>] {
        if !self.render_dirty {
            return &self.render_cache;
        }

        let screen = self.parser.screen();
        let mut lines = Vec::with_capacity(self.rows as usize);

        for row in 0..self.rows {
            let mut spans = Vec::new();
            let mut buffer = String::new();
            let mut current_style: Option<Style> = None;

            for col in 0..self.cols {
                let (contents, style) = match screen.cell(row, col) {
                    Some(cell) if cell.is_wide_continuation() => continue,
                    Some(cell) => {
                        let text = if cell.has_contents() {
                            cell.contents()
                        } else {
                            " ".to_string()
                        };
                        (text, style_for_cell(cell, base_style))
                    }
                    None => (" ".to_string(), base_style),
                };

                match current_style {
                    Some(existing) if existing == style => buffer.push_str(&contents),
                    Some(existing) => {
                        spans.push(Span::styled(std::mem::take(&mut buffer), existing));
                        buffer.push_str(&contents);
                        current_style = Some(style);
                    }
                    None => {
                        buffer.push_str(&contents);
                        current_style = Some(style);
                    }
                }
            }

            if let Some(style) = current_style {
                spans.push(Span::styled(buffer, style));
            } else {
                spans.push(Span::styled(String::new(), base_style));
            }

            lines.push(Line::from(spans));
        }

        self.render_cache = lines;
        self.render_dirty = false;
        &self.render_cache
    }

    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    pub fn title(&self) -> &str {
        self.parser.screen().title()
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn cursor_position_for_render(&self) -> (u16, u16) {
        let (row, col) = self.cursor_position();
        self.codex_prompt_cursor_fallback(row, col)
            .unwrap_or((row, col))
    }

    pub fn hide_cursor(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    fn codex_prompt_cursor_fallback(&self, row: u16, _col: u16) -> Option<(u16, u16)> {
        // Some full-screen apps redraw the prompt but leave cursor metadata stale.
        // If the reported cursor is far from the bottom, prefer the visible prompt line.
        if self.rows < 3 || row >= self.rows.saturating_sub(3) {
            return None;
        }

        let screen = self.parser.screen();
        let start_row = self.rows.saturating_sub(6);
        for scan_row in (start_row..self.rows).rev() {
            let mut first_non_space: Option<(u16, char)> = None;
            let mut last_non_space: Option<u16> = None;

            for col in 0..self.cols {
                let Some(cell) = screen.cell(scan_row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }

                let contents = if cell.has_contents() {
                    cell.contents()
                } else {
                    " ".to_string()
                };
                let ch = contents.chars().next().unwrap_or(' ');
                if ch != ' ' {
                    if first_non_space.is_none() {
                        first_non_space = Some((col, ch));
                    }
                    last_non_space = Some(col);
                }
            }

            if let Some((first_col, first_char)) = first_non_space {
                if first_char == '›' || first_char == '>' {
                    let next_col = last_non_space.unwrap_or(first_col).saturating_add(1);
                    let capped_col = next_col.min(self.cols.saturating_sub(1));
                    return Some((scan_row, capped_col));
                }
            }
        }

        None
    }
}

pub fn encode_key_event(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let bytes = match key.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Left => cursor_sequence('D', application_cursor),
        KeyCode::Right => cursor_sequence('C', application_cursor),
        KeyCode::Up => cursor_sequence('A', application_cursor),
        KeyCode::Down => cursor_sequence('B', application_cursor),
        KeyCode::Home => home_end_sequence('H', application_cursor),
        KeyCode::End => home_end_sequence('F', application_cursor),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![control_byte(ch)?]
        }
        KeyCode::Char(ch) => ch.to_string().into_bytes(),
        _ => return None,
    };

    if alt {
        let mut prefixed = vec![0x1b];
        prefixed.extend(bytes);
        Some(prefixed)
    } else {
        Some(bytes)
    }
}

pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.as_bytes().to_vec()
    }
}

pub fn terminal_attention_reason(text: &str, help_token: &str) -> Option<String> {
    if detect_help_signal(text, help_token) {
        return Some("codex needs feedback".to_string());
    }

    let lower = text.to_lowercase();
    ATTENTION_PATTERNS
        .iter()
        .find_map(|(needle, message)| lower.contains(needle).then(|| (*message).to_string()))
}

fn output_chunk_has_attention_hint(data: &[u8], help_token: &str) -> bool {
    if data.is_empty() {
        return false;
    }

    let lower = String::from_utf8_lossy(data).to_lowercase();
    if lower.contains("needs help") || lower.contains("need help") {
        return true;
    }

    if !help_token.trim().is_empty() && lower.contains(&help_token.to_lowercase()) {
        return true;
    }

    ATTENTION_PATTERNS
        .iter()
        .any(|(needle, _)| lower.contains(needle))
}

fn cursor_sequence(direction: char, application_cursor: bool) -> Vec<u8> {
    if application_cursor {
        format!("\x1bO{direction}").into_bytes()
    } else {
        format!("\x1b[{direction}").into_bytes()
    }
}

fn home_end_sequence(direction: char, application_cursor: bool) -> Vec<u8> {
    if application_cursor {
        format!("\x1bO{direction}").into_bytes()
    } else {
        format!("\x1b[{direction}").into_bytes()
    }
}

fn control_byte(ch: char) -> Option<u8> {
    match ch {
        'a'..='z' => Some((ch as u8) - b'a' + 1),
        'A'..='Z' => Some((ch as u8) - b'A' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        '?' => Some(127),
        _ => None,
    }
}

fn style_for_cell(cell: &vt100::Cell, base_style: Style) -> Style {
    let default_fg = base_style.fg.unwrap_or(Color::White);
    let default_bg = base_style.bg.unwrap_or(Color::Black);
    let mut fg = default_fg;
    let mut bg = default_bg;

    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut style = base_style.fg(fg).bg(bg);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_encoding_respects_application_cursor_mode() {
        assert_eq!(
            encode_key_event(KeyEvent::from(KeyCode::Up), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key_event(KeyEvent::from(KeyCode::Up), true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn key_encoding_supports_control_shortcuts() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key_event(key, false), Some(vec![0x03]));
    }

    #[test]
    fn attention_reason_detects_help_and_approval() {
        assert_eq!(
            terminal_attention_reason("[[NEEDS_HELP]]", "[[NEEDS_HELP]]"),
            Some("codex needs feedback".to_string())
        );
        assert_eq!(
            terminal_attention_reason("Approval required to continue", "[[NEEDS_HELP]]"),
            Some("command approval requested".to_string())
        );
    }

    #[test]
    fn pane_terminal_tracks_attention_transitions() {
        let mut pane = PaneTerminal::new(6, 40);
        let first = pane.process_output(b"waiting for your input", "[[NEEDS_HELP]]");
        assert_eq!(
            first,
            PaneActivity {
                attention: Some("terminal is waiting for input".to_string()),
                attention_changed: true,
                rang_bell: false,
            }
        );

        let second = pane.process_output(b"\x1b[2J\x1b[Hall good", "[[NEEDS_HELP]]");
        assert_eq!(second.attention_changed, true);
        assert_eq!(second.attention, None);
    }

    #[test]
    fn paste_encoding_uses_bracketed_mode_when_requested() {
        assert_eq!(
            encode_paste("hello", true),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
        assert_eq!(encode_paste("hello", false), b"hello".to_vec());
    }
}
