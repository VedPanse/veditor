use std::{
	fs,
	io,
	path::{Path, PathBuf},
	time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	prelude::*,
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, BorderType, Gauge, List, ListItem, Paragraph, Sparkline, Wrap},
	DefaultTerminal,
};
use syntect::{
	easy::HighlightLines,
	highlighting::{FontStyle, Style as SyntectStyle, Theme as SyntectTheme, ThemeSet},
	parsing::SyntaxSet,
};

const ACCENT_COLOR: &str = "#FFA500";
const STARTUP_FILE: &str = "src/main.rs";
const TICK_RATE: Duration = Duration::from_millis(100);

fn main() -> std::io::Result<()> {
	let mut app = App::new(PathBuf::from(STARTUP_FILE));
	ratatui::run(|terminal| run_app(terminal, &mut app))
}

fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
	loop {
		terminal.draw(|frame| render(frame, app))?;

		if event::poll(TICK_RATE)? {
			match app.handle_event(event::read()?)? {
				AppAction::Continue => {}
				AppAction::Quit => break Ok(()),
			}
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
	Normal,
	Insert,
	Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
	Editor,
	Terminal,
	Performance,
	ProjectTree,
	Codex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppAction {
	Continue,
	Quit,
}

#[derive(Clone, Copy)]
struct UiTheme {
	accent: Color,
	bg: Color,
	panel: Color,
	panel_alt: Color,
	text: Color,
	muted: Color,
	border: Color,
}

#[derive(Default)]
struct CommandState {
	input: String,
}

#[derive(Default, Clone, Copy)]
struct EditorViewport {
	text_height: usize,
	text_width: usize,
	line_number_width: usize,
	text_area: Rect,
	status_area: Rect,
}

#[derive(Clone)]
struct StyledSegment {
	text: String,
	style: Style,
}

struct Highlighter {
	syntax_set: SyntaxSet,
	theme: SyntectTheme,
}

struct EditorBuffer {
	file_path: PathBuf,
	lines: Vec<String>,
	cursor_row: usize,
	cursor_col: usize,
	scroll_row: usize,
	scroll_col: usize,
	modified: bool,
	newline_at_eof: bool,
	highlighted_lines: Vec<Vec<StyledSegment>>,
}

struct App {
	mode: Mode,
	focus: Focus,
	pending_normal_command: Option<char>,
	command_state: CommandState,
	status_message: String,
	unnamed_register: String,
	project_entries: Vec<String>,
	editor: EditorBuffer,
	highlighter: Highlighter,
	ui: UiTheme,
	editor_viewport: EditorViewport,
}

impl App {
	fn new(file_path: PathBuf) -> Self {
		let ui = ui_theme();
		let highlighter = Highlighter::new();
		let project_entries = collect_project_entries(Path::new("."));

		let (editor, status_message) = match EditorBuffer::from_path(file_path) {
			Ok(editor) => (editor, format!("opened {}", STARTUP_FILE)),
			Err(error) => (
				EditorBuffer::empty(PathBuf::from(STARTUP_FILE)),
				format!("failed to open {}: {}", STARTUP_FILE, error),
			),
		};

		let mut app = Self {
			mode: Mode::Normal,
			focus: Focus::Editor,
			pending_normal_command: None,
			command_state: CommandState::default(),
			status_message,
			unnamed_register: String::new(),
			project_entries,
			editor,
			highlighter,
			ui,
			editor_viewport: EditorViewport::default(),
		};

		app.rehighlight_buffer();
		app
	}

	fn handle_event(&mut self, event: Event) -> io::Result<AppAction> {
		match event {
			Event::Key(key) if is_key_press(key.kind) => self.handle_key_event(key),
			Event::Paste(text) => {
				self.handle_paste(text);
				Ok(AppAction::Continue)
			}
			Event::Mouse(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {
				Ok(AppAction::Continue)
			}
			_ => Ok(AppAction::Continue),
		}
	}

	fn handle_key_event(&mut self, key: KeyEvent) -> io::Result<AppAction> {
		if self.handle_global_key_event(key) {
			return Ok(AppAction::Continue);
		}

		match self.mode {
			Mode::Normal => self.handle_normal_mode(key),
			Mode::Insert => self.handle_insert_mode(key),
			Mode::Command => self.handle_command_mode(key),
		}
	}

	fn handle_global_key_event(&mut self, key: KeyEvent) -> bool {
		if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
			self.focus = self.focus.next();
			self.status_message = format!("focus {}", self.focus.label());
			return true;
		}

		false
	}

	fn handle_normal_mode(&mut self, key: KeyEvent) -> io::Result<AppAction> {
		if self.focus != Focus::Editor {
			match key.code {
				KeyCode::Esc => return Ok(AppAction::Quit),
				KeyCode::Char(':') => {
					self.mode = Mode::Command;
					self.command_state.input.clear();
					self.status_message = "command".to_string();
				}
				_ => {}
			}
			return Ok(AppAction::Continue);
		}

		if let Some(pending) = self.pending_normal_command {
			self.pending_normal_command = None;
			match (pending, key.code) {
				('g', KeyCode::Char('g')) => {
					self.editor.move_to_first_line();
					self.status_message = "gg".to_string();
				}
				('d', KeyCode::Char('d')) => {
					if let Some(deleted) = self.editor.delete_current_line() {
						self.unnamed_register = deleted;
						self.rehighlight_buffer();
						self.status_message = "dd".to_string();
					}
				}
				('d', KeyCode::Char('w')) => {
					if let Some(deleted) = self.editor.delete_word_forward() {
						self.unnamed_register = deleted;
						self.rehighlight_buffer();
						self.status_message = "dw".to_string();
					}
				}
				_ => {
					self.status_message = format!("unknown normal command {}{}", pending, key_label(key.code));
				}
			}
			self.editor.ensure_cursor_visible(self.editor_viewport);
			return Ok(AppAction::Continue);
		}

		match key.code {
			KeyCode::Esc => return Ok(AppAction::Quit),
			KeyCode::Char('i') => {
				self.mode = Mode::Insert;
				self.status_message = "insert mode".to_string();
			}
			KeyCode::Char('a') => {
				self.editor.move_right_in_line();
				self.mode = Mode::Insert;
				self.status_message = "append mode".to_string();
			}
			KeyCode::Char('A') => {
				self.editor.move_line_end();
				self.mode = Mode::Insert;
				self.status_message = "append line".to_string();
			}
			KeyCode::Char('I') => {
				self.editor.move_first_non_blank();
				self.mode = Mode::Insert;
				self.status_message = "insert line".to_string();
			}
			KeyCode::Char('o') => {
				self.editor.open_below();
				self.rehighlight_buffer();
				self.mode = Mode::Insert;
				self.status_message = "open below".to_string();
			}
			KeyCode::Char('O') => {
				self.editor.open_above();
				self.rehighlight_buffer();
				self.mode = Mode::Insert;
				self.status_message = "open above".to_string();
			}
			KeyCode::Char(':') => {
				self.mode = Mode::Command;
				self.command_state.input.clear();
				self.status_message = "command".to_string();
			}
			KeyCode::Up | KeyCode::Char('k') => self.editor.move_up(),
			KeyCode::Down | KeyCode::Char('j') => self.editor.move_down(),
			KeyCode::Left | KeyCode::Char('h') => self.editor.move_left(),
			KeyCode::Right | KeyCode::Char('l') => self.editor.move_right(),
			KeyCode::Char('w') => self.editor.move_word_forward(),
			KeyCode::Char('b') => self.editor.move_word_backward(),
			KeyCode::Char('e') => self.editor.move_word_end(),
			KeyCode::Home | KeyCode::Char('0') => self.editor.move_line_start(),
			KeyCode::End | KeyCode::Char('$') => self.editor.move_line_end(),
			KeyCode::Char('G') => self.editor.move_to_last_line(),
			KeyCode::Char('g') => self.pending_normal_command = Some('g'),
			KeyCode::Char('d') => self.pending_normal_command = Some('d'),
			KeyCode::Char('x') => {
				if let Some(deleted) = self.editor.delete_char_under_cursor() {
					self.unnamed_register = deleted;
					self.rehighlight_buffer();
					self.status_message = "x".to_string();
				}
			}
			KeyCode::Char('D') => {
				if let Some(deleted) = self.editor.delete_to_line_end() {
					self.unnamed_register = deleted;
					self.rehighlight_buffer();
					self.status_message = "D".to_string();
				}
			}
			KeyCode::Char('p') => {
				if self.editor.put_after(&self.unnamed_register) {
					self.rehighlight_buffer();
					self.status_message = "p".to_string();
				}
			}
			KeyCode::Char(ch) => {
				if ch != 'q' {
					self.status_message = format!("normal mode ignored `{}`", ch);
				}
			}
			_ => {}
		}

		self.editor.ensure_cursor_visible(self.editor_viewport);
		Ok(AppAction::Continue)
	}

	fn handle_insert_mode(&mut self, key: KeyEvent) -> io::Result<AppAction> {
		if self.focus != Focus::Editor {
			match key.code {
				KeyCode::Esc => {
					self.mode = Mode::Normal;
					self.status_message = "normal mode".to_string();
				}
				_ => {}
			}
			return Ok(AppAction::Continue);
		}

		let mut changed = false;

		match key.code {
			KeyCode::Esc => {
				self.mode = Mode::Normal;
				self.status_message = "normal mode".to_string();
			}
			KeyCode::Up => self.editor.move_up(),
			KeyCode::Down => self.editor.move_down(),
			KeyCode::Left => self.editor.move_left(),
			KeyCode::Right => self.editor.move_right(),
			KeyCode::Home => self.editor.move_line_start(),
			KeyCode::End => self.editor.move_line_end(),
			KeyCode::Backspace => changed = self.editor.backspace(),
			KeyCode::Delete => changed = self.editor.delete(),
			KeyCode::Enter => changed = self.editor.insert_newline(),
			KeyCode::Tab => changed = self.editor.insert_tab(),
			KeyCode::Char(ch) => changed = self.editor.insert_char(ch),
			_ => {}
		}

		if changed {
			self.rehighlight_buffer();
			self.status_message = format!("editing {}", self.editor.file_name());
		}

		self.editor.ensure_cursor_visible(self.editor_viewport);
		Ok(AppAction::Continue)
	}

	fn handle_command_mode(&mut self, key: KeyEvent) -> io::Result<AppAction> {
		match key.code {
			KeyCode::Esc => {
				self.mode = Mode::Normal;
				self.command_state.input.clear();
				self.status_message = "normal mode".to_string();
			}
			KeyCode::Backspace => {
				self.command_state.input.pop();
			}
			KeyCode::Enter => {
				let command = self.command_state.input.trim().to_string();
				self.command_state.input.clear();
				self.mode = Mode::Normal;

				match command.as_str() {
					"w" => match self.editor.save() {
						Ok(bytes) => {
							self.status_message =
								format!("saved {} ({} bytes)", self.editor.file_name(), bytes);
						}
						Err(error) => {
							self.status_message = format!("save failed: {}", error);
						}
					},
					"" => {
						self.status_message = "normal mode".to_string();
					}
					_ => {
						self.status_message = format!("unknown command :{}", command);
					}
				}

				self.rehighlight_buffer();
			}
			KeyCode::Char(ch) => {
				self.command_state.input.push(ch);
			}
			_ => {}
		}

		Ok(AppAction::Continue)
	}

	fn handle_paste(&mut self, text: String) {
		match self.mode {
			Mode::Insert if self.focus == Focus::Editor => {
				if self.editor.insert_text(&text) {
					self.rehighlight_buffer();
					self.editor.ensure_cursor_visible(self.editor_viewport);
					self.status_message = format!("pasted into {}", self.editor.file_name());
				}
			}
			Mode::Command => {
				self.command_state.input.push_str(&text.replace('\n', ""));
			}
			_ => {}
		}
	}

	fn rehighlight_buffer(&mut self) {
		self.editor.highlighted_lines =
			build_highlight_cache(&self.editor, &self.highlighter, self.ui);
	}
}

impl Focus {
	fn label(self) -> &'static str {
		match self {
			Focus::Editor => "editor",
			Focus::Terminal => "terminal",
			Focus::Performance => "performance",
			Focus::ProjectTree => "project tree",
			Focus::Codex => "codex",
		}
	}

	fn next(self) -> Self {
		match self {
			Focus::Editor => Focus::Terminal,
			Focus::Terminal => Focus::Performance,
			Focus::Performance => Focus::ProjectTree,
			Focus::ProjectTree => Focus::Codex,
			Focus::Codex => Focus::Editor,
		}
	}
}

impl Highlighter {
	fn new() -> Self {
		let syntax_set = SyntaxSet::load_defaults_nonewlines();
		let theme_set = ThemeSet::load_defaults();
		let theme = theme_set
			.themes
			.get("Solarized (dark)")
			.cloned()
			.or_else(|| theme_set.themes.values().next().cloned())
			.unwrap_or_default();

		Self { syntax_set, theme }
	}
}

impl EditorBuffer {
	fn from_path(file_path: PathBuf) -> io::Result<Self> {
		let content = fs::read_to_string(&file_path)?;
		let newline_at_eof = content.ends_with('\n');
		let normalized = content.replace("\r\n", "\n");
		let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();

		if newline_at_eof && !lines.is_empty() {
			lines.pop();
		}

		if lines.is_empty() {
			lines.push(String::new());
		}

		Ok(Self {
			file_path,
			lines,
			cursor_row: 0,
			cursor_col: 0,
			scroll_row: 0,
			scroll_col: 0,
			modified: false,
			newline_at_eof,
			highlighted_lines: Vec::new(),
		})
	}

	fn empty(file_path: PathBuf) -> Self {
		Self {
			file_path,
			lines: vec![String::new()],
			cursor_row: 0,
			cursor_col: 0,
			scroll_row: 0,
			scroll_col: 0,
			modified: false,
			newline_at_eof: true,
			highlighted_lines: Vec::new(),
		}
	}

	fn file_name(&self) -> String {
		self.file_path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("untitled")
			.to_string()
	}

	fn move_left(&mut self) {
		if self.cursor_col > 0 {
			self.cursor_col -= 1;
			return;
		}

		if self.cursor_row > 0 {
			self.cursor_row -= 1;
			self.cursor_col = char_len(&self.lines[self.cursor_row]);
		}
	}

	fn move_right(&mut self) {
		let line_len = char_len(&self.lines[self.cursor_row]);
		if self.cursor_col < line_len {
			self.cursor_col += 1;
			return;
		}

		if self.cursor_row + 1 < self.lines.len() {
			self.cursor_row += 1;
			self.cursor_col = 0;
		}
	}

	fn move_right_in_line(&mut self) {
		let line_len = char_len(&self.lines[self.cursor_row]);
		if self.cursor_col < line_len {
			self.cursor_col += 1;
		}
	}

	fn move_up(&mut self) {
		if self.cursor_row > 0 {
			self.cursor_row -= 1;
			self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_row]));
		}
	}

	fn move_down(&mut self) {
		if self.cursor_row + 1 < self.lines.len() {
			self.cursor_row += 1;
			self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_row]));
		}
	}

	fn move_line_start(&mut self) {
		self.cursor_col = 0;
	}

	fn move_line_end(&mut self) {
		self.cursor_col = char_len(&self.lines[self.cursor_row]);
	}

	fn move_first_non_blank(&mut self) {
		self.cursor_col = self.lines[self.cursor_row]
			.chars()
			.position(|ch| !ch.is_whitespace())
			.unwrap_or(0);
	}

	fn move_to_first_line(&mut self) {
		self.cursor_row = 0;
		self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_row]));
	}

	fn move_to_last_line(&mut self) {
		self.cursor_row = self.lines.len().saturating_sub(1);
		self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_row]));
	}

	fn move_word_forward(&mut self) {
		let (row, col) = next_word_start(&self.lines, self.cursor_row, self.cursor_col);
		self.cursor_row = row;
		self.cursor_col = col;
	}

	fn move_word_backward(&mut self) {
		let (row, col) = prev_word_start(&self.lines, self.cursor_row, self.cursor_col);
		self.cursor_row = row;
		self.cursor_col = col;
	}

	fn move_word_end(&mut self) {
		let (row, col) = word_end(&self.lines, self.cursor_row, self.cursor_col);
		self.cursor_row = row;
		self.cursor_col = col;
	}

	fn insert_char(&mut self, ch: char) -> bool {
		let byte_index = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col);
		self.lines[self.cursor_row].insert(byte_index, ch);
		self.cursor_col += 1;
		self.modified = true;
		true
	}

	fn insert_tab(&mut self) -> bool {
		self.insert_str("    ")
	}

	fn insert_text(&mut self, text: &str) -> bool {
		self.insert_str(text)
	}

	fn insert_str(&mut self, text: &str) -> bool {
		let mut changed = false;
		for ch in text.chars() {
			match ch {
				'\r' => {}
				'\n' => changed |= self.insert_newline(),
				_ => changed |= self.insert_char(ch),
			}
		}
		changed
	}

	fn insert_newline(&mut self) -> bool {
		let current_line = self.lines[self.cursor_row].clone();
		let split_at = char_to_byte_index(&current_line, self.cursor_col);
		let (left, right) = current_line.split_at(split_at);
		self.lines[self.cursor_row] = left.to_string();
		self.lines.insert(self.cursor_row + 1, right.to_string());
		self.cursor_row += 1;
		self.cursor_col = 0;
		self.modified = true;
		true
	}

	fn backspace(&mut self) -> bool {
		if self.cursor_col > 0 {
			let start = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col - 1);
			let end = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col);
			self.lines[self.cursor_row].replace_range(start..end, "");
			self.cursor_col -= 1;
			self.modified = true;
			return true;
		}

		if self.cursor_row == 0 {
			return false;
		}

		let current = self.lines.remove(self.cursor_row);
		self.cursor_row -= 1;
		self.cursor_col = char_len(&self.lines[self.cursor_row]);
		self.lines[self.cursor_row].push_str(&current);
		self.modified = true;
		true
	}

	fn delete(&mut self) -> bool {
		let line_len = char_len(&self.lines[self.cursor_row]);
		if self.cursor_col < line_len {
			let start = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col);
			let end = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col + 1);
			self.lines[self.cursor_row].replace_range(start..end, "");
			self.modified = true;
			return true;
		}

		if self.cursor_row + 1 >= self.lines.len() {
			return false;
		}

		let next_line = self.lines.remove(self.cursor_row + 1);
		self.lines[self.cursor_row].push_str(&next_line);
		self.modified = true;
		true
	}

	fn delete_char_under_cursor(&mut self) -> Option<String> {
		let line_len = char_len(&self.lines[self.cursor_row]);
		if self.cursor_col >= line_len {
			return None;
		}

		let start = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col);
		let end = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col + 1);
		let deleted = self.lines[self.cursor_row][start..end].to_string();
		self.lines[self.cursor_row].replace_range(start..end, "");
		self.modified = true;
		Some(deleted)
	}

	fn delete_to_line_end(&mut self) -> Option<String> {
		let line_len = char_len(&self.lines[self.cursor_row]);
		if self.cursor_col >= line_len {
			return None;
		}

		let start = char_to_byte_index(&self.lines[self.cursor_row], self.cursor_col);
		let deleted = self.lines[self.cursor_row][start..].to_string();
		self.lines[self.cursor_row].truncate(start);
		self.modified = true;
		Some(deleted)
	}

	fn delete_current_line(&mut self) -> Option<String> {
		if self.lines.is_empty() {
			return None;
		}

		let deleted = self.lines.remove(self.cursor_row);
		if self.lines.is_empty() {
			self.lines.push(String::new());
			self.cursor_row = 0;
		} else if self.cursor_row >= self.lines.len() {
			self.cursor_row = self.lines.len() - 1;
		}
		self.cursor_col = self.cursor_col.min(char_len(&self.lines[self.cursor_row]));
		self.modified = true;
		Some(format!("{}\n", deleted))
	}

	fn delete_word_forward(&mut self) -> Option<String> {
		let (end_row, end_col) = next_word_start(&self.lines, self.cursor_row, self.cursor_col + 1);
		if end_row == self.cursor_row && end_col == self.cursor_col {
			return None;
		}

		let deleted = self.delete_range(self.cursor_row, self.cursor_col, end_row, end_col)?;
		self.modified = true;
		Some(deleted)
	}

	fn put_after(&mut self, text: &str) -> bool {
		if text.is_empty() {
			return false;
		}

		if text.ends_with('\n') {
			let insert_at = self.cursor_row + 1;
			let mut new_lines: Vec<String> = text.trim_end_matches('\n').split('\n').map(str::to_owned).collect();
			if new_lines.is_empty() {
				new_lines.push(String::new());
			}
			for (idx, line) in new_lines.iter().cloned().enumerate() {
				self.lines.insert(insert_at + idx, line);
			}
			self.cursor_row = insert_at;
			self.cursor_col = 0;
			self.modified = true;
			return true;
		}

		self.move_right_in_line();
		let changed = self.insert_str(text);
		if self.cursor_col > 0 {
			self.cursor_col -= 1;
		}
		changed
	}

	fn open_below(&mut self) {
		let insert_at = self.cursor_row + 1;
		self.lines.insert(insert_at, String::new());
		self.cursor_row = insert_at;
		self.cursor_col = 0;
		self.modified = true;
	}

	fn open_above(&mut self) {
		self.lines.insert(self.cursor_row, String::new());
		self.cursor_col = 0;
		self.modified = true;
	}

	fn delete_range(
		&mut self,
		start_row: usize,
		start_col: usize,
		end_row: usize,
		end_col: usize,
	) -> Option<String> {
		if start_row > end_row || (start_row == end_row && start_col >= end_col) {
			return None;
		}

		if start_row == end_row {
			let start = char_to_byte_index(&self.lines[start_row], start_col);
			let end = char_to_byte_index(&self.lines[start_row], end_col);
			let deleted = self.lines[start_row][start..end].to_string();
			self.lines[start_row].replace_range(start..end, "");
			self.cursor_row = start_row;
			self.cursor_col = start_col.min(char_len(&self.lines[start_row]));
			return Some(deleted);
		}

		let mut deleted = String::new();
		let start_byte = char_to_byte_index(&self.lines[start_row], start_col);
		deleted.push_str(&self.lines[start_row][start_byte..]);
		deleted.push('\n');

		for row in (start_row + 1)..end_row {
			deleted.push_str(&self.lines[row]);
			deleted.push('\n');
		}

		let end_byte = char_to_byte_index(&self.lines[end_row], end_col);
		deleted.push_str(&self.lines[end_row][..end_byte]);

		let prefix = self.lines[start_row][..start_byte].to_string();
		let suffix = self.lines[end_row][end_byte..].to_string();
		self.lines.splice(start_row..=end_row, [format!("{prefix}{suffix}")]);
		self.cursor_row = start_row;
		self.cursor_col = start_col.min(char_len(&self.lines[start_row]));
		Some(deleted)
	}

	fn ensure_cursor_visible(&mut self, viewport: EditorViewport) {
		if viewport.text_height == 0 || viewport.text_width == 0 {
			return;
		}

		if self.cursor_row < self.scroll_row {
			self.scroll_row = self.cursor_row;
		} else if self.cursor_row >= self.scroll_row + viewport.text_height {
			self.scroll_row = self.cursor_row + 1 - viewport.text_height;
		}

		if self.cursor_col < self.scroll_col {
			self.scroll_col = self.cursor_col;
		} else if self.cursor_col >= self.scroll_col + viewport.text_width {
			self.scroll_col = self.cursor_col + 1 - viewport.text_width;
		}
	}

	fn save(&mut self) -> io::Result<usize> {
		let mut content = self.lines.join("\n");
		let has_content = !(self.lines.len() == 1 && self.lines[0].is_empty());
		if self.newline_at_eof && has_content {
			content.push('\n');
		}

		fs::write(&self.file_path, &content)?;
		self.modified = false;
		Ok(content.len())
	}
}

