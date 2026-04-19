use std::{
	collections::BTreeSet,
	env, fs,
	io::{self, Read, Write},
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	thread,
	time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
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
	ui: UiTheme,
	project_tree: ProjectTree,
	nvim: PtyPane,
	terminal: PtyPane,
}

struct PtyPane {
	title: &'static str,
	parser: Arc<Mutex<Parser>>,
	writer: Box<dyn Write + Send>,
	master: Box<dyn MasterPty + Send>,
	child: Box<dyn Child + Send + Sync>,
	last_size: (u16, u16),
	exit_status: Option<String>,
}

struct PtySnapshot {
	lines: Vec<Line<'static>>,
	cursor: Option<(u16, u16)>,
}

struct ProjectTree {
	root: PathBuf,
	expanded: BTreeSet<PathBuf>,
	visible: Vec<TreeEntry>,
	selected: usize,
}

#[derive(Clone)]
struct TreeEntry {
	path: PathBuf,
	depth: usize,
	is_dir: bool,
}

enum TreeAction {
	OpenFile(PathBuf),
	ToggleDir,
}

impl App {
	fn new(file_path: PathBuf) -> io::Result<Self> {
		let root = env::current_dir()?;
		let mut project_tree = ProjectTree::new(root.clone());
		project_tree.select_path(&file_path);

		Ok(Self {
			focus: Focus::Editor,
			status_message: "embedded nvim + terminal ready".to_string(),
			ui: ui_theme(),
			project_tree,
			nvim: PtyPane::spawn_nvim(file_path)?,
			terminal: PtyPane::spawn_shell(root)?,
		})
	}

	fn tick(&mut self) {
		if let Some(status) = self.nvim.poll_exit() {
			self.status_message = status;
		}
		if let Some(status) = self.terminal.poll_exit() {
			self.status_message = status;
		}
	}

