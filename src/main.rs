use std::{
	fs,
	io::{self, Read, Write},
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	thread,
	time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	prelude::*,
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, BorderType, Gauge, List, ListItem, Paragraph, Sparkline, Wrap},
	DefaultTerminal,
};
use vt100::{Color as VtColor, Parser};

const ACCENT_COLOR: &str = "#FFA500";
const STARTUP_FILE: &str = "src/main.rs";
const TICK_RATE: Duration = Duration::from_millis(33);
const INITIAL_ROWS: u16 = 40;
const INITIAL_COLS: u16 = 120;

fn main() -> io::Result<()> {
	let mut app = App::new(PathBuf::from(STARTUP_FILE))?;
	ratatui::run(|terminal| run_app(terminal, &mut app))
}

fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
	loop {
		app.tick();
		terminal.draw(|frame| render(frame, app))?;

		if event::poll(TICK_RATE)? {
			match app.handle_event(event::read()?) {
				AppAction::Continue => {}
				AppAction::Quit => break Ok(()),
			}
		}
	}
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

struct App {
	focus: Focus,
	status_message: String,
	project_entries: Vec<String>,
	ui: UiTheme,
	nvim: NvimPane,
}

struct NvimPane {
	file_path: PathBuf,
	parser: Arc<Mutex<Parser>>,
	writer: Box<dyn Write + Send>,
	master: Box<dyn portable_pty::MasterPty + Send>,
	child: Box<dyn portable_pty::Child + Send + Sync>,
	last_size: (u16, u16),
	exit_status: Option<String>,
}

struct NvimSnapshot {
	lines: Vec<Line<'static>>,
	cursor: Option<(u16, u16)>,
}

impl App {
	fn new(file_path: PathBuf) -> io::Result<Self> {
		Ok(Self {
			focus: Focus::Editor,
			status_message: "embedded nvim ready".to_string(),
			project_entries: collect_project_entries(Path::new(".")),
			ui: ui_theme(),
			nvim: NvimPane::new(file_path)?,
		})
	}

	fn tick(&mut self) {
		if let Some(status) = self.nvim.poll_exit() {
			self.status_message = status;
		}
	}