fn render(frame: &mut Frame, app: &mut App) {
	let area = frame.area();
	frame.render_widget(Block::default().style(Style::default().bg(app.ui.bg)), area);

	let [header, body] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(3), Constraint::Min(20)])
		.areas(area);

	render_header(frame, header, app);

	let [left, right] = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
		.areas(body);

	let [left_top, left_bottom] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
		.areas(left);

	let [terminal_area, performance_area] = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(63), Constraint::Percentage(37)])
		.areas(left_top);

	let [editor_area, codex_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Percentage(82), Constraint::Percentage(18)])
		.areas(right);

	terminal_ui(frame, terminal_area, app);
	performance_block(frame, performance_area, app);
	project_tree(frame, left_bottom, app);
	code_editor(frame, editor_area, app);
	codex_block(frame, codex_area, app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
	let [brand, center, status] = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([
			Constraint::Length(18),
			Constraint::Min(20),
			Constraint::Length(34),
		])
		.areas(area);

	let brand_text = Paragraph::new(Line::from(vec![
		Span::styled(" C", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
		Span::styled("¥", Style::default().fg(app.ui.accent)),
		Span::styled("B", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
		Span::styled("editor", Style::default().fg(app.ui.accent)),
	]))
	.block(panel("veditor", app.ui, false))
	.alignment(Alignment::Left);

	let center_text = Paragraph::new(Line::from(vec![
		Span::styled(
			format!(" {} ", app.editor.file_path.display()),
			Style::default().fg(app.ui.text),
		),
		Span::styled(mode_label(app.mode), Style::default().fg(app.ui.accent)),
	]))
	.block(panel("workspace", app.ui, false))
	.alignment(Alignment::Center);

	let dirty = if app.editor.modified { "modified" } else { "saved" };
	let status_text = Paragraph::new(Line::from(vec![
		Span::styled(
			format!(" {} ", mode_label(app.mode)),
			Style::default().fg(app.ui.bg).bg(app.ui.accent),
		),
		Span::raw(" "),
		Span::styled(app.focus.label(), Style::default().fg(app.ui.text)),
		Span::raw("  "),
		Span::styled(dirty, Style::default().fg(app.ui.muted)),
	]))
	.block(panel("status", app.ui, false))
	.alignment(Alignment::Right);

	frame.render_widget(brand_text, brand);
	frame.render_widget(center_text, center);
	frame.render_widget(status_text, status);
}

fn terminal_ui(frame: &mut Frame, area: Rect, app: &App) {
	let lines = vec![
		Line::from(vec![
			Span::styled("$ ", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
			Span::styled("cargo run", Style::default().fg(app.ui.text)),
		]),
		Line::styled(
			format!("open {}", app.editor.file_path.display()),
			Style::default().fg(app.ui.text),
		),
		Line::styled(
			format!("{} lines  cursor {},{}", app.editor.lines.len(), app.editor.cursor_row + 1, app.editor.cursor_col + 1),
			Style::default().fg(app.ui.muted),
		),
		Line::default(),
		Line::styled("keyboard only", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
		Line::styled("Ctrl-W change focus", Style::default().fg(app.ui.text)),
		Line::styled("i/a/o/d/x/w/b/gg/G   : command", Style::default().fg(app.ui.text)),
		Line::styled("Esc leaves insert, Esc in Normal quits", Style::default().fg(app.ui.text)),
	];

	let terminal = Paragraph::new(lines)
		.block(panel("terminal", app.ui, app.focus == Focus::Terminal))
		.wrap(Wrap { trim: false });

	frame.render_widget(terminal, area);
}

fn code_editor(frame: &mut Frame, area: Rect, app: &mut App) {
	let block = panel("editor", app.ui, app.focus == Focus::Editor);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	let [tabs_area, text_area, status_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)])
		.areas(inner);

	app.editor_viewport = EditorViewport {
		text_height: text_area.height as usize,
		text_width: text_area.width.saturating_sub(line_number_width(app.editor.lines.len()) as u16) as usize,
		line_number_width: line_number_width(app.editor.lines.len()),
		text_area,
		status_area,
	};
	app.editor.ensure_cursor_visible(app.editor_viewport);

	let tabs = Paragraph::new(Line::from(vec![
		Span::styled(
			format!(" {} ", app.editor.file_name()),
			Style::default()
				.fg(app.ui.bg)
				.bg(app.ui.accent)
				.add_modifier(Modifier::BOLD),
		),
		Span::raw(" "),
		Span::styled("terminal", Style::default().fg(app.ui.muted)),
		Span::raw(" "),
		Span::styled("project", Style::default().fg(app.ui.muted)),
	]))
	.style(Style::default().bg(app.ui.panel));
	frame.render_widget(tabs, tabs_area);

	let visible_lines = render_visible_editor_lines(app);
	let editor = Paragraph::new(visible_lines).style(Style::default().bg(app.ui.panel));
	frame.render_widget(editor, text_area);

	let status = if app.mode == Mode::Command {
		Line::from(vec![
			Span::styled(":", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
			Span::styled(app.command_state.input.clone(), Style::default().fg(app.ui.text)),
		])
	} else {
		let dirty = if app.editor.modified { "[+]" } else { "[ ]" };
		Line::from(vec![
			Span::styled(
				format!(" {} ", mode_label(app.mode)),
				Style::default().fg(app.ui.bg).bg(app.ui.accent),
			),
			Span::raw(" "),
			Span::styled(dirty, Style::default().fg(app.ui.accent)),
			Span::raw(" "),
			Span::styled(app.status_message.clone(), Style::default().fg(app.ui.text)),
		])
	};

	let status_widget = Paragraph::new(status).style(Style::default().bg(app.ui.panel_alt));
	frame.render_widget(status_widget, status_area);

	render_editor_cursor(frame, app);
}

fn project_tree(frame: &mut Frame, area: Rect, app: &App) {
	let items = app
		.project_entries
		.iter()
		.map(|entry| {
			let style = if entry.ends_with(&app.editor.file_name()) {
				Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)
			} else {
				Style::default().fg(app.ui.text)
			};
			ListItem::new(Line::from(Span::styled(entry.clone(), style)))
		})
		.collect::<Vec<_>>();

	let tree = List::new(items).block(panel("project tree", app.ui, app.focus == Focus::ProjectTree));
	frame.render_widget(tree, area);
}

fn performance_block(frame: &mut Frame, area: Rect, app: &App) {
	let [gpu_area, cpu_area, mem_area, graph_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3),
			Constraint::Length(3),
			Constraint::Length(3),
			Constraint::Min(5),
		])
		.areas(area);

	let gauge_style = Style::default().fg(app.ui.accent).bg(app.ui.panel_alt);
	let block_focus = app.focus == Focus::Performance;

	frame.render_widget(
		Gauge::default()
			.block(panel("gpu", app.ui, block_focus))
			.gauge_style(gauge_style)
			.percent(68),
		gpu_area,
	);

	frame.render_widget(
		Gauge::default()
			.block(panel("cpu", app.ui, false))
			.gauge_style(gauge_style)
			.percent(42),
		cpu_area,
	);

	frame.render_widget(
		Gauge::default()
			.block(panel("mem", app.ui, false))
			.gauge_style(gauge_style)
			.percent(57),
		mem_area,
	);

	let spark = Sparkline::default()
		.block(panel("fps / render", app.ui, false))
		.data(&[3, 6, 9, 8, 11, 14, 10, 12, 15, 14, 16, 13, 17, 18, 16, 19])
		.style(Style::default().fg(app.ui.accent))
		.max(20);

	frame.render_widget(spark, graph_area);
}