	fn handle_event(&mut self, event: Event) -> AppAction {
		match event {
			Event::Key(key) if is_key_press(key.kind) => self.handle_key(key),
			Event::Paste(text) => {
				match self.focus {
					Focus::Editor => {
						if let Err(error) = self.nvim.send_paste(&text) {
							self.status_message = format!("nvim paste failed: {error}");
						}
					}
					Focus::Terminal => {
						if let Err(error) = self.terminal.send_paste(&text) {
							self.status_message = format!("terminal paste failed: {error}");
						}
					}
					_ => {}
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

		match self.focus {
			Focus::Editor => self.forward_key_to_pane(key, true),
			Focus::Terminal => self.forward_key_to_pane(key, false),
			Focus::ProjectTree => self.handle_project_tree_key(key),
			Focus::Performance | Focus::Codex => {
				if key.code == KeyCode::Esc {
					AppAction::Quit
				} else {
					AppAction::Continue
				}
			}
		}
	}

	fn forward_key_to_pane(&mut self, key: KeyEvent, editor: bool) -> AppAction {
		let result = if editor {
			self.nvim.send_key(key)
		} else {
			self.terminal.send_key(key)
		};

		if let Err(error) = result {
			let label = if editor { "nvim" } else { "terminal" };
			self.status_message = format!("{label} input failed: {error}");
		}

		AppAction::Continue
	}

	fn handle_project_tree_key(&mut self, key: KeyEvent) -> AppAction {
		match key.code {
			KeyCode::Esc => AppAction::Quit,
			KeyCode::Up => {
				self.project_tree.move_selection(-1);
				AppAction::Continue
			}
			KeyCode::Down => {
				self.project_tree.move_selection(1);
				AppAction::Continue
			}
			KeyCode::Enter => {
				match self.project_tree.activate_selected() {
					Some(TreeAction::ToggleDir) => {
						self.status_message = "toggled directory".to_string();
					}
					Some(TreeAction::OpenFile(path)) => {
						if let Err(error) = self.nvim.open_file(&path) {
							self.status_message = format!("open failed: {error}");
						} else {
							self.focus = Focus::Editor;
							self.status_message = format!("opened {}", path.display());
						}
					}
					None => {}
				}
				AppAction::Continue
			}
			_ => AppAction::Continue,
		}
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

impl PtyPane {
	fn spawn_nvim(file_path: PathBuf) -> io::Result<Self> {
		let mut cmd = CommandBuilder::new("nvim");
		cmd.arg("--clean");
		cmd.arg("-n");
		cmd.arg(file_path.as_os_str());
		cmd.arg("+set mouse=");
		cmd.arg("+set list");
		cmd.arg("+set listchars=tab:>-,space:.,trail:~");
		cmd.arg("+syntax on");
		cmd.cwd(env::current_dir()?);
		cmd.env("TERM", "xterm-256color");
		Self::spawn("nvim", cmd)
	}

	fn spawn_shell(cwd: PathBuf) -> io::Result<Self> {
		let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
		let mut cmd = CommandBuilder::new(shell);
		cmd.arg("-i");
		cmd.cwd(cwd);
		cmd.env("TERM", "xterm-256color");
		Self::spawn("terminal", cmd)
	}

	fn spawn(title: &'static str, cmd: CommandBuilder) -> io::Result<Self> {
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

		let child = pair.slave.spawn_command(cmd).map_err(io_error)?;
		let writer = pair.master.take_writer().map_err(io_error)?;

		Ok(Self {
			title,
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
			self.exit_status = Some(format!("{} resize failed: {error}", self.title));
			return;
		}

		if let Ok(mut parser) = self.parser.lock() {
			parser.screen_mut().set_size(rows, cols);
		}

		self.last_size = (rows, cols);
	}

	fn send_key(&mut self, key: KeyEvent) -> io::Result<()> {
		let payload = self.encode_key(key);
		if payload.is_empty() {
			return Ok(());
		}
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

	fn open_file(&mut self, path: &Path) -> io::Result<()> {
		let escaped = escape_nvim_path(path);
		let command = format!("\x1b:edit {escaped}\r");
		self.writer.write_all(command.as_bytes())?;
		self.writer.flush()
	}

	fn snapshot(&self, ui: UiTheme) -> PtySnapshot {
		let Ok(parser) = self.parser.lock() else {
			return PtySnapshot {
				lines: vec![Line::from("terminal unavailable")],
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
				let style = vt_style_to_ratatui(cell, ui);

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

		PtySnapshot { lines, cursor }
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
				let message = format!("{} exited: {status}", self.title);
				self.exit_status = Some(message.clone());
				Some(message)
			}
			Ok(None) => None,
			Err(error) => Some(format!("{} status failed: {error}", self.title)),
		}
	}
}

impl Drop for PtyPane {
	fn drop(&mut self) {
		let _ = self.child.kill();
	}
}

impl ProjectTree {
	fn new(root: PathBuf) -> Self {
		let mut expanded = BTreeSet::new();
		expanded.insert(root.clone());
		let src = root.join("src");
		if src.is_dir() {
			expanded.insert(src);
		}

		let mut tree = Self {
			root,
			expanded,
			visible: Vec::new(),
			selected: 0,
		};
		tree.refresh(None);
		tree
	}

	fn refresh(&mut self, selected_path: Option<PathBuf>) {
		self.visible.clear();
		let root = self.root.clone();
		self.collect_entries(&root, 0);

		if self.visible.is_empty() {
			self.selected = 0;
			return;
		}

		if let Some(path) = selected_path {
			self.select_path(&path);
		} else if self.selected >= self.visible.len() {
			self.selected = self.visible.len() - 1;
		}
	}

	fn collect_entries(&mut self, dir: &Path, depth: usize) {
		let Ok(read_dir) = fs::read_dir(dir) else {
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

			let is_dir = path.is_dir();
			self.visible.push(TreeEntry {
				path: path.clone(),
				depth,
				is_dir,
			});

			if is_dir && self.expanded.contains(&path) {
				self.collect_entries(&path, depth + 1);
			}
		}
	}

	fn move_selection(&mut self, delta: isize) {
		if self.visible.is_empty() {
			self.selected = 0;
			return;
		}

		let current = self.selected as isize + delta;
		let max = self.visible.len().saturating_sub(1) as isize;
		self.selected = current.clamp(0, max) as usize;
	}

	fn select_path(&mut self, path: &Path) {
		if let Some(index) = self.visible.iter().position(|entry| entry.path == path) {
			self.selected = index;
		}
	}

	fn activate_selected(&mut self) -> Option<TreeAction> {
		let entry = self.visible.get(self.selected)?.clone();
		if entry.is_dir {
			if self.expanded.contains(&entry.path) {
				self.expanded.remove(&entry.path);
			} else {
				self.expanded.insert(entry.path.clone());
			}
			self.refresh(Some(entry.path));
			Some(TreeAction::ToggleDir)
		} else {
			Some(TreeAction::OpenFile(entry.path))
		}
	}
}

fn render(frame: &mut Frame, app: &mut App) {
	let area = frame.area();
	frame.render_widget(Block::default().style(Style::default().bg(app.ui.bg)), area);

	let [left, right] = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
		.areas(area);

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

	render_pty_pane(frame, terminal_area, app.ui, app.focus == Focus::Terminal, &mut app.terminal);
	performance_block(frame, performance_area, app);
	project_tree(frame, left_bottom, app);
	render_pty_pane(frame, editor_area, app.ui, app.focus == Focus::Editor, &mut app.nvim);
	codex_block(frame, codex_area, app);
}

fn render_pty_pane(
	frame: &mut Frame,
	area: Rect,
	ui: UiTheme,
	focused: bool,
	pane: &mut PtyPane,
) {
	let block = panel(pane.title, ui, focused);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	pane.resize(inner);
	let snapshot = pane.snapshot(ui);
	let widget = Paragraph::new(snapshot.lines).style(Style::default().bg(ui.panel));
	frame.render_widget(widget, inner);

	if focused {
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
		.project_tree
		.visible
		.iter()
		.enumerate()
		.map(|(index, entry)| {
			let relative = entry
				.path
				.strip_prefix(&app.project_tree.root)
				.unwrap_or(&entry.path)
				.display()
				.to_string();
			let indent = "  ".repeat(entry.depth);
			let symbol = if entry.is_dir {
				if app.project_tree.expanded.contains(&entry.path) {
					"▾"
				} else {
					"▸"
				}
			} else {
				"•"
			};

			let style = if index == app.project_tree.selected {
				Style::default()
					.fg(app.ui.bg)
					.bg(app.ui.accent)
					.add_modifier(Modifier::BOLD)
			} else if entry.path == app.project_tree.root.join(STARTUP_FILE) {
				Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)
			} else {
				Style::default().fg(app.ui.text)
			};

			ListItem::new(Line::from(Span::styled(
				format!("{indent}{symbol} {relative}"),
				style,
			)))
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
			Span::styled("status", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
			Span::raw(": "),
			Span::styled(app.status_message.clone(), Style::default().fg(app.ui.text)),
		]),
		Line::styled("Ctrl-W cycles pane focus", Style::default().fg(app.ui.text)),
		Line::styled("project tree: Up/Down select, Enter expand/open", Style::default().fg(app.ui.muted)),
		Line::styled("editor + terminal are real PTYs", Style::default().fg(app.ui.muted)),
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

fn escape_nvim_path(path: &Path) -> String {
	path.display()
		.to_string()
		.replace('\\', "\\\\")
		.replace(' ', "\\ ")
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