	fn handle_event(&mut self, event: Event) -> AppAction {
		match event {
			Event::Key(key) if is_key_press(key.kind) => self.handle_key(key),
			Event::Paste(text) => {
				if self.focus == Focus::Editor {
					if let Err(error) = self.nvim.send_paste(&text) {
						self.status_message = format!("paste failed: {error}");
					}
				}
				AppAction::Continue
			}
			Event::Mouse(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {
				AppAction::Continue
			}
			_ => AppAction::Continue,
		}
	}

	fn handle_key(&mut self, key: KeyEvent) -> AppAction {
		if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
			self.focus = self.focus.next();
			self.status_message = format!("focus {}", self.focus.label());
			return AppAction::Continue;
		}

		if self.focus == Focus::Editor {
			match self.nvim.send_key(key) {
				Ok(()) => {}
				Err(error) => {
					self.status_message = format!("nvim input failed: {error}");
				}
			}
			return AppAction::Continue;
		}

		if key.code == KeyCode::Esc {
			return AppAction::Quit;
		}

		AppAction::Continue
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

impl NvimPane {
	fn new(file_path: PathBuf) -> io::Result<Self> {
		let pty_system = native_pty_system();
		let pair = pty_system
			.openpty(PtySize {
				rows: INITIAL_ROWS,
				cols: INITIAL_COLS,
				pixel_width: 0,
				pixel_height: 0,
			})
			.map_err(io_error)?;

		let parser = Arc::new(Mutex::new(Parser::new(INITIAL_ROWS, INITIAL_COLS, 0)));
		let reader_parser = Arc::clone(&parser);
		let mut reader = pair.master.try_clone_reader().map_err(io_error)?;

		thread::spawn(move || {
			let mut buffer = [0u8; 8192];
			loop {
				match reader.read(&mut buffer) {
					Ok(0) => break,
					Ok(count) => {
						if let Ok(mut parser) = reader_parser.lock() {
							parser.process(&buffer[..count]);
						} else {
							break;
						}
					}
					Err(_) => break,
				}
			}
		});

		let mut cmd = CommandBuilder::new("nvim");
		cmd.arg("--clean");
		cmd.arg(file_path.as_os_str());
		cmd.arg("+set mouse=");
		cmd.arg("+set list");
		cmd.arg("+set listchars=tab:>-,space:.,trail:~");
		cmd.arg("+syntax on");
		cmd.cwd(std::env::current_dir()?);
		cmd.env("TERM", "xterm-256color");

		let child = pair.slave.spawn_command(cmd).map_err(io_error)?;
		let writer = pair.master.take_writer().map_err(io_error)?;

		Ok(Self {
			file_path,
			parser,
			writer,
			master: pair.master,
			child,
			last_size: (INITIAL_ROWS, INITIAL_COLS),
			exit_status: None,
		})
	}

	fn resize(&mut self, area: Rect) {
		let rows = area.height.max(1);
		let cols = area.width.max(1);
		if (rows, cols) == self.last_size {
			return;
		}

		if let Err(error) = self.master.resize(PtySize {
			rows,
			cols,
			pixel_width: 0,
			pixel_height: 0,
		}) {
			self.exit_status = Some(format!("resize failed: {error}"));
			return;
		}

		if let Ok(mut parser) = self.parser.lock() {
			parser.screen_mut().set_size(rows, cols);
		}

		self.last_size = (rows, cols);
	}

	fn send_key(&mut self, key: KeyEvent) -> io::Result<()> {
		let payload = self.encode_key(key);
		self.writer.write_all(&payload)?;
		self.writer.flush()
	}

	fn send_paste(&mut self, text: &str) -> io::Result<()> {
		let bracketed = self
			.parser
			.lock()
			.map(|parser| parser.screen().bracketed_paste())
			.unwrap_or(false);

		if bracketed {
			self.writer.write_all(b"\x1b[200~")?;
		}
		self.writer.write_all(text.as_bytes())?;
		if bracketed {
			self.writer.write_all(b"\x1b[201~")?;
		}
		self.writer.flush()
	}

	fn snapshot(&self) -> NvimSnapshot {
		let Ok(parser) = self.parser.lock() else {
			return NvimSnapshot {
				lines: vec![Line::from("nvim screen unavailable")],
				cursor: None,
			};
		};

		let screen = parser.screen();
		let (rows, cols) = screen.size();
		let mut lines = Vec::with_capacity(rows as usize);

		for row in 0..rows {
			let mut spans = Vec::new();
			let mut current_text = String::new();
			let mut current_style: Option<Style> = None;

			for col in 0..cols {
				let Some(cell) = screen.cell(row, col) else {
					continue;
				};
				if cell.is_wide_continuation() {
					continue;
				}

				let text = if cell.has_contents() {
					cell.contents().to_string()
				} else {
					" ".to_string()
				};
				let style = vt_style_to_ratatui(cell, ui_theme());

				match current_style {
					Some(active) if active == style => current_text.push_str(&text),
					Some(active) => {
						spans.push(Span::styled(std::mem::take(&mut current_text), active));
						current_text.push_str(&text);
						current_style = Some(style);
					}
					None => {
						current_text.push_str(&text);
						current_style = Some(style);
					}
				}
			}

			if let Some(style) = current_style {
				spans.push(Span::styled(current_text, style));
			} else {
				spans.push(Span::raw(" "));
			}

			lines.push(Line::from(spans));
		}

		let cursor = if screen.hide_cursor() {
			None
		} else {
			Some(screen.cursor_position())
		};

		NvimSnapshot { lines, cursor }
	}

	fn encode_key(&self, key: KeyEvent) -> Vec<u8> {
		let app_cursor = self
			.parser
			.lock()
			.map(|parser| parser.screen().application_cursor())
			.unwrap_or(false);

		let mut bytes = Vec::new();
		if key.modifiers.contains(KeyModifiers::ALT) {
			bytes.push(0x1b);
		}

		match key.code {
			KeyCode::Backspace => bytes.push(0x7f),
			KeyCode::Enter => bytes.push(b'\r'),
			KeyCode::Left => bytes.extend_from_slice(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
			KeyCode::Right => {
				bytes.extend_from_slice(if app_cursor { b"\x1bOC" } else { b"\x1b[C" })
			}
			KeyCode::Up => bytes.extend_from_slice(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
			KeyCode::Down => bytes.extend_from_slice(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
			KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
			KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
			KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
			KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
			KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
			KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
			KeyCode::Tab => {
				if key.modifiers.contains(KeyModifiers::SHIFT) {
					bytes.extend_from_slice(b"\x1b[Z");
				} else {
					bytes.push(b'\t');
				}
			}
			KeyCode::Esc => bytes.push(0x1b),
			KeyCode::Char(ch) => {
				if key.modifiers.contains(KeyModifiers::CONTROL) {
					if let Some(ctrl) = encode_ctrl_char(ch) {
						bytes.push(ctrl);
					}
				} else {
					let mut utf8 = [0u8; 4];
					bytes.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
				}
			}
			_ => {}
		}

		bytes
	}

	fn poll_exit(&mut self) -> Option<String> {
		if self.exit_status.is_some() {
			return self.exit_status.clone();
		}

		match self.child.try_wait() {
			Ok(Some(status)) => {
				let message = format!("nvim exited: {status}");
				self.exit_status = Some(message.clone());
				Some(message)
			}
			Ok(None) => None,
			Err(error) => Some(format!("nvim status failed: {error}")),
		}
	}
}

impl Drop for NvimPane {
	fn drop(&mut self) {
		let _ = self.child.kill();
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
	nvim_editor(frame, editor_area, app);
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
			format!(" {} ", app.nvim.file_path.display()),
			Style::default().fg(app.ui.text),
		),
		Span::styled("EMBEDDED NVIM", Style::default().fg(app.ui.accent)),
	]))
	.block(panel("workspace", app.ui, false))
	.alignment(Alignment::Center);

	let status_text = Paragraph::new(Line::from(vec![
		Span::styled(
			" NVIM ",
			Style::default().fg(app.ui.bg).bg(app.ui.accent),
		),
		Span::raw(" "),
		Span::styled(app.focus.label(), Style::default().fg(app.ui.text)),
		Span::raw("  "),
		Span::styled(app.status_message.clone(), Style::default().fg(app.ui.muted)),
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
			Span::styled("nvim --clean", Style::default().fg(app.ui.text)),
		]),
		Line::styled(
			format!("open {}", app.nvim.file_path.display()),
			Style::default().fg(app.ui.text),
		),
		Line::styled("set mouse=", Style::default().fg(app.ui.text)),
		Line::styled("set list", Style::default().fg(app.ui.text)),
		Line::styled("set listchars=tab:>-,space:.,trail:~", Style::default().fg(app.ui.muted)),
		Line::default(),
		Line::styled("Ctrl-W change dashboard focus", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
		Line::styled("Esc quits only outside nvim pane", Style::default().fg(app.ui.text)),
	];

	let terminal = Paragraph::new(lines)
		.block(panel("terminal", app.ui, app.focus == Focus::Terminal))
		.wrap(Wrap { trim: false });

	frame.render_widget(terminal, area);
}

fn nvim_editor(frame: &mut Frame, area: Rect, app: &mut App) {
	let block = panel("nvim", app.ui, app.focus == Focus::Editor);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	app.nvim.resize(inner);
	let snapshot = app.nvim.snapshot();
	let editor = Paragraph::new(snapshot.lines).style(Style::default().bg(app.ui.panel));
	frame.render_widget(editor, inner);

	if app.focus == Focus::Editor {
		if let Some((row, col)) = snapshot.cursor {
			let cursor_x = inner.x.saturating_add(col);
			let cursor_y = inner.y.saturating_add(row);
			if cursor_x < inner.right() && cursor_y < inner.bottom() {
				frame.set_cursor_position((cursor_x, cursor_y));
			}
		}
	}
}

fn project_tree(frame: &mut Frame, area: Rect, app: &App) {
	let items = app
		.project_entries
		.iter()
		.map(|entry| {
			let style = if entry.ends_with(STARTUP_FILE) || entry.ends_with("src/main.rs") {
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
	let focus = app.focus == Focus::Performance;

	frame.render_widget(
		Gauge::default()
			.block(panel("gpu", app.ui, focus))
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
			Span::styled("embedded nvim", Style::default().fg(app.ui.text)),
		]),
		Line::styled("real vim keybindings live in the editor pane", Style::default().fg(app.ui.text)),
		Line::styled("tabs and spaces are shown by nvim listchars", Style::default().fg(app.ui.muted)),
		Line::styled("app only owns layout + focus switching", Style::default().fg(app.ui.muted)),
	];

	let codex = Paragraph::new(content)
		.block(panel("codex", app.ui, app.focus == Focus::Codex))
		.wrap(Wrap { trim: true });

	frame.render_widget(codex, area);
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
			format!("{indent}▾ {relative}")
		} else {
			format!("{indent}• {relative}")
		};
		entries.push(label);

		if path.is_dir() {
			collect_entries_recursive(root, &path, entries, depth + 1);
		}
	}
}

fn vt_style_to_ratatui(cell: &vt100::Cell, ui: UiTheme) -> Style {
	let mut fg = vt_color_to_ratatui(cell.fgcolor(), ui.text);
	let mut bg = vt_color_to_ratatui(cell.bgcolor(), ui.panel);

	if cell.inverse() {
		std::mem::swap(&mut fg, &mut bg);
	}

	let mut style = Style::default().fg(fg).bg(bg);
	if cell.bold() {
		style = style.add_modifier(Modifier::BOLD);
	}
	if cell.dim() {
		style = style.add_modifier(Modifier::DIM);
	}
	if cell.italic() {
		style = style.add_modifier(Modifier::ITALIC);
	}
	if cell.underline() {
		style = style.add_modifier(Modifier::UNDERLINED);
	}
	style
}

fn vt_color_to_ratatui(color: VtColor, default: Color) -> Color {
	match color {
		VtColor::Default => default,
		VtColor::Idx(idx) => ansi_index_to_color(idx),
		VtColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
	}
}

fn ansi_index_to_color(idx: u8) -> Color {
	match idx {
		0 => Color::Black,
		1 => Color::Red,
		2 => Color::Green,
		3 => Color::Yellow,
		4 => Color::Blue,
		5 => Color::Magenta,
		6 => Color::Cyan,
		7 => Color::Gray,
		8 => Color::DarkGray,
		9 => Color::LightRed,
		10 => Color::LightGreen,
		11 => Color::LightYellow,
		12 => Color::LightBlue,
		13 => Color::LightMagenta,
		14 => Color::LightCyan,
		15 => Color::White,
		16..=231 => {
			let index = idx - 16;
			let r = index / 36;
			let g = (index % 36) / 6;
			let b = index % 6;
			Color::Rgb(cube_value(r), cube_value(g), cube_value(b))
		}
		232..=255 => {
			let value = 8 + (idx - 232) * 10;
			Color::Rgb(value, value, value)
		}
	}
}

fn cube_value(index: u8) -> u8 {
	match index {
		0 => 0,
		_ => 55 + index * 40,
	}
}

fn encode_ctrl_char(ch: char) -> Option<u8> {
	match ch {
		'a'..='z' => Some((ch as u8) - b'a' + 1),
		'A'..='Z' => Some((ch as u8) - b'A' + 1),
		' ' => Some(0),
		'[' => Some(27),
		'\\' => Some(28),
		']' => Some(29),
		'^' => Some(30),
		'_' => Some(31),
		_ => None,
	}
}

fn is_key_press(kind: KeyEventKind) -> bool {
	matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
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

fn io_error(error: impl std::fmt::Display) -> io::Error {
	io::Error::other(error.to_string())
}