fn codex_block(frame: &mut Frame, area: Rect, app: &App) {
	let content = vec![
		Line::from(vec![
			Span::styled("mode", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
			Span::raw(": "),
			Span::styled(mode_label(app.mode), Style::default().fg(app.ui.text)),
		]),
		Line::styled("single-file editor v1", Style::default().fg(app.ui.text)),
		Line::styled("syntax: syntect", Style::default().fg(app.ui.muted)),
		Line::styled("quit: Esc from Normal", Style::default().fg(app.ui.muted)),
	];

	let codex = Paragraph::new(content)
		.block(panel("codex", app.ui, app.focus == Focus::Codex))
		.wrap(Wrap { trim: true });

	frame.render_widget(codex, area);
}

fn render_visible_editor_lines(app: &App) -> Vec<Line<'static>> {
	let mut rendered = Vec::with_capacity(app.editor_viewport.text_height);
	let width = app.editor_viewport.text_width;
	let line_number_width = app.editor_viewport.line_number_width;

	for row_offset in 0..app.editor_viewport.text_height {
		let buffer_row = app.editor.scroll_row + row_offset;

		if buffer_row >= app.editor.lines.len() {
			rendered.push(Line::from(vec![Span::styled(
				"~",
				Style::default().fg(app.ui.muted),
			)]));
			continue;
		}

		let current_row = buffer_row == app.editor.cursor_row;
		let number_style = if current_row {
			Style::default()
				.fg(app.ui.bg)
				.bg(app.ui.accent)
				.add_modifier(Modifier::BOLD)
		} else {
			Style::default().fg(app.ui.muted)
		};

		let mut spans = vec![Span::styled(
			format!("{:>width$} ", buffer_row + 1, width = line_number_width.saturating_sub(1)),
			number_style,
		)];

		let segments = app
			.editor
			.highlighted_lines
			.get(buffer_row)
			.map(Vec::as_slice)
			.unwrap_or(&[]);

		let mut text_spans = clip_segments(
			segments,
			app.editor.scroll_col,
			width,
			if current_row {
				Some(Style::default().bg(app.ui.panel_alt))
			} else {
				None
			},
		);

		if text_spans.is_empty() {
			text_spans.push(Span::styled(
				" ".repeat(width.max(1)),
				Style::default().bg(if current_row { app.ui.panel_alt } else { app.ui.panel }),
			));
		}

		spans.append(&mut text_spans);
		rendered.push(Line::from(spans));
	}

	rendered
}

fn render_editor_cursor(frame: &mut Frame, app: &App) {
	if app.focus != Focus::Editor {
		return;
	}

	match app.mode {
		Mode::Command => {
			let cursor_x = app
				.editor_viewport
				.status_area
				.x
				.saturating_add(1 + app.command_state.input.chars().count() as u16);
			if cursor_x < app.editor_viewport.status_area.right() {
				frame.set_cursor_position((cursor_x, app.editor_viewport.status_area.y));
			}
		}
		Mode::Normal | Mode::Insert => {
			if app.editor.cursor_row < app.editor.scroll_row {
				return;
			}

			let row = app.editor.cursor_row - app.editor.scroll_row;
			if row >= app.editor_viewport.text_height {
				return;
			}

			let visible_col = app.editor.cursor_col.saturating_sub(app.editor.scroll_col);
			if visible_col >= app.editor_viewport.text_width {
				return;
			}

			let cursor_x = app
				.editor_viewport
				.text_area
				.x
				.saturating_add(app.editor_viewport.line_number_width as u16)
				.saturating_add(visible_col as u16);
			let cursor_y = app.editor_viewport.text_area.y.saturating_add(row as u16);

			if cursor_x < app.editor_viewport.text_area.right()
				&& cursor_y < app.editor_viewport.text_area.bottom()
			{
				frame.set_cursor_position((cursor_x, cursor_y));
			}
		}
	}
}

fn build_highlight_cache(
	editor: &EditorBuffer,
	highlighter: &Highlighter,
	ui: UiTheme,
) -> Vec<Vec<StyledSegment>> {
	let syntax = editor
		.file_path
		.extension()
		.and_then(|ext| ext.to_str())
		.and_then(|ext| highlighter.syntax_set.find_syntax_by_extension(ext))
		.or_else(|| highlighter.syntax_set.find_syntax_by_extension("txt"));

	let Some(syntax) = syntax else {
		return editor
			.lines
			.iter()
			.map(|line| {
				vec![StyledSegment {
					text: line.clone(),
					style: Style::default().fg(ui.text),
				}]
			})
			.collect();
	};

	let mut h = HighlightLines::new(syntax, &highlighter.theme);
	let mut cache = Vec::with_capacity(editor.lines.len());

	for line in &editor.lines {
		let ranges = h.highlight_line(line, &highlighter.syntax_set);
		let styled_line = match ranges {
			Ok(segments) => segments
				.into_iter()
				.map(|(style, text)| StyledSegment {
					text: text.to_string(),
					style: syntect_to_ratatui(style, ui),
				})
				.collect(),
			Err(_) => vec![StyledSegment {
				text: line.clone(),
				style: Style::default().fg(ui.text),
			}],
		};
		cache.push(styled_line);
	}

	cache
}

fn clip_segments(
	segments: &[StyledSegment],
	scroll_col: usize,
	width: usize,
	line_bg: Option<Style>,
) -> Vec<Span<'static>> {
	if width == 0 {
		return Vec::new();
	}

	let mut visible = Vec::new();
	let mut consumed = 0usize;
	let mut remaining = width;

	for segment in segments {
		let segment_len = char_len(&segment.text);
		let seg_end = consumed + segment_len;
		if seg_end <= scroll_col {
			consumed = seg_end;
			continue;
		}

		let local_start = scroll_col.saturating_sub(consumed);
		let available = segment_len.saturating_sub(local_start);
		let take = available.min(remaining);
		if take == 0 {
			break;
		}

		let text = slice_chars(&segment.text, local_start, local_start + take);
		let mut style = segment.style;
		if let Some(bg) = line_bg {
			style = style.patch(bg);
		}
		visible.push(Span::styled(text, style));

		remaining -= take;
		consumed = seg_end;
		if remaining == 0 {
			break;
		}
	}

	if remaining > 0 {
		let mut fill_style = Style::default();
		if let Some(bg) = line_bg {
			fill_style = fill_style.patch(bg);
		}
		visible.push(Span::styled(" ".repeat(remaining), fill_style));
	}

	visible
}

fn panel<'a>(title: &'a str, ui: UiTheme, focused: bool) -> Block<'a> {
	let border_style = if focused {
		Style::default().fg(ui.accent).add_modifier(Modifier::BOLD)
	} else {
		Style::default().fg(ui.border)
	};

	Block::bordered()
		.border_type(BorderType::Rounded)
		.border_style(border_style)
		.style(Style::default().bg(ui.panel))
		.title(
			Line::from(vec![Span::styled(
				format!(" {} ", title),
				Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
			)])
			.left_aligned(),
		)
}

fn mode_label(mode: Mode) -> &'static str {
	match mode {
		Mode::Normal => "NORMAL",
		Mode::Insert => "INSERT",
		Mode::Command => "COMMAND",
	}
}

fn line_number_width(line_count: usize) -> usize {
	line_count.max(1).to_string().len() + 1
}

fn collect_project_entries(root: &Path) -> Vec<String> {
	let mut entries = Vec::new();
	collect_entries_recursive(root, root, &mut entries, 0);
	if entries.is_empty() {
		entries.push("src/main.rs".to_string());
	}
	entries
}

fn collect_entries_recursive(root: &Path, current: &Path, entries: &mut Vec<String>, depth: usize) {
	if depth > 2 {
		return;
	}

	let Ok(read_dir) = fs::read_dir(current) else {
		return;
	};

	let mut paths = read_dir.filter_map(Result::ok).collect::<Vec<_>>();
	paths.sort_by_key(|entry| entry.path());

	for entry in paths {
		let path = entry.path();
		let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
		if name == ".git" || name == "target" || name.ends_with(".swp") {
			continue;
		}

		let relative = path
			.strip_prefix(root)
			.unwrap_or(&path)
			.display()
			.to_string();

		let indent = "  ".repeat(depth);
		let label = if path.is_dir() {
			format!("{}▾ {}", indent, relative)
		} else {
			format!("{}• {}", indent, relative)
		};
		entries.push(label);

		if path.is_dir() {
			collect_entries_recursive(root, &path, entries, depth + 1);
		}
	}
}

fn syntect_to_ratatui(style: SyntectStyle, ui: UiTheme) -> Style {
	let mut rat_style = Style::default().fg(Color::Rgb(
		style.foreground.r,
		style.foreground.g,
		style.foreground.b,
	));

	if style.font_style.contains(FontStyle::BOLD) {
		rat_style = rat_style.add_modifier(Modifier::BOLD);
	}
	if style.font_style.contains(FontStyle::ITALIC) {
		rat_style = rat_style.add_modifier(Modifier::ITALIC);
	}
	if style.font_style.contains(FontStyle::UNDERLINE) {
		rat_style = rat_style.add_modifier(Modifier::UNDERLINED);
	}

	if style.foreground.r == 0 && style.foreground.g == 0 && style.foreground.b == 0 {
		rat_style = rat_style.fg(ui.accent);
	}

	rat_style
}

fn ui_theme() -> UiTheme {
	let accent = parse_hex_color(ACCENT_COLOR).unwrap_or(Color::Rgb(255, 165, 0));

	UiTheme {
		accent,
		bg: Color::Rgb(12, 5, 2),
		panel: Color::Rgb(29, 10, 4),
		panel_alt: Color::Rgb(52, 20, 8),
		text: Color::Rgb(255, 211, 140),
		muted: Color::Rgb(168, 112, 66),
		border: Color::Rgb(122, 68, 22),
	}
}

fn parse_hex_color(value: &str) -> Option<Color> {
	let value = value.trim_start_matches('#');
	if value.len() != 6 {
		return None;
	}

	let r = u8::from_str_radix(&value[0..2], 16).ok()?;
	let g = u8::from_str_radix(&value[2..4], 16).ok()?;
	let b = u8::from_str_radix(&value[4..6], 16).ok()?;

	Some(Color::Rgb(r, g, b))
}

fn key_label(code: KeyCode) -> String {
	match code {
		KeyCode::Char(ch) => ch.to_string(),
		KeyCode::Esc => "Esc".to_string(),
		KeyCode::Enter => "Enter".to_string(),
		KeyCode::Tab => "Tab".to_string(),
		KeyCode::Backspace => "Backspace".to_string(),
		KeyCode::Delete => "Delete".to_string(),
		KeyCode::Left => "Left".to_string(),
		KeyCode::Right => "Right".to_string(),
		KeyCode::Up => "Up".to_string(),
		KeyCode::Down => "Down".to_string(),
		_ => "?".to_string(),
	}
}

fn char_len(value: &str) -> usize {
	value.chars().count()
}

fn char_to_byte_index(value: &str, char_idx: usize) -> usize {
	value
		.char_indices()
		.nth(char_idx)
		.map(|(index, _)| index)
		.unwrap_or(value.len())
}

fn slice_chars(value: &str, start: usize, end: usize) -> String {
	value.chars().skip(start).take(end.saturating_sub(start)).collect()
}

fn is_key_press(kind: KeyEventKind) -> bool {
	matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn next_word_start(lines: &[String], row: usize, col: usize) -> (usize, usize) {
	let flattened = flatten_lines(lines);
	let start = line_col_to_index(lines, row, col).min(flattened.len());
	let chars: Vec<char> = flattened.chars().collect();

	let mut idx = start;
	if idx < chars.len() && is_word_char(chars[idx]) {
		while idx < chars.len() && is_word_char(chars[idx]) {
			idx += 1;
		}
	}
	while idx < chars.len() && !is_word_char(chars[idx]) {
		idx += 1;
	}

	index_to_line_col(lines, idx.min(chars.len()))
}

fn prev_word_start(lines: &[String], row: usize, col: usize) -> (usize, usize) {
	let flattened = flatten_lines(lines);
	let chars: Vec<char> = flattened.chars().collect();
	let mut idx = line_col_to_index(lines, row, col);
	if idx == 0 {
		return (0, 0);
	}

	idx = idx.saturating_sub(1);
	while idx > 0 && !is_word_char(chars[idx]) {
		idx -= 1;
	}
	while idx > 0 && is_word_char(chars[idx - 1]) {
		idx -= 1;
	}

	index_to_line_col(lines, idx)
}

fn word_end(lines: &[String], row: usize, col: usize) -> (usize, usize) {
	let flattened = flatten_lines(lines);
	let chars: Vec<char> = flattened.chars().collect();
	let mut idx = line_col_to_index(lines, row, col).min(chars.len());

	while idx < chars.len() && !is_word_char(chars[idx]) {
		idx += 1;
	}
	while idx + 1 < chars.len() && is_word_char(chars[idx + 1]) {
		idx += 1;
	}

	index_to_line_col(lines, idx.min(chars.len()))
}

fn flatten_lines(lines: &[String]) -> String {
	lines.join("\n")
}

fn line_col_to_index(lines: &[String], row: usize, col: usize) -> usize {
	let mut index = 0usize;
	for (idx, line) in lines.iter().enumerate() {
		if idx == row {
			return index + col.min(char_len(line));
		}
		index += char_len(line) + 1;
	}
	index
}

fn index_to_line_col(lines: &[String], index: usize) -> (usize, usize) {
	let mut remaining = index;
	for (row, line) in lines.iter().enumerate() {
		let line_len = char_len(line);
		if remaining <= line_len {
			return (row, remaining);
		}
		remaining = remaining.saturating_sub(line_len + 1);
	}

	let last_row = lines.len().saturating_sub(1);
	(last_row, char_len(&lines[last_row]))
}

fn is_word_char(ch: char) -> bool {
	ch.is_alphanumeric() || ch == '_'
}
