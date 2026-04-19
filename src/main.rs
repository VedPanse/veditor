use std::{
	collections::{BTreeSet, HashMap},
	env, fs,
	io::{self, Read, Write},
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{
		mpsc::{self, Receiver, Sender},
		Arc, Mutex,
	},
	thread,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageReader};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	prelude::*,
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, BorderType, Gauge, List, ListItem, ListState, Paragraph, Sparkline, Wrap},
	DefaultTerminal,
};
use vt100::{Color as VtColor, Parser};

const ACCENT_COLOR: &str = "#ffffff";
const STARTUP_FILE: &str = "src/main.rs";
const TICK_RATE: Duration = Duration::from_millis(33);
const INITIAL_ROWS: u16 = 40;
const INITIAL_COLS: u16 = 120;
const METRICS_SAMPLE_RATE: Duration = Duration::from_millis(350);
const HISTORY_POINTS: usize = 32;
const PROJECT_TREE_SEARCH_TIMEOUT: Duration = Duration::from_millis(1200);
const DOCUMENT_PREVIEW_MAX_LINES: usize = 1200;

fn main() -> io::Result<()> {
	let mut app = App::new(env::args_os().nth(1).map(PathBuf::from))?;
	ratatui::run(|terminal| run_app(terminal, &mut app))
}

fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
	loop {
		app.tick();
		terminal.draw(|frame| render(frame, app))?;

		if event::poll(TICK_RATE)? {
			match app.handle_event(event::read()?) {
				AppAction::Continue => {}
				AppAction::Quit => {
					let _ = app.persist_session_state(true);
					break Ok(());
				}
			}
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
	Editor,
	Terminal,
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
	accent_soft: Color,
	accent_dim: Color,
	bg: Color,
	panel: Color,
	panel_alt: Color,
	text: Color,
	muted: Color,
	border: Color,
	selection: Color,
	special: Color,
	type_color: Color,
	ansi: [Color; 16],
}

struct App {
	focus: Focus,
	status_message: String,
	ui: UiTheme,
	project_tree: ProjectTree,
	codex_chat: CodexChat,
	codex_tx: Sender<CodexResult>,
	codex_rx: Receiver<CodexResult>,
	next_codex_request_id: u64,
	pending_codex_request: Option<u64>,
	project_picker: Option<ProjectPicker>,
	create_prompt: Option<CreatePrompt>,
	command_prompt: Option<String>,
	editor_preview: Option<EditorPreview>,
	open_files: Vec<PathBuf>,
	active_file: Option<PathBuf>,
	nvim: PtyPane,
	terminal: PtyPane,
	terminal_metrics: TerminalMetrics,
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

struct TerminalMetrics {
	shell_pid: Option<u32>,
	active_process: String,
	process_count: usize,
	cpu_percent: f32,
	mem_bytes: u64,
	gpu_percent: Option<f32>,
	cpu_history: Vec<u64>,
	last_sample: Instant,
	total_memory_bytes: u64,
	note: String,
}

struct TerminalProcessSample {
	active_process: String,
	process_count: usize,
	cpu_percent: f32,
	mem_bytes: u64,
	gpu_percent: Option<f32>,
}

struct ProcessSnapshot {
	pid: u32,
	cpu_percent: f32,
	rss_kib: u64,
	command: String,
}

struct CreatePrompt {
	kind: CreateKind,
	base_dir: PathBuf,
	input: String,
}

enum EditorPreview {
	Image(ImagePreview),
	Document(DocumentPreview),
}

struct ImagePreview {
	path: PathBuf,
	image: DynamicImage,
	cache: Option<ImagePreviewCache>,
}

struct DocumentPreview {
	path: PathBuf,
	kind: &'static str,
	summary: String,
	lines: Vec<Line<'static>>,
}

struct ImagePreviewCache {
	width: u16,
	height: u16,
	panel: Color,
	lines: Vec<Line<'static>>,
}

impl EditorPreview {
	fn path(&self) -> &Path {
		match self {
			Self::Image(preview) => &preview.path,
			Self::Document(preview) => &preview.path,
		}
	}
}

struct ProjectPicker {
	current_dir: PathBuf,
	entries: Vec<ProjectPickerEntry>,
	selected: usize,
	search_query: String,
	search_match: Option<PathBuf>,
	last_search_input: Option<Instant>,
}

struct ProjectPickerEntry {
	path: PathBuf,
	label: String,
}

struct CodexChat {
	messages: Vec<ChatMessage>,
	input: String,
}

struct ChatMessage {
	role: ChatRole,
	content: String,
	pending_request_id: Option<u64>,
}

#[derive(Clone, Copy)]
enum ChatRole {
	User,
	Assistant,
}

#[derive(Clone, Copy)]
enum CreateKind {
	File,
	Directory,
}

struct ProjectTree {
	root: PathBuf,
	expanded: BTreeSet<PathBuf>,
	visible: Vec<TreeEntry>,
	selected: usize,
	search_query: String,
	search_match: Option<PathBuf>,
	last_search_input: Option<Instant>,
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

enum TreeSearchUpdate {
	Cleared,
	Matched(PathBuf),
	NoMatch,
	Unchanged,
}

enum ProjectPickerSearchUpdate {
	Cleared,
	Matched(PathBuf),
	NoMatch,
	Unchanged,
}

struct CodexResult {
	request_id: u64,
	reply: Result<String, String>,
}

struct SessionState {
	root: PathBuf,
	open_files: Vec<PathBuf>,
	active_file: Option<PathBuf>,
	accent_hex: Option<String>,
}

struct NvimBufferState {
	files: Vec<PathBuf>,
	current: Option<PathBuf>,
}

impl App {
	fn new(requested_path: Option<PathBuf>) -> io::Result<Self> {
		let saved_session = load_saved_session();
		let accent_hex = saved_session
			.as_ref()
			.and_then(|session| session.accent_hex.as_deref())
			.and_then(normalize_hex_color)
			.unwrap_or_else(|| ACCENT_COLOR.to_string());
		let cwd = env::current_dir()?;
		let (root, saved_session, requested_path) = if let Some(requested_path) = requested_path {
			let requested_path = absolutize_path(&cwd, &requested_path);
			let requested_path = if requested_path.exists() {
				fs::canonicalize(&requested_path).unwrap_or(requested_path)
			} else {
				requested_path
			};
			let root = if requested_path.is_dir() {
				requested_path.clone()
			} else {
				requested_path
					.parent()
					.unwrap_or(&cwd)
					.to_path_buf()
			};
			(root, None, Some(requested_path))
		} else {
			let root = saved_session
				.as_ref()
				.map(|session| session.root.clone())
				.unwrap_or(cwd);
			(root, saved_session, None)
		};
		let ui = ui_theme(&accent_hex);
		let restored_files = saved_session
			.as_ref()
			.map(|session| sanitize_session_files(&root, &session.open_files))
			.unwrap_or_default();
		let restored_active = saved_session
			.as_ref()
			.and_then(|session| sanitize_session_active_file(&root, session.active_file.as_ref()));
		let startup_target = resolve_startup_target(
			&root,
			requested_path
				.as_deref()
				.or(restored_active.as_deref())
				.or_else(|| restored_files.first().map(PathBuf::as_path))
				.unwrap_or(Path::new(STARTUP_FILE)),
		);
		let initial_editor_target = initial_editor_target(&root, &restored_files, &startup_target);
		let mut project_tree = ProjectTree::new(root.clone());
		project_tree.expand_to(&startup_target);
		project_tree.select_path(&startup_target);
		let (codex_tx, codex_rx) = mpsc::channel();
		let mut app = Self {
			terminal_metrics: TerminalMetrics::new(None),
			focus: Focus::Editor,
			status_message: "embedded nvim + terminal ready".to_string(),
			ui,
			project_tree,
			codex_chat: CodexChat::new(&root, &startup_target),
			codex_tx,
			codex_rx,
			next_codex_request_id: 1,
			pending_codex_request: None,
			project_picker: None,
			create_prompt: None,
			command_prompt: None,
			editor_preview: None,
			open_files: Vec::new(),
			active_file: None,
			nvim: PtyPane::spawn_nvim(initial_editor_target.clone(), root.clone(), ui)?,
			terminal: PtyPane::spawn_shell(root)?,
		}
		.with_metrics();
		app.restore_session_files(restored_files, restored_active, initial_editor_target)?;
		let _ = app.persist_session_state(false);
		Ok(app)
	}

	fn tick(&mut self) {
		if let Some(status) = self.nvim.poll_exit() {
			self.status_message = status;
		}
		if let Some(status) = self.terminal.poll_exit() {
			self.status_message = status;
		}
		self.project_tree.expire_search();
		if let Some(picker) = &mut self.project_picker {
			picker.expire_search();
		}
		self.receive_codex_replies();
		self.refresh_terminal_metrics(false);
	}

	fn handle_event(&mut self, event: Event) -> AppAction {
		match event {
			Event::Key(key) if is_key_press(key.kind) => self.handle_key(key),
			Event::Paste(text) => {
				match self.focus {
					Focus::Editor => {
						if self.editor_preview.is_some() {
							self.status_message = "image preview is read-only".to_string();
						} else if let Err(error) = self.nvim.send_paste(&text) {
							self.status_message = format!("nvim paste failed: {error}");
						}
					}
					Focus::Terminal => {
						if let Err(error) = self.terminal.send_paste(&text) {
							self.status_message = format!("terminal paste failed: {error}");
						}
					}
					Focus::ProjectTree => self.push_project_tree_prompt_text(&text),
					Focus::Codex => self.codex_chat.input.push_str(&text),
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
			Focus::Codex => self.handle_codex_key(key),
		}
	}

	fn forward_key_to_pane(&mut self, key: KeyEvent, editor: bool) -> AppAction {
		if editor && self.editor_preview.is_some() {
			if key.code == KeyCode::Enter {
				self.status_message = "image preview open".to_string();
			}
			return AppAction::Continue;
		}

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
		if is_project_open_shortcut(key) {
			self.create_prompt = None;
			self.command_prompt = None;
			self.begin_project_switch_prompt();
			return AppAction::Continue;
		}

		if self.create_prompt.is_some() {
			return self.handle_create_prompt_key(key);
		}

		if self.command_prompt.is_some() {
			return self.handle_command_prompt_key(key);
		}

		if is_command_prompt_start(key) {
			self.begin_command_prompt();
			return AppAction::Continue;
		}

		if self.project_picker.is_some() {
			return self.handle_project_picker_key(key);
		}

		if key.modifiers.contains(KeyModifiers::CONTROL) {
			match key.code {
				KeyCode::Char('d') | KeyCode::Char('D') => {
					self.begin_create_prompt(CreateKind::Directory, self.project_tree.root.clone());
					return AppAction::Continue;
				}
				KeyCode::Char('n') => {
					if let Some(dir) = self.selected_directory() {
						self.begin_create_prompt(CreateKind::File, dir);
					} else {
						self.status_message = "select a directory for Ctrl-N".to_string();
					}
					return AppAction::Continue;
				}
				KeyCode::Char('N') => {
					if let Some(dir) = self.selected_directory() {
						self.begin_create_prompt(CreateKind::Directory, dir);
					} else {
						self.status_message = "select a directory for Ctrl-Shift-N".to_string();
					}
					return AppAction::Continue;
				}
				_ => {}
			}
		}

		match key.code {
			KeyCode::Char('%') if !self.project_tree.search_active() => {
				self.begin_create_prompt(CreateKind::File, self.project_tree.root.clone());
				AppAction::Continue
			}
			KeyCode::Esc => {
				if self.project_tree.search_active() {
					self.project_tree.clear_search();
					self.status_message = "cleared tree search".to_string();
					AppAction::Continue
				} else {
					AppAction::Quit
				}
			}
			KeyCode::Backspace => {
				let update = self.project_tree.backspace_search();
				self.apply_project_tree_search_update(update);
				AppAction::Continue
			}
			KeyCode::Up => {
				self.project_tree.clear_search();
				self.project_tree.move_selection(-1);
				AppAction::Continue
			}
			KeyCode::Down => {
				self.project_tree.clear_search();
				self.project_tree.move_selection(1);
				AppAction::Continue
			}
			KeyCode::Enter => {
				if self.project_tree.search_active() && !self.project_tree.has_search_match() {
					self.status_message = format!("no match for {}", self.project_tree.search_query);
					return AppAction::Continue;
				}

				self.project_tree.clear_search();
				match self.project_tree.activate_selected() {
					Some(TreeAction::ToggleDir) => {
						self.status_message = "toggled directory".to_string();
					}
					Some(TreeAction::OpenFile(path)) => {
						if let Err(error) = self.open_file_in_editor(path.clone()) {
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
			KeyCode::Char(ch)
				if !key
					.modifiers
					.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
			{
				let update = self.project_tree.push_search_text(&ch.to_string());
				self.apply_project_tree_search_update(update);
				AppAction::Continue
			}
			_ => AppAction::Continue,
		}
	}

	fn handle_create_prompt_key(&mut self, key: KeyEvent) -> AppAction {
		match key.code {
			KeyCode::Esc => {
				self.create_prompt = None;
				self.status_message = "creation cancelled".to_string();
			}
			KeyCode::Enter => {
				if let Err(error) = self.commit_create_prompt() {
					self.status_message = format!("create failed: {error}");
				}
			}
			KeyCode::Backspace => {
				if let Some(prompt) = &mut self.create_prompt {
					prompt.input.pop();
				}
			}
			KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
				if let Some(prompt) = &mut self.create_prompt {
					prompt.input.push(ch);
				}
			}
			_ => {}
		}

		AppAction::Continue
	}

	fn handle_command_prompt_key(&mut self, key: KeyEvent) -> AppAction {
		match key.code {
			KeyCode::Esc => {
				self.command_prompt = None;
				self.status_message = "command cancelled".to_string();
			}
			KeyCode::Enter => {
				if let Err(error) = self.commit_command_prompt() {
					self.status_message = format!("command failed: {error}");
				}
			}
			KeyCode::Backspace => {
				if let Some(prompt) = &mut self.command_prompt {
					if prompt.len() > 1 {
						prompt.pop();
					} else {
						self.command_prompt = None;
						self.status_message = "command cancelled".to_string();
					}
				}
			}
			KeyCode::Char(ch)
				if !key
					.modifiers
					.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
			{
				self.push_command_prompt_text(&ch.to_string());
			}
			_ => {}
		}

		AppAction::Continue
	}

	fn handle_project_picker_key(&mut self, key: KeyEvent) -> AppAction {
		match key.code {
			KeyCode::Esc => {
				if self
					.project_picker
					.as_ref()
					.is_some_and(ProjectPicker::search_active)
				{
					if let Some(picker) = &mut self.project_picker {
						picker.clear_search();
					}
					self.status_message = "cleared project search".to_string();
				} else {
					self.project_picker = None;
					self.status_message = "project switch cancelled".to_string();
				}
			}
			KeyCode::Up => {
				if let Some(picker) = &mut self.project_picker {
					picker.clear_search();
					picker.move_selection(-1);
				}
			}
			KeyCode::Down => {
				if let Some(picker) = &mut self.project_picker {
					picker.clear_search();
					picker.move_selection(1);
				}
			}
			KeyCode::Enter => {
				if self
					.project_picker
					.as_ref()
					.is_some_and(|picker| picker.search_active() && !picker.has_search_match())
				{
					if let Some(picker) = &self.project_picker {
						self.status_message =
							format!("no project match for {}", picker.search_query);
					}
					return AppAction::Continue;
				}

				if let Err(error) =
					self.commit_project_picker_selection(key.modifiers.contains(KeyModifiers::SHIFT))
				{
					self.status_message = format!("switch failed: {error}");
				}
			}
			KeyCode::Backspace if self
				.project_picker
				.as_ref()
				.is_some_and(ProjectPicker::search_active) =>
			{
				let update = if let Some(picker) = &mut self.project_picker {
					picker.backspace_search()
				} else {
					ProjectPickerSearchUpdate::Unchanged
				};
				self.apply_project_picker_search_update(update);
			}
			KeyCode::Left | KeyCode::Backspace => {
				if let Err(error) = self.project_picker_parent() {
					self.status_message = format!("switch failed: {error}");
				}
			}
			KeyCode::Right => {
				if let Err(error) = self.project_picker_descend() {
					self.status_message = format!("switch failed: {error}");
				}
			}
			KeyCode::Char(ch)
				if !key
					.modifiers
					.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
			{
				let update = if let Some(picker) = &mut self.project_picker {
					picker.push_search_text(&ch.to_string())
				} else {
					ProjectPickerSearchUpdate::Unchanged
				};
				self.apply_project_picker_search_update(update);
			}
			_ => {}
		}

		AppAction::Continue
	}

	fn handle_codex_key(&mut self, key: KeyEvent) -> AppAction {
		match key.code {
			KeyCode::Esc => {
				if self.codex_chat.input.is_empty() {
					AppAction::Quit
				} else {
					self.codex_chat.input.clear();
					self.status_message = "cleared codex input".to_string();
					AppAction::Continue
				}
			}
			KeyCode::Enter => {
				self.submit_codex_prompt();
				AppAction::Continue
			}
			KeyCode::Backspace => {
				self.codex_chat.input.pop();
				AppAction::Continue
			}
			KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
				self.codex_chat.input.push(ch);
				AppAction::Continue
			}
			_ => AppAction::Continue,
		}
	}

	fn open_file_in_editor(&mut self, path: PathBuf) -> io::Result<()> {
		if uses_editor_preview(&path) {
			self.editor_preview = Some(load_editor_preview(path.clone())?);
			self.mark_file_open(path);
			let _ = self.persist_session_state(false);
			return Ok(());
		}

		self.editor_preview = None;
		if self.nvim.is_exited() {
			self.nvim = PtyPane::spawn_nvim(path.clone(), self.project_tree.root.clone(), self.ui)?;
			self.mark_file_open(path);
			let _ = self.persist_session_state(false);
			return Ok(());
		}

		self.nvim.open_file(&path)?;
		self.mark_file_open(path);
		let _ = self.persist_session_state(false);
		Ok(())
	}

	fn with_metrics(mut self) -> Self {
		self.refresh_terminal_metrics(true);
		self
	}

	fn begin_project_switch_prompt(&mut self) {
		self.project_tree.clear_search();
		let start_dir = self
			.project_tree
			.root
			.parent()
			.unwrap_or(&self.project_tree.root)
			.to_path_buf();
		match ProjectPicker::new(start_dir, Some(self.project_tree.root.clone())) {
			Ok(picker) => {
				self.project_picker = Some(picker);
				self.status_message =
					"select a project. enter opens project, shift+enter opens directory".to_string();
			}
			Err(error) => {
				self.status_message = format!("switch failed: {error}");
			}
		}
	}

	fn begin_create_prompt(&mut self, kind: CreateKind, base_dir: PathBuf) {
		self.project_tree.clear_search();
		let scope = if base_dir == self.project_tree.root {
			"root".to_string()
		} else {
			base_dir
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or("directory")
				.to_string()
		};
		let label = match kind {
			CreateKind::File => "new file",
			CreateKind::Directory => "new directory",
		};

		self.create_prompt = Some(CreatePrompt {
			kind,
			base_dir,
			input: String::new(),
		});
		self.status_message = format!("{label} in {scope}");
	}

	fn begin_command_prompt(&mut self) {
		self.project_tree.clear_search();
		if let Some(picker) = &mut self.project_picker {
			picker.clear_search();
		}
		self.command_prompt = Some(":".to_string());
		self.status_message = "command mode".to_string();
	}

	fn push_project_tree_prompt_text(&mut self, text: &str) {
		if self.create_prompt.is_some() {
			self.push_create_prompt_text(text);
			return;
		}
		if self.command_prompt.is_some() {
			self.push_command_prompt_text(text);
			return;
		}
		if text.starts_with(':') {
			self.begin_command_prompt();
			if let Some(rest) = text.strip_prefix(':') {
				self.push_command_prompt_text(rest);
			}
			return;
		}
		if self.project_picker.is_some() {
			let update = if let Some(picker) = &mut self.project_picker {
				picker.push_search_text(text)
			} else {
				ProjectPickerSearchUpdate::Unchanged
			};
			self.apply_project_picker_search_update(update);
			return;
		}

		let update = self.project_tree.push_search_text(text);
		self.apply_project_tree_search_update(update);
	}

	fn push_create_prompt_text(&mut self, text: &str) {
		if let Some(prompt) = &mut self.create_prompt {
			prompt.input.push_str(text);
		}
	}

	fn push_command_prompt_text(&mut self, text: &str) {
		if let Some(prompt) = &mut self.command_prompt {
			let sanitized = text
				.chars()
				.filter(|ch| *ch != '\n' && *ch != '\r')
				.collect::<String>();
			prompt.push_str(&sanitized);
		}
	}

	fn apply_project_tree_search_update(&mut self, update: TreeSearchUpdate) {
		match update {
			TreeSearchUpdate::Cleared => {
				self.status_message = "cleared tree search".to_string();
			}
			TreeSearchUpdate::Matched(path) => {
				self.status_message =
					format!("tree search: {}", relative_to_root(&self.project_tree.root, &path));
			}
			TreeSearchUpdate::NoMatch => {
				self.status_message = format!("no match for {}", self.project_tree.search_query);
			}
			TreeSearchUpdate::Unchanged => {}
		}
	}

	fn apply_project_picker_search_update(&mut self, update: ProjectPickerSearchUpdate) {
		match update {
			ProjectPickerSearchUpdate::Cleared => {
				self.status_message = "cleared project search".to_string();
			}
			ProjectPickerSearchUpdate::Matched(path) => {
				if let Some(picker) = &self.project_picker {
					self.status_message =
						format!("project search: {}", relative_to_root(&picker.current_dir, &path));
				}
			}
			ProjectPickerSearchUpdate::NoMatch => {
				if let Some(picker) = &self.project_picker {
					self.status_message = format!("no project match for {}", picker.search_query);
				}
			}
			ProjectPickerSearchUpdate::Unchanged => {}
		}
	}

	fn commit_command_prompt(&mut self) -> io::Result<()> {
		let Some(command) = self.command_prompt.as_ref() else {
			return Ok(());
		};

		let trimmed = command.trim();
		let mut parts = trimmed.split_whitespace();
		match (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) {
			(Some(":set"), Some("accent"), Some(value), None, None) => {
				let hex = normalize_hex_color(value).ok_or_else(|| {
					io_error("usage: :set accent #RRGGBB")
				})?;
				self.ui = ui_theme(&hex);
				if !self.nvim.is_exited() {
					self.nvim.apply_theme(self.ui)?;
				}
				let _ = self.persist_session_state(false);
				self.command_prompt = None;
				self.status_message = format!("accent set to {hex}");
				Ok(())
			}
			_ => Err(io_error("unknown command")),
		}
	}

	fn selected_directory(&self) -> Option<PathBuf> {
		let entry = self.project_tree.visible.get(self.project_tree.selected)?;
		entry.is_dir.then(|| entry.path.clone())
	}

	fn commit_create_prompt(&mut self) -> io::Result<()> {
		let Some(prompt) = self.create_prompt.as_ref() else {
			return Ok(());
		};

		let kind = prompt.kind;
		let base_dir = prompt.base_dir.clone();
		let name = prompt.input.trim().to_string();
		if name.is_empty() {
			self.status_message = "name cannot be empty".to_string();
			return Ok(());
		}

		let target = base_dir.join(&name);
		if target.exists() {
			self.status_message = format!("already exists: {}", target.display());
			return Ok(());
		}

		match kind {
			CreateKind::File => {
				fs::write(&target, [])?;
				self.project_tree.expand_to(&target);
				self.project_tree.refresh(Some(target.clone()));
				self.open_file_in_editor(target.clone())?;
				self.focus = Focus::Editor;
				self.status_message = format!("created file {}", target.display());
			}
			CreateKind::Directory => {
				fs::create_dir(&target)?;
				self.project_tree.expand_to(&target);
				self.project_tree.refresh(Some(target.clone()));
				self.focus = Focus::ProjectTree;
				self.status_message = format!("created directory {}", target.display());
			}
		}

		self.create_prompt = None;
		Ok(())
	}

	fn commit_project_picker_selection(&mut self, open_directory: bool) -> io::Result<()> {
		let Some(path) = self
			.project_picker
			.as_ref()
			.and_then(ProjectPicker::selected_path)
		else {
			return Ok(());
		};

		let root = if open_directory {
			path
		} else {
			default_project_root(&path)
		};

		self.switch_project(root)?;
		self.project_picker = None;
		Ok(())
	}

	fn project_picker_parent(&mut self) -> io::Result<()> {
		let Some(picker) = &mut self.project_picker else {
			return Ok(());
		};
		let Some(parent) = picker.current_dir.parent().map(Path::to_path_buf) else {
			return Ok(());
		};

		let previous = picker.current_dir.clone();
		picker.clear_search();
		picker.set_dir(parent, Some(previous))?;
		self.status_message = format!("browsing {}", picker.current_dir.display());
		Ok(())
	}

	fn project_picker_descend(&mut self) -> io::Result<()> {
		let Some(picker) = &mut self.project_picker else {
			return Ok(());
		};
		let Some(path) = picker.selected_path() else {
			return Ok(());
		};

		picker.clear_search();
		picker.set_dir(path.clone(), None)?;
		self.status_message = format!("browsing {}", path.display());
		Ok(())
	}

	fn switch_project(&mut self, root: PathBuf) -> io::Result<()> {
		let _ = self.persist_session_state(true);
		let target = default_project_target(&root);
		let editor_target = initial_editor_target(&root, &[], &target);
		let mut project_tree = ProjectTree::new(root.clone());
		project_tree.expand_to(&target);
		project_tree.select_path(&target);

		self.terminal = PtyPane::spawn_shell(root.clone())?;
		self.nvim = PtyPane::spawn_nvim(editor_target, root.clone(), self.ui)?;
		self.editor_preview = uses_editor_preview(&target)
			.then(|| load_editor_preview(target.clone()))
			.transpose()?;
		self.project_tree = project_tree;
		self.codex_chat.switch_project(&root, &target);
		self.terminal_metrics = TerminalMetrics::new(self.terminal.process_id());
		self.refresh_terminal_metrics(true);
		self.focus = Focus::ProjectTree;
		self.status_message = format!("opened project {}", root.display());
		self.open_files.clear();
		self.active_file = None;
		self.mark_file_open(target);
		let _ = self.persist_session_state(false);
		Ok(())
	}

	fn submit_codex_prompt(&mut self) {
		let prompt = self.codex_chat.input.trim().to_string();
		if prompt.is_empty() {
			return;
		}

		if self.pending_codex_request.is_some() {
			self.status_message = "codex request already in flight".to_string();
			return;
		}

		if prompt == "/clear" {
			self.codex_chat.messages.clear();
			self.codex_chat.input.clear();
			self.codex_chat.push_assistant(
				"chat cleared. ask about the selected project and i will keep that path as the working context.",
			);
			self.status_message = "cleared codex chat".to_string();
			return;
		}

		let working_project = self.selected_working_project();
		let working_label = relative_to_root(&self.project_tree.root, &working_project);

		self.codex_chat.messages.push(ChatMessage {
			role: ChatRole::User,
			content: prompt.clone(),
			pending_request_id: None,
		});
		let transcript = self.codex_chat.api_transcript();
		let request_id = self.next_codex_request_id;
		self.next_codex_request_id += 1;
		self.pending_codex_request = Some(request_id);
		self.codex_chat.push_pending(request_id);
		self.codex_chat.input.clear();
		self.status_message = format!("sending codex request for {working_label}");

		let tx = self.codex_tx.clone();
		thread::spawn(move || {
			let reply = request_codex_reply(&working_project, &transcript);
			let _ = tx.send(CodexResult { request_id, reply });
		});
	}

	fn selected_working_project(&self) -> PathBuf {
		let Some(entry) = self.project_tree.visible.get(self.project_tree.selected) else {
			return self.project_tree.root.clone();
		};

		if entry.is_dir {
			entry.path.clone()
		} else {
			entry
				.path
				.parent()
				.unwrap_or(&self.project_tree.root)
				.to_path_buf()
		}
	}

	fn receive_codex_replies(&mut self) {
		while let Ok(result) = self.codex_rx.try_recv() {
			self.pending_codex_request = self.pending_codex_request.filter(|id| *id != result.request_id);
			match result.reply {
				Ok(reply) => {
					self.codex_chat.resolve_pending(result.request_id, reply);
					self.status_message = "codex replied".to_string();
				}
				Err(error) => {
					self.codex_chat.resolve_pending(
						result.request_id,
						format!("request failed: {error}"),
					);
					self.status_message = "codex request failed".to_string();
				}
			}
		}
	}

	fn restore_session_files(
		&mut self,
		files: Vec<PathBuf>,
		active_file: Option<PathBuf>,
		initial_editor_target: PathBuf,
	) -> io::Result<()> {
		self.open_files = files.clone();
		self.active_file = active_file.clone();
		if initial_editor_target.is_file()
			&& !self.open_files.iter().any(|path| path == &initial_editor_target)
		{
			self.open_files.push(initial_editor_target.clone());
		}

		for path in files.iter().filter(|path| !uses_editor_preview(path)) {
			if *path == initial_editor_target {
				continue;
			}
			self.nvim.open_file(path)?;
		}

		if let Some(active_file) = active_file {
			if uses_editor_preview(&active_file) {
				self.editor_preview = Some(load_editor_preview(active_file.clone())?);
			} else {
				self.editor_preview = None;
				if active_file != initial_editor_target {
					self.nvim.open_file(&active_file)?;
				}
			}
		} else if uses_editor_preview(&initial_editor_target) {
			self.editor_preview = Some(load_editor_preview(initial_editor_target.clone())?);
			self.active_file = Some(initial_editor_target);
		} else if self.active_file.is_none() {
			self.active_file = Some(initial_editor_target);
		}

		Ok(())
	}

	fn mark_file_open(&mut self, path: PathBuf) {
		if !self.open_files.iter().any(|existing| existing == &path) {
			self.open_files.push(path.clone());
		}
		self.active_file = Some(path);
	}

	fn persist_session_state(&mut self, sync_nvim: bool) -> io::Result<()> {
		let mut open_files = self.open_files.clone();
		let mut active_file = self.active_file.clone();

		if sync_nvim {
			if let Some(nvim_state) = self.snapshot_nvim_state()? {
				open_files.retain(|path| uses_editor_preview(path));
				for path in nvim_state.files {
					if !open_files.iter().any(|existing| existing == &path) {
						open_files.push(path);
					}
				}
				if self.editor_preview.is_none() {
					active_file = nvim_state.current.or(active_file);
				}
			}
		}

		if let Some(preview) = &self.editor_preview {
			if !open_files.iter().any(|path| path == preview.path()) {
				open_files.push(preview.path().to_path_buf());
			}
			active_file = Some(preview.path().to_path_buf());
		}

		open_files = sanitize_session_files(&self.project_tree.root, &open_files);
		active_file = sanitize_session_active_file(&self.project_tree.root, active_file.as_ref());
		self.open_files = open_files.clone();
		self.active_file = active_file.clone();

		save_saved_session(&SessionState {
			root: self.project_tree.root.clone(),
			open_files,
			active_file,
			accent_hex: Some(color_hex(self.ui.accent)),
		})
	}

	fn snapshot_nvim_state(&mut self) -> io::Result<Option<NvimBufferState>> {
		if self.nvim.is_exited() {
			return Ok(None);
		}

		let dump_path = nvim_snapshot_path()?;
		if let Some(parent) = dump_path.parent() {
			fs::create_dir_all(parent)?;
		}
		if dump_path.exists() {
			let _ = fs::remove_file(&dump_path);
		}

		self.nvim.dump_buffer_state(&dump_path)?;
		thread::sleep(Duration::from_millis(75));
		if !dump_path.exists() {
			return Ok(None);
		}

		let contents = fs::read_to_string(&dump_path)?;
		let _ = fs::remove_file(dump_path);
		Ok(parse_nvim_buffer_state(&contents))
	}

	fn refresh_terminal_metrics(&mut self, force: bool) {
		let now = Instant::now();
		if !force && now.duration_since(self.terminal_metrics.last_sample) < METRICS_SAMPLE_RATE {
			return;
		}
		self.terminal_metrics.last_sample = now;
		self.terminal_metrics.shell_pid = self.terminal.process_id();

		let Some(shell_pid) = self.terminal_metrics.shell_pid else {
			self.terminal_metrics.note = "terminal pid unavailable".to_string();
			self.terminal_metrics.cpu_percent = 0.0;
			self.terminal_metrics.mem_bytes = 0;
			self.terminal_metrics.active_process = "terminal".to_string();
			self.terminal_metrics.process_count = 0;
			push_history(&mut self.terminal_metrics.cpu_history, 0);
			return;
		};

		match sample_terminal_process_tree(shell_pid) {
			Ok(sample) => {
				self.terminal_metrics.active_process = sample.active_process;
				self.terminal_metrics.process_count = sample.process_count;
				self.terminal_metrics.cpu_percent = sample.cpu_percent;
				self.terminal_metrics.mem_bytes = sample.mem_bytes;
				self.terminal_metrics.gpu_percent = sample.gpu_percent;
				self.terminal_metrics.note = "tracking terminal process tree".to_string();
				push_history(
					&mut self.terminal_metrics.cpu_history,
					sample.cpu_percent.max(0.0).round() as u64,
				);
			}
			Err(error) => {
				self.terminal_metrics.note = format!("metrics unavailable: {error}");
				self.terminal_metrics.cpu_percent = 0.0;
				self.terminal_metrics.mem_bytes = 0;
				self.terminal_metrics.gpu_percent = None;
				self.terminal_metrics.active_process = "terminal".to_string();
				self.terminal_metrics.process_count = 0;
				push_history(&mut self.terminal_metrics.cpu_history, 0);
			}
		}
	}
}

impl Focus {
	fn label(self) -> &'static str {
		match self {
			Focus::Editor => "editor",
			Focus::Terminal => "terminal",
			Focus::ProjectTree => "project tree",
			Focus::Codex => "codex",
		}
	}

	fn next(self) -> Self {
		match self {
			Focus::Editor => Focus::Terminal,
			Focus::Terminal => Focus::ProjectTree,
			Focus::ProjectTree => Focus::Codex,
			Focus::Codex => Focus::Editor,
		}
	}
}

impl CodexChat {
	fn new(root: &Path, selected_file: &Path) -> Self {
		let working_project = selected_file.parent().unwrap_or(root);
		let working_label = relative_to_root(root, working_project);

		let mut chat = Self {
			messages: Vec::new(),
			input: String::new(),
		};
		chat.push_assistant(&format!(
			"minimal codex chat ready.\nworking project: {working_label}\nask something here and i will keep the selected project as context."
		));
		chat
	}

	fn push_assistant(&mut self, content: &str) {
		self.messages.push(ChatMessage {
			role: ChatRole::Assistant,
			content: content.to_string(),
			pending_request_id: None,
		});
	}

	fn push_pending(&mut self, request_id: u64) {
		self.messages.push(ChatMessage {
			role: ChatRole::Assistant,
			content: "thinking...".to_string(),
			pending_request_id: Some(request_id),
		});
	}

	fn switch_project(&mut self, root: &Path, selected_target: &Path) {
		let working_project = if selected_target.is_dir() {
			selected_target
		} else {
			selected_target.parent().unwrap_or(root)
		};
		let working_label = relative_to_root(root, working_project);
		self.push_assistant(&format!("switched project context to {working_label}."));
	}

	fn resolve_pending(&mut self, request_id: u64, content: String) {
		if let Some(message) = self
			.messages
			.iter_mut()
			.find(|message| message.pending_request_id == Some(request_id))
		{
			message.content = content;
			message.pending_request_id = None;
			return;
		}

		self.push_assistant(&content);
	}

	fn api_transcript(&self) -> String {
		let mut lines = Vec::new();
		for message in &self.messages {
			if message.pending_request_id.is_some() {
				continue;
			}

			let role = match message.role {
				ChatRole::User => "User",
				ChatRole::Assistant => "Assistant",
			};
			lines.push(format!("{role}: {}", message.content));
		}
		lines.join("\n\n")
	}

}

impl TerminalMetrics {
	fn new(shell_pid: Option<u32>) -> Self {
		Self {
			shell_pid,
			active_process: "terminal".to_string(),
			process_count: 0,
			cpu_percent: 0.0,
			mem_bytes: 0,
			gpu_percent: None,
			cpu_history: vec![0],
			last_sample: Instant::now() - METRICS_SAMPLE_RATE,
			total_memory_bytes: read_total_memory_bytes().unwrap_or(0),
			note: "tracking terminal process tree".to_string(),
		}
	}

	fn memory_percent(&self) -> f32 {
		if self.total_memory_bytes == 0 {
			return 0.0;
		}

		(self.mem_bytes as f64 / self.total_memory_bytes as f64 * 100.0) as f32
	}

	fn memory_label(&self) -> String {
		if self.total_memory_bytes == 0 {
			return format_bytes(self.mem_bytes);
		}

		format!(
			"{} / {}",
			format_bytes(self.mem_bytes),
			format_bytes(self.total_memory_bytes)
		)
	}
}

impl PtyPane {
	fn spawn_nvim(file_path: PathBuf, cwd: PathBuf, ui: UiTheme) -> io::Result<Self> {
		let mut cmd = CommandBuilder::new("nvim");
		cmd.arg("--clean");
		cmd.arg("-n");
		cmd.arg(file_path.as_os_str());
		cmd.arg("+set hidden mouse= number numberwidth=4 norelativenumber list listchars=tab:>-,space:.,trail:~ termguicolors background=dark");
		cmd.arg("+syntax on");
		cmd.arg(build_nvim_theme_command(ui));
		cmd.arg(
			"+lua function _G.veditor_close_buffer() local current = vim.api.nvim_get_current_buf(); local listed = vim.fn.getbufinfo({buflisted = 1}); if vim.bo.modified then vim.cmd('write') end; if #listed > 1 then vim.cmd('bnext') else vim.cmd('enew') end; if vim.api.nvim_buf_is_valid(current) then vim.cmd('bdelete ' .. current) end end",
		);
		cmd.arg(
			"+lua function _G.veditor_dump_buffers(path) local files = {} for _, buf in ipairs(vim.fn.getbufinfo({buflisted = 1})) do if buf.name ~= '' then table.insert(files, buf.name) end end local current = vim.api.nvim_buf_get_name(vim.api.nvim_get_current_buf()) if current == '' then current = vim.NIL end vim.fn.writefile({vim.json.encode({files = files, current = current})}, path) end",
		);
		cmd.arg("+command! VeditorClose lua _G.veditor_close_buffer()");
		cmd.arg(
			"+cnoreabbrev <expr> x getcmdtype() == ':' && getcmdline() ==# 'x' ? 'VeditorClose' : 'x'",
		);
		cmd.cwd(cwd);
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
		let command = format!("\x1b:drop {escaped}\r");
		self.writer.write_all(command.as_bytes())?;
		self.writer.flush()
	}

	fn apply_theme(&mut self, ui: UiTheme) -> io::Result<()> {
		let command = format!("\x1b:lua {}\r", nvim_theme_lua(ui));
		self.writer.write_all(command.as_bytes())?;
		self.writer.flush()
	}

	fn dump_buffer_state(&mut self, path: &Path) -> io::Result<()> {
		let escaped = escape_lua_string(path);
		let command = format!("\x1b:lua _G.veditor_dump_buffers('{escaped}')\r");
		self.writer.write_all(command.as_bytes())?;
		self.writer.flush()
	}

	fn is_exited(&mut self) -> bool {
		self.poll_exit().is_some()
	}

	fn process_id(&self) -> Option<u32> {
		self.child.process_id()
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
			search_query: String::new(),
			search_match: None,
			last_search_input: None,
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
		for path in sorted_project_entries(dir) {
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

	fn expand_to(&mut self, path: &Path) {
		for ancestor in path.ancestors() {
			if ancestor.starts_with(&self.root) && ancestor.is_dir() {
				self.expanded.insert(ancestor.to_path_buf());
			}
		}
	}

	fn search_active(&self) -> bool {
		!self.search_query.is_empty()
	}

	fn has_search_match(&self) -> bool {
		self.search_match.is_some()
	}

	fn clear_search(&mut self) {
		self.search_query.clear();
		self.search_match = None;
		self.last_search_input = None;
	}

	fn expire_search(&mut self) {
		if self
			.last_search_input
			.is_some_and(|timestamp| timestamp.elapsed() >= PROJECT_TREE_SEARCH_TIMEOUT)
		{
			self.clear_search();
		}
	}

	fn push_search_text(&mut self, text: &str) -> TreeSearchUpdate {
		self.expire_search();

		let sanitized = text
			.chars()
			.filter(|ch| *ch != '\n' && *ch != '\r')
			.collect::<String>();
		if sanitized.is_empty() {
			return TreeSearchUpdate::Unchanged;
		}

		self.search_query.push_str(&sanitized);
		self.last_search_input = Some(Instant::now());
		self.select_search_match()
	}

	fn backspace_search(&mut self) -> TreeSearchUpdate {
		if self.search_query.is_empty() {
			return TreeSearchUpdate::Unchanged;
		}

		self.search_query.pop();
		if self.search_query.is_empty() {
			self.clear_search();
			return TreeSearchUpdate::Cleared;
		}

		self.last_search_input = Some(Instant::now());
		self.select_search_match()
	}

	fn select_search_match(&mut self) -> TreeSearchUpdate {
		if self.search_query.is_empty() {
			self.search_match = None;
			return TreeSearchUpdate::Cleared;
		}

		let Some(path) = self.find_match(&self.search_query) else {
			self.search_match = None;
			return TreeSearchUpdate::NoMatch;
		};

		self.expand_to(&path);
		self.refresh(Some(path.clone()));
		self.search_match = Some(path.clone());
		TreeSearchUpdate::Matched(path)
	}

	fn find_match(&self, query: &str) -> Option<PathBuf> {
		let needle = query.trim().to_lowercase();
		if needle.is_empty() {
			return None;
		}

		let mut prefix = None;
		let mut contains = None;
		self.find_match_in_dir(&self.root, &needle, &mut prefix, &mut contains);
		prefix.or(contains)
	}

	fn find_match_in_dir(
		&self,
		dir: &Path,
		needle: &str,
		prefix: &mut Option<PathBuf>,
		contains: &mut Option<PathBuf>,
	) {
		for path in sorted_project_entries(dir) {
			let label = path
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or_default()
				.to_lowercase();
			let relative = relative_to_root(&self.root, &path).to_lowercase();

			if prefix.is_none() && (label.starts_with(needle) || relative.starts_with(needle)) {
				*prefix = Some(path.clone());
			}
			if contains.is_none() && (label.contains(needle) || relative.contains(needle)) {
				*contains = Some(path.clone());
			}
			if prefix.is_some() && contains.is_some() {
				return;
			}
			if path.is_dir() {
				self.find_match_in_dir(&path, needle, prefix, contains);
				if prefix.is_some() && contains.is_some() {
					return;
				}
			}
		}
	}
}

impl ProjectPicker {
	fn new(current_dir: PathBuf, selected_path: Option<PathBuf>) -> io::Result<Self> {
		let mut picker = Self {
			current_dir,
			entries: Vec::new(),
			selected: 0,
			search_query: String::new(),
			search_match: None,
			last_search_input: None,
		};
		picker.refresh(selected_path)?;
		Ok(picker)
	}

	fn refresh(&mut self, selected_path: Option<PathBuf>) -> io::Result<()> {
		self.entries = project_picker_entries(&self.current_dir)?;
		self.search_match = None;

		if self.entries.is_empty() {
			self.selected = 0;
			return Ok(());
		}

		if let Some(selected_path) = selected_path {
			if let Some(index) = self.entries.iter().position(|entry| entry.path == selected_path) {
				self.selected = index;
				return Ok(());
			}
		}

		if self.selected >= self.entries.len() {
			self.selected = self.entries.len() - 1;
		}

		Ok(())
	}

	fn set_dir(&mut self, current_dir: PathBuf, selected_path: Option<PathBuf>) -> io::Result<()> {
		self.current_dir = current_dir;
		self.refresh(selected_path)
	}

	fn move_selection(&mut self, delta: isize) {
		if self.entries.is_empty() {
			self.selected = 0;
			return;
		}

		let current = self.selected as isize + delta;
		let max = self.entries.len().saturating_sub(1) as isize;
		self.selected = current.clamp(0, max) as usize;
	}

	fn selected_path(&self) -> Option<PathBuf> {
		self.entries.get(self.selected).map(|entry| entry.path.clone())
	}

	fn search_active(&self) -> bool {
		!self.search_query.is_empty()
	}

	fn has_search_match(&self) -> bool {
		self.search_match.is_some()
	}

	fn clear_search(&mut self) {
		self.search_query.clear();
		self.search_match = None;
		self.last_search_input = None;
	}

	fn expire_search(&mut self) {
		if self
			.last_search_input
			.is_some_and(|timestamp| timestamp.elapsed() >= PROJECT_TREE_SEARCH_TIMEOUT)
		{
			self.clear_search();
		}
	}

	fn push_search_text(&mut self, text: &str) -> ProjectPickerSearchUpdate {
		self.expire_search();

		let sanitized = text
			.chars()
			.filter(|ch| *ch != '\n' && *ch != '\r')
			.collect::<String>();
		if sanitized.is_empty() {
			return ProjectPickerSearchUpdate::Unchanged;
		}

		self.search_query.push_str(&sanitized);
		self.last_search_input = Some(Instant::now());
		self.select_search_match()
	}

	fn backspace_search(&mut self) -> ProjectPickerSearchUpdate {
		if self.search_query.is_empty() {
			return ProjectPickerSearchUpdate::Unchanged;
		}

		self.search_query.pop();
		if self.search_query.is_empty() {
			self.clear_search();
			return ProjectPickerSearchUpdate::Cleared;
		}

		self.last_search_input = Some(Instant::now());
		self.select_search_match()
	}

	fn select_search_match(&mut self) -> ProjectPickerSearchUpdate {
		if self.search_query.is_empty() {
			self.search_match = None;
			return ProjectPickerSearchUpdate::Cleared;
		}

		let needle = self.search_query.trim().to_lowercase();
		if needle.is_empty() {
			self.clear_search();
			return ProjectPickerSearchUpdate::Cleared;
		}

		let Some(path) = find_project_picker_match(&self.current_dir, &needle) else {
			self.search_match = None;
			return ProjectPickerSearchUpdate::NoMatch;
		};

		if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
			self.selected = index;
		} else {
			let Some(parent) = path.parent() else {
				self.search_match = None;
				return ProjectPickerSearchUpdate::NoMatch;
			};
			self.current_dir = parent.to_path_buf();
			if let Err(_) = self.refresh(Some(path.clone())) {
				self.search_match = None;
				return ProjectPickerSearchUpdate::NoMatch;
			}
		}

		self.search_match = Some(path.clone());
		ProjectPickerSearchUpdate::Matched(path)
	}
}

impl ImagePreview {
	fn load(path: PathBuf) -> io::Result<Self> {
		let image = ImageReader::open(&path)
			.map_err(io_error)?
			.decode()
			.map_err(io_error)?;
		Ok(Self {
			path,
			image,
			cache: None,
		})
	}

	fn lines(&mut self, width: u16, height: u16, ui: UiTheme) -> &[Line<'static>] {
		let refresh = self
			.cache
			.as_ref()
			.is_none_or(|cache| cache.width != width || cache.height != height || cache.panel != ui.panel);
		if refresh {
			self.cache = Some(ImagePreviewCache {
				width,
				height,
				panel: ui.panel,
				lines: build_image_preview_lines(&self.image, width, height, ui),
			});
		}

		self.cache
			.as_ref()
			.map(|cache| cache.lines.as_slice())
			.unwrap_or(&[])
	}
}

impl DocumentPreview {
	fn load(path: PathBuf) -> io::Result<Self> {
		if is_pdf_path(&path) {
			Self::load_pdf(path)
		} else if is_notebook_path(&path) {
			Self::load_notebook(path)
		} else {
			Err(io_error("unsupported document preview"))
		}
	}

	fn load_pdf(path: PathBuf) -> io::Result<Self> {
		let text = pdf_extract::extract_text(&path).map_err(io_error)?;
		let (lines, truncated) = preview_lines_from_iter(text.lines().map(str::to_string));
		let summary = if truncated {
			format!("pdf text preview  showing first {DOCUMENT_PREVIEW_MAX_LINES} lines")
		} else {
			"pdf text preview".to_string()
		};
		Ok(Self {
			path,
			kind: "pdf",
			summary,
			lines,
		})
	}

	fn load_notebook(path: PathBuf) -> io::Result<Self> {
		let contents = fs::read_to_string(&path)?;
		let value: serde_json::Value = serde_json::from_str(&contents).map_err(io_error)?;
		let cells = value
			.get("cells")
			.and_then(serde_json::Value::as_array)
			.ok_or_else(|| io_error("notebook missing cells"))?;

		let mut rendered = Vec::new();
		for (index, cell) in cells.iter().enumerate() {
			if index > 0 {
				rendered.push(String::new());
			}

			let cell_type = cell
				.get("cell_type")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("cell");
			rendered.push(format!("[{cell_type} {}]", index + 1));

			for line in notebook_lines(cell.get("source")) {
				rendered.push(line);
			}

			for line in notebook_output_lines(cell.get("outputs")) {
				rendered.push(line);
			}
		}

		if rendered.is_empty() {
			rendered.push("notebook is empty".to_string());
		}

		let (lines, truncated) = preview_lines_from_iter(rendered.into_iter());
		let summary = if truncated {
			format!("notebook preview  showing first {DOCUMENT_PREVIEW_MAX_LINES} lines")
		} else {
			"notebook preview".to_string()
		};
		Ok(Self {
			path,
			kind: "ipynb",
			summary,
			lines,
		})
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
		.constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
		.areas(right);

	render_pty_pane(frame, terminal_area, app.ui, app.focus == Focus::Terminal, &mut app.terminal);
	performance_block(frame, performance_area, app);
	project_tree(frame, left_bottom, app);
	render_editor(frame, editor_area, app);
	codex_block(frame, codex_area, app);
}

fn render_editor(frame: &mut Frame, area: Rect, app: &mut App) {
	if let Some(preview) = &mut app.editor_preview {
		match preview {
			EditorPreview::Image(preview) => {
				render_image_preview(frame, area, app.ui, app.focus == Focus::Editor, preview);
			}
			EditorPreview::Document(preview) => {
				render_document_preview(frame, area, app.ui, app.focus == Focus::Editor, preview);
			}
		}
	} else {
		render_pty_pane(frame, area, app.ui, app.focus == Focus::Editor, &mut app.nvim);
	}
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

fn render_image_preview(
	frame: &mut Frame,
	area: Rect,
	ui: UiTheme,
	focused: bool,
	preview: &mut ImagePreview,
) {
	let title = format!(
		" image  {} ",
		preview
			.path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("preview")
	);
	let block = panel(&title, ui, focused);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	let [meta_area, preview_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(2), Constraint::Min(1)])
		.areas(inner);

	let (width, height) = preview.image.dimensions();
	let meta = Paragraph::new(vec![
		Line::styled(
			preview.path.display().to_string(),
			Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
		),
		Line::styled(
			format!("{width}x{height}  static preview"),
			Style::default().fg(ui.muted),
		),
	])
	.style(Style::default().bg(ui.panel));
	frame.render_widget(meta, meta_area);

	let widget = Paragraph::new(preview.lines(preview_area.width, preview_area.height, ui).to_vec())
		.style(Style::default().bg(ui.panel));
	frame.render_widget(widget, preview_area);
}

fn render_document_preview(
	frame: &mut Frame,
	area: Rect,
	ui: UiTheme,
	focused: bool,
	preview: &DocumentPreview,
) {
	let title = format!(
		" {}  {} ",
		preview.kind,
		preview
			.path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("preview")
	);
	let block = panel(&title, ui, focused);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	let [meta_area, preview_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(2), Constraint::Min(1)])
		.areas(inner);

	let meta = Paragraph::new(vec![
		Line::styled(
			preview.path.display().to_string(),
			Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
		),
		Line::styled(preview.summary.clone(), Style::default().fg(ui.muted)),
	])
	.style(Style::default().bg(ui.panel));
	frame.render_widget(meta, meta_area);

	let widget = Paragraph::new(preview.lines.clone())
		.style(Style::default().bg(ui.panel).fg(ui.text))
		.wrap(Wrap { trim: false });
	frame.render_widget(widget, preview_area);
}

fn build_image_preview_lines(
	image: &DynamicImage,
	width: u16,
	height: u16,
	ui: UiTheme,
) -> Vec<Line<'static>> {
	if width == 0 || height == 0 {
		return Vec::new();
	}

	let target_width = width as u32;
	let target_height = height as u32 * 2;
	let resized = image
		.resize(target_width.max(1), target_height.max(1), FilterType::Lanczos3)
		.to_rgba8();
	let image_width = resized.width() as u16;
	let image_height = resized.height() as u16;
	let rows = image_height.div_ceil(2);
	let x_pad = width.saturating_sub(image_width) / 2;
	let y_pad = height.saturating_sub(rows) / 2;
	let panel_rgb = color_to_rgb(ui.panel);

	let mut lines = Vec::with_capacity(height as usize);
	for row in 0..height {
		if row < y_pad || row >= y_pad.saturating_add(rows) {
			lines.push(Line::from(Span::styled(
				" ".repeat(width as usize),
				Style::default().bg(ui.panel),
			)));
			continue;
		}

		let image_row = row - y_pad;
		let mut spans = Vec::new();
		if x_pad > 0 {
			spans.push(Span::styled(
				" ".repeat(x_pad as usize),
				Style::default().bg(ui.panel),
			));
		}

		for col in 0..image_width {
			let top = blend_rgba_to_rgb(resized.get_pixel(col as u32, image_row as u32 * 2).0, panel_rgb);
			let bottom_y = image_row as u32 * 2 + 1;
			let bottom = if bottom_y < image_height as u32 {
				blend_rgba_to_rgb(resized.get_pixel(col as u32, bottom_y).0, panel_rgb)
			} else {
				panel_rgb
			};

			spans.push(Span::styled(
				"▀",
				Style::default()
					.fg(rgb_to_color(top))
					.bg(rgb_to_color(bottom)),
			));
		}

		let right_pad = width.saturating_sub(x_pad).saturating_sub(image_width);
		if right_pad > 0 {
			spans.push(Span::styled(
				" ".repeat(right_pad as usize),
				Style::default().bg(ui.panel),
			));
		}

		lines.push(Line::from(spans));
	}

	lines
}

fn project_tree(frame: &mut Frame, area: Rect, app: &App) {
	let title = if let Some(picker) = &app.project_picker {
		if picker.search_active() {
			format!(
				" open project  {}  search: {} ",
				picker.current_dir.display(),
				picker.search_query
			)
		} else {
			format!(" open project  {} ", picker.current_dir.display())
		}
	} else if app.project_tree.search_active() {
		format!("project tree  search: {}", app.project_tree.search_query)
	} else {
		"project tree".to_string()
	};

	let block = panel(&title, app.ui, app.focus == Focus::ProjectTree);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	let (list_area, command_area) = if app.command_prompt.is_some() {
		let [list_area, command_area] = Layout::default()
			.direction(Direction::Vertical)
			.constraints([Constraint::Min(1), Constraint::Length(4)])
			.areas(inner);
		(list_area, Some(command_area))
	} else {
		(inner, None)
	};

	if let Some(picker) = &app.project_picker {
		let items = picker
			.entries
			.iter()
			.map(|entry| {
				ListItem::new(Line::from(vec![
					Span::styled("▸", Style::default().fg(app.ui.accent)),
					Span::raw("  "),
					Span::styled("󰉋", Style::default().fg(app.ui.accent)),
					Span::raw("  "),
					Span::styled(entry.label.clone(), Style::default().fg(app.ui.text)),
				]))
			})
			.collect::<Vec<_>>();

		let list = List::new(items)
			.highlight_style(
				Style::default()
					.fg(app.ui.bg)
					.bg(app.ui.accent)
					.add_modifier(Modifier::BOLD),
			)
			.highlight_symbol(" ");
		let mut state = ListState::default();
		if !picker.entries.is_empty() {
			state.select(Some(picker.selected));
		}
		frame.render_stateful_widget(list, list_area, &mut state);
	} else {
		let items = app
			.project_tree
			.visible
			.iter()
			.enumerate()
			.map(|(index, entry)| {
				let label = entry
					.path
					.file_name()
					.and_then(|name| name.to_str())
					.unwrap_or_else(|| entry.path.to_str().unwrap_or_default())
					.to_string();
				let indent = "   ".repeat(entry.depth);
				let is_selected = index == app.project_tree.selected;
				let is_startup = entry.path == app.project_tree.root.join(STARTUP_FILE);
				let row_style = if is_selected {
					Style::default()
						.fg(app.ui.bg)
						.bg(app.ui.accent)
						.add_modifier(Modifier::BOLD)
				} else if is_startup {
					Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)
				} else {
					Style::default().fg(app.ui.text)
				};
				let accent_style = if is_selected {
					row_style
				} else {
					row_style.fg(app.ui.accent)
				};
				let toggle = if entry.is_dir {
					if app.project_tree.expanded.contains(&entry.path) {
						"▾"
					} else {
						"▸"
					}
				} else {
					" "
				};
				let icon = if entry.is_dir { "󰉋" } else { "󰈔" };

				ListItem::new(Line::from(vec![
					Span::styled(indent, row_style),
					Span::styled(toggle, accent_style),
					Span::styled("  ", row_style),
					Span::styled(icon, accent_style),
					Span::styled("  ", row_style),
					Span::styled(label, row_style),
				]))
			})
			.collect::<Vec<_>>();

		let tree = List::new(items)
			.highlight_style(
				Style::default()
					.fg(app.ui.bg)
					.bg(app.ui.accent)
					.add_modifier(Modifier::BOLD),
			)
			.highlight_symbol(" ");
		let mut state = ListState::default();
		if !app.project_tree.visible.is_empty() {
			state.select(Some(app.project_tree.selected));
		}
		frame.render_stateful_widget(tree, list_area, &mut state);
	}

	if let (Some(command), Some(command_area)) = (&app.command_prompt, command_area) {
		let command_block = Block::bordered()
			.border_style(Style::default().fg(app.ui.border))
			.style(Style::default().bg(app.ui.panel_alt))
			.title(Line::styled(
				" command  enter run  esc cancel ",
				Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD),
			));
		let command_inner = command_block.inner(command_area);
		let command_widget = Paragraph::new(Line::from(vec![
			Span::styled(":", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
			Span::styled(
				command
					.strip_prefix(':')
					.map(str::to_string)
					.unwrap_or_else(|| command.clone()),
				Style::default().fg(app.ui.text),
			),
		]))
		.block(command_block)
		.wrap(Wrap { trim: false });
		frame.render_widget(command_widget, command_area);

		let cursor_x = command_inner
			.x
			.saturating_add(command.chars().count() as u16);
		let cursor_y = command_inner.y;
		if cursor_x < command_inner.right()
			&& cursor_y < command_inner.bottom()
			&& app.focus == Focus::ProjectTree
		{
			frame.set_cursor_position((cursor_x, cursor_y));
		}
	}
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
	let focus = false;
	let metrics = &app.terminal_metrics;
	let gpu_percent = metrics.gpu_percent.unwrap_or(0.0).clamp(0.0, 100.0) as u16;
	let cpu_percent = metrics.cpu_percent.clamp(0.0, 100.0) as u16;
	let mem_percent = metrics.memory_percent().clamp(0.0, 100.0) as u16;

	frame.render_widget(
		Gauge::default()
			.block(panel("gpu / terminal", app.ui, focus))
			.gauge_style(gauge_style)
			.label(
				metrics
					.gpu_percent
					.map(|value| format!("{value:.1}%"))
					.unwrap_or_else(|| "unavailable".to_string()),
			)
			.percent(gpu_percent),
		gpu_area,
	);
	frame.render_widget(
		Gauge::default()
			.block(panel("cpu / terminal", app.ui, false))
			.gauge_style(gauge_style)
			.label(format!("{:.1}% {}", metrics.cpu_percent, metrics.active_process))
			.percent(cpu_percent),
		cpu_area,
	);
	frame.render_widget(
		Gauge::default()
			.block(panel("mem / terminal", app.ui, false))
			.gauge_style(gauge_style)
			.label(metrics.memory_label())
			.percent(mem_percent),
		mem_area,
	);

	let [status_area, spark_area] = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Length(3), Constraint::Min(3)])
		.areas(graph_area);

	let detail = Paragraph::new(vec![
		Line::styled(
			format!(
				"pid {}  processes {}",
				metrics.shell_pid.unwrap_or(0),
				metrics.process_count
			),
			Style::default().fg(app.ui.text),
		),
		Line::styled(metrics.note.clone(), Style::default().fg(app.ui.muted)),
	])
	.block(panel("terminal job", app.ui, false))
	.wrap(Wrap { trim: true });
	frame.render_widget(detail, status_area);

	let spark = Sparkline::default()
		.block(panel("cpu / history", app.ui, false))
		.data(&metrics.cpu_history)
		.style(Style::default().fg(app.ui.accent))
		.max(metrics.cpu_history.iter().copied().max().unwrap_or(100).max(100));

	frame.render_widget(spark, spark_area);
}

fn codex_block(frame: &mut Frame, area: Rect, app: &App) {
	let block = panel("codex", app.ui, app.focus == Focus::Codex);
	let inner = block.inner(area);
	frame.render_widget(block, area);

	if inner.width == 0 || inner.height == 0 {
		return;
	}

	let (history_area, input_area) = if inner.height >= 6 {
		let [history_area, input_area] = Layout::default()
			.direction(Direction::Vertical)
			.constraints([Constraint::Min(1), Constraint::Length(3)])
			.areas(inner);
		(Some(history_area), input_area)
	} else {
		(None, inner)
	};

	if let Some(history_area) = history_area {
		let history = Paragraph::new(codex_history_lines(app, history_area.width))
			.style(Style::default().bg(app.ui.panel).fg(app.ui.text))
			.wrap(Wrap { trim: false });
		frame.render_widget(history, history_area);
	}

	let input_lines = if let Some(prompt) = &app.create_prompt {
		vec![
			Line::from(vec![
				Span::styled(
					match prompt.kind {
						CreateKind::File => "create file",
						CreateKind::Directory => "create directory",
					},
					Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD),
				),
				Span::raw("  "),
				Span::styled(prompt.input.clone(), Style::default().fg(app.ui.text)),
			]),
			Line::styled("enter confirm  esc cancel", Style::default().fg(app.ui.muted)),
		]
	} else {
		vec![
			Line::from(vec![
				Span::styled("you", Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD)),
				Span::raw("  "),
				Span::styled(app.codex_chat.input.clone(), Style::default().fg(app.ui.text)),
			]),
			Line::styled("enter send", Style::default().fg(app.ui.muted)),
		]
	};

	let input_block = Block::bordered()
		.border_style(Style::default().fg(app.ui.border))
		.style(Style::default().bg(app.ui.panel_alt))
		.title(Line::styled(
			" chat ",
			Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD),
		));
	let input_inner = input_block.inner(input_area);
	let input = Paragraph::new(input_lines)
		.block(input_block)
		.wrap(Wrap { trim: false });
	frame.render_widget(input, input_area);

	let (cursor_prefix, cursor_len) = if let Some(prompt) = &app.create_prompt {
		(
			match prompt.kind {
				CreateKind::File => "create file  ",
				CreateKind::Directory => "create directory  ",
			},
			prompt.input.chars().count(),
		)
	} else {
		("you  ", app.codex_chat.input.chars().count())
	};
	let cursor_x = input_inner
		.x
		.saturating_add((cursor_prefix.chars().count() + cursor_len) as u16);
	let cursor_y = input_inner.y;

	if cursor_x < input_inner.right() && cursor_y < input_inner.bottom() {
		if app.create_prompt.is_some() || app.focus == Focus::Codex {
			frame.set_cursor_position((cursor_x, cursor_y));
		}
	}
}

fn codex_history_lines(app: &App, width: u16) -> Vec<Line<'static>> {
	if width == 0 {
		return Vec::new();
	}

	let mut lines = Vec::new();
	for message in &app.codex_chat.messages {
		let label = match message.role {
			ChatRole::User => "you",
			ChatRole::Assistant => "codex",
		};
		let content = message.content.replace('\n', " ");
		lines.push(Line::from(vec![
			Span::styled(
				format!("{label}  "),
				Style::default().fg(app.ui.accent).add_modifier(Modifier::BOLD),
			),
			Span::styled(content, Style::default().fg(app.ui.text)),
		]));
	}

	let max_lines = lines.len().min(8);
	let start = lines.len().saturating_sub(max_lines);
	lines[start..].to_vec()
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
	let ui = ui_theme(ACCENT_COLOR);
	match idx {
		0..=15 => ui.ansi[idx as usize],
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

fn escape_lua_string(path: &Path) -> String {
	path.display()
		.to_string()
		.replace('\\', "\\\\")
		.replace('\'', "\\'")
}

fn is_project_open_shortcut(key: KeyEvent) -> bool {
	let command_mod = key.modifiers.contains(KeyModifiers::SUPER)
		|| key.modifiers.contains(KeyModifiers::CONTROL);
	command_mod && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
}

fn is_command_prompt_start(key: KeyEvent) -> bool {
	!key
		.modifiers
		.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
		&& matches!(key.code, KeyCode::Char(':'))
}

fn default_project_target(root: &Path) -> PathBuf {
	let startup = root.join(STARTUP_FILE);
	if startup.is_file() {
		return startup;
	}

	find_first_project_file(root).unwrap_or_else(|| root.to_path_buf())
}

fn resolve_startup_target(root: &Path, requested: &Path) -> PathBuf {
	let requested = if requested.is_absolute() {
		requested.to_path_buf()
	} else {
		root.join(requested)
	};

	if requested.exists() {
		requested
	} else {
		default_project_target(root)
	}
}

fn absolutize_path(base: &Path, path: &Path) -> PathBuf {
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		base.join(path)
	}
}

fn initial_editor_target(root: &Path, files: &[PathBuf], fallback: &Path) -> PathBuf {
	let candidate = files
		.iter()
		.find(|path| !uses_editor_preview(path))
		.cloned()
		.unwrap_or_else(|| resolve_startup_target(root, fallback));

	if uses_editor_preview(&candidate) {
		let default_target = default_project_target(root);
		if uses_editor_preview(&default_target) {
			root.to_path_buf()
		} else {
			default_target
		}
	} else {
		candidate
	}
}

fn sanitize_session_files(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
	let mut unique = Vec::new();
	for path in files {
		let candidate = if path.is_absolute() {
			path.clone()
		} else {
			root.join(path)
		};
		let Ok(canonical) = fs::canonicalize(candidate) else {
			continue;
		};
		if canonical.is_file() && !unique.iter().any(|existing| existing == &canonical) {
			unique.push(canonical);
		}
	}
	unique
}

fn sanitize_session_active_file(root: &Path, path: Option<&PathBuf>) -> Option<PathBuf> {
	let candidate = path?;
	let candidate = if candidate.is_absolute() {
		candidate.clone()
	} else {
		root.join(candidate)
	};
	let canonical = fs::canonicalize(candidate).ok()?;
	canonical.is_file().then_some(canonical)
}

fn load_saved_session() -> Option<SessionState> {
	let path = session_state_path()?;
	let contents = fs::read_to_string(path).ok()?;
	let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
	let root = value.get("root")?.as_str()?;
	let root = fs::canonicalize(root).ok()?;
	if !root.is_dir() {
		return None;
	}

	let open_files = value
		.get("open_files")
		.and_then(serde_json::Value::as_array)
		.map(|files| {
			files
				.iter()
				.filter_map(serde_json::Value::as_str)
				.map(PathBuf::from)
				.collect::<Vec<_>>()
		})
		.unwrap_or_default();
	let active_file = value
		.get("active_file")
		.and_then(serde_json::Value::as_str)
		.map(PathBuf::from);
	let accent_hex = value
		.get("accent_hex")
		.and_then(serde_json::Value::as_str)
		.map(str::to_string);

	Some(SessionState {
		root,
		open_files,
		active_file,
		accent_hex,
	})
}

fn save_saved_session(session: &SessionState) -> io::Result<()> {
	let Some(path) = session_state_path() else {
		return Ok(());
	};
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}

	let payload = serde_json::json!({
		"root": session.root.display().to_string(),
		"open_files": session.open_files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
		"active_file": session.active_file.as_ref().map(|path| path.display().to_string()),
		"accent_hex": session.accent_hex,
	});
	let contents = serde_json::to_string_pretty(&payload).map_err(io_error)?;
	fs::write(path, contents)
}

fn parse_nvim_buffer_state(contents: &str) -> Option<NvimBufferState> {
	let value: serde_json::Value = serde_json::from_str(contents).ok()?;
	let files = value
		.get("files")
		.and_then(serde_json::Value::as_array)
		.map(|files| {
			files
				.iter()
				.filter_map(serde_json::Value::as_str)
				.map(PathBuf::from)
				.collect::<Vec<_>>()
		})
		.unwrap_or_default();
	let current = value
		.get("current")
		.and_then(serde_json::Value::as_str)
		.filter(|value| !value.is_empty())
		.map(PathBuf::from);

	Some(NvimBufferState { files, current })
}

fn session_state_path() -> Option<PathBuf> {
	let home = env::var_os("HOME")?;
	Some(PathBuf::from(home).join(".veditor").join("session.json"))
}

fn nvim_snapshot_path() -> io::Result<PathBuf> {
	let Some(path) = session_state_path() else {
		return Err(io_error("home directory unavailable"));
	};
	let parent = path
		.parent()
		.ok_or_else(|| io_error("invalid session path"))?;
	Ok(parent.join("nvim-buffers.json"))
}

fn find_first_project_file(dir: &Path) -> Option<PathBuf> {
	let read_dir = fs::read_dir(dir).ok()?;
	let mut entries = read_dir.filter_map(Result::ok).collect::<Vec<_>>();
	entries.sort_by_key(|entry| entry.path());

	for entry in entries {
		let path = entry.path();
		let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
		if name == ".git" || name == "target" || name.ends_with(".swp") {
			continue;
		}

		if path.is_file() {
			return Some(path);
		}

		if path.is_dir() {
			if let Some(child) = find_first_project_file(&path) {
				return Some(child);
			}
		}
	}

	None
}

fn relative_to_root(root: &Path, path: &Path) -> String {
	match path.strip_prefix(root) {
		Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
		Ok(relative) => relative.display().to_string(),
		Err(_) => path.display().to_string(),
	}
}

fn project_picker_entries(root: &Path) -> io::Result<Vec<ProjectPickerEntry>> {
	let read_dir = fs::read_dir(root)?;
	let mut dirs = read_dir
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| path.is_dir())
		.filter(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.map(|name| !name.starts_with('.') && name != "target")
				.unwrap_or(true)
		})
		.collect::<Vec<_>>();
	dirs.sort();

	Ok(dirs
		.into_iter()
		.map(|path| ProjectPickerEntry {
			label: path
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or_else(|| path.to_str().unwrap_or_default())
				.to_string(),
			path,
		})
		.collect())
}

fn find_project_picker_match(root: &Path, needle: &str) -> Option<PathBuf> {
	let mut prefix = None;
	let mut contains = None;
	find_project_picker_match_in_dir(root, root, needle, &mut prefix, &mut contains);
	prefix.or(contains)
}

fn find_project_picker_match_in_dir(
	root: &Path,
	dir: &Path,
	needle: &str,
	prefix: &mut Option<PathBuf>,
	contains: &mut Option<PathBuf>,
) {
	for path in sorted_project_entries(dir) {
		if !path.is_dir() {
			continue;
		}

		let label = path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or_default()
			.to_lowercase();
		let relative = relative_to_root(root, &path).to_lowercase();

		if prefix.is_none() && (label.starts_with(needle) || relative.starts_with(needle)) {
			*prefix = Some(path.clone());
		}
		if contains.is_none() && (label.contains(needle) || relative.contains(needle)) {
			*contains = Some(path.clone());
		}
		if prefix.is_some() && contains.is_some() {
			return;
		}

		find_project_picker_match_in_dir(root, &path, needle, prefix, contains);
		if prefix.is_some() && contains.is_some() {
			return;
		}
	}
}

fn default_project_root(root: &Path) -> PathBuf {
	let startup = root.join(STARTUP_FILE);
	if startup.is_file() {
		return root.to_path_buf();
	}

	find_first_project_root(root).unwrap_or_else(|| root.to_path_buf())
}

fn sorted_project_entries(dir: &Path) -> Vec<PathBuf> {
	let Ok(read_dir) = fs::read_dir(dir) else {
		return Vec::new();
	};

	let mut paths = read_dir
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.collect::<Vec<_>>();
	paths.sort();
	paths
		.into_iter()
		.filter(|path| {
			let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
			let is_dir = path.is_dir();
			!((is_dir && name.starts_with('.')) || name == "target" || name.ends_with(".swp"))
		})
		.collect()
}

fn find_first_project_root(dir: &Path) -> Option<PathBuf> {
	let entries = sorted_project_entries(dir);
	if entries.iter().any(|path| path.is_file()) {
		return Some(dir.to_path_buf());
	}

	for path in entries {
		if path.is_dir() {
			if let Some(child) = find_first_project_root(&path) {
				return Some(child);
			}
		}
	}

	None
}

fn request_codex_reply(working_project: &Path, transcript: &str) -> Result<String, String> {
	let output_path = codex_last_message_path();
	let prompt = format!(
		"You are Codex embedded inside a terminal editor. The current working project is '{}'. Answer directly and concisely. When relevant, treat that path as the project root.\n\n{}",
		working_project.display(),
		transcript
	);

	let mut child = Command::new("codex")
		.args([
			"exec",
			"--cd",
			working_project.to_str().unwrap_or("."),
			"--skip-git-repo-check",
			"--sandbox",
			"read-only",
			"--color",
			"never",
			"--output-last-message",
		])
		.arg(&output_path)
		.arg("-")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|error| {
			if error.kind() == io::ErrorKind::NotFound {
				"codex cli not found in PATH".to_string()
			} else {
				error.to_string()
			}
		})?;

	if let Some(mut stdin) = child.stdin.take() {
		stdin
			.write_all(prompt.as_bytes())
			.and_then(|_| stdin.flush())
			.map_err(|error| error.to_string())?;
	}

	let output = child.wait_with_output().map_err(|error| error.to_string())?;
	let last_message = fs::read_to_string(&output_path)
		.ok()
		.map(|text| text.trim().to_string());
	let _ = fs::remove_file(&output_path);

	if output.status.success() {
		if let Some(message) = last_message
			.as_ref()
			.filter(|message| !message.is_empty())
			.cloned()
		{
			return Ok(message);
		}
	}

	let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
	let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
	if let Some(message) = last_message
		.as_ref()
		.filter(|message| !message.is_empty())
		.cloned()
	{
		return Ok(message);
	}
	if !stderr.is_empty() {
		return Err(stderr);
	}
	if !stdout.is_empty() {
		return Err(stdout);
	}
	Err(format!("codex exec failed with status {}", output.status))
}

fn sample_terminal_process_tree(root_pid: u32) -> io::Result<TerminalProcessSample> {
	let output = Command::new("ps")
		.args(["-axo", "pid=,ppid=,pcpu=,rss=,comm="])
		.output()?;

	if !output.status.success() {
		return Err(io_error("ps failed"));
	}

	let text = String::from_utf8_lossy(&output.stdout);
	let mut snapshots = HashMap::new();
	let mut children: HashMap<u32, Vec<u32>> = HashMap::new();

	for line in text.lines() {
		let mut parts = line.split_whitespace();
		let Some(pid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
			continue;
		};
		let Some(ppid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
			continue;
		};
		let Some(cpu_percent) = parts.next().and_then(|value| value.parse::<f32>().ok()) else {
			continue;
		};
		let Some(rss_kib) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
			continue;
		};
		let command = parts.collect::<Vec<_>>().join(" ");
		if command.is_empty() {
			continue;
		}

		snapshots.insert(
			pid,
			ProcessSnapshot {
				pid,
				cpu_percent,
				rss_kib,
				command,
			},
		);
		children.entry(ppid).or_default().push(pid);
	}

	let root = snapshots
		.get(&root_pid)
		.ok_or_else(|| io_error(format!("terminal pid {root_pid} not found")))?;

	let mut stack = vec![root_pid];
	let mut cpu_percent = 0.0;
	let mut mem_bytes = 0_u64;
	let mut process_count = 0_usize;
	let mut busiest = root;

	while let Some(pid) = stack.pop() {
		let Some(process) = snapshots.get(&pid) else {
			continue;
		};

		process_count += 1;
		cpu_percent += process.cpu_percent;
		mem_bytes += process.rss_kib.saturating_mul(1024);

		if process.cpu_percent > busiest.cpu_percent
			|| (busiest.pid == root_pid && process.pid != root_pid && process.cpu_percent >= busiest.cpu_percent)
		{
			busiest = process;
		}

		if let Some(descendants) = children.get(&pid) {
			stack.extend(descendants.iter().copied());
		}
	}

	Ok(TerminalProcessSample {
		active_process: process_label(&busiest.command),
		process_count,
		cpu_percent,
		mem_bytes,
		gpu_percent: None,
	})
}

fn process_label(command: &str) -> String {
	Path::new(command)
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or(command)
		.to_string()
}

fn read_total_memory_bytes() -> io::Result<u64> {
	let output = Command::new("sysctl").args(["-n", "hw.memsize"]).output()?;
	if !output.status.success() {
		return Err(io_error("sysctl failed"));
	}

	let value = String::from_utf8_lossy(&output.stdout);
	value
		.trim()
		.parse::<u64>()
		.map_err(|error| io_error(format!("invalid hw.memsize: {error}")))
}

fn push_history(history: &mut Vec<u64>, value: u64) {
	if history.len() >= HISTORY_POINTS {
		history.remove(0);
	}
	history.push(value);
}

fn format_bytes(bytes: u64) -> String {
	const KIB: f64 = 1024.0;
	const MIB: f64 = KIB * 1024.0;
	const GIB: f64 = MIB * 1024.0;

	let bytes = bytes as f64;
	if bytes >= GIB {
		format!("{:.1} GiB", bytes / GIB)
	} else if bytes >= MIB {
		format!("{:.1} MiB", bytes / MIB)
	} else if bytes >= KIB {
		format!("{:.0} KiB", bytes / KIB)
	} else {
		format!("{bytes:.0} B")
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

fn ui_theme(accent_hex: &str) -> UiTheme {
	let accent = color_to_rgb(parse_hex_color(accent_hex).unwrap_or(Color::Rgb(30, 144, 255)));
	let bg = mix(rgb(3, 4, 8), accent, 0.10);
	let panel = mix(rgb(7, 10, 18), accent, 0.16);
	let panel_alt = mix(rgb(12, 16, 28), accent, 0.24);
	let accent_soft = mix(accent, rgb(255, 255, 255), 0.24);
	let accent_dim = mix(accent, bg, 0.38);
	let text = mix(rgb(245, 247, 250), accent, 0.18);
	let muted = mix(text, panel_alt, 0.44);
	let border = mix(panel_alt, accent, 0.55);
	let selection = mix(bg, accent, 0.42);
	let special = mix(accent_soft, text, 0.28);
	let type_color = mix(accent, text, 0.38);
	let ansi = [
		rgb_to_color(bg),
		rgb_to_color(accent_dim),
		rgb_to_color(mix(accent, text, 0.10)),
		rgb_to_color(accent),
		rgb_to_color(accent_soft),
		rgb_to_color(type_color),
		rgb_to_color(mix(text, accent, 0.12)),
		rgb_to_color(text),
		rgb_to_color(panel_alt),
		rgb_to_color(mix(accent_dim, text, 0.25)),
		rgb_to_color(mix(accent, text, 0.25)),
		rgb_to_color(mix(accent_soft, text, 0.20)),
		rgb_to_color(mix(accent, rgb(255, 255, 255), 0.36)),
		rgb_to_color(mix(type_color, text, 0.22)),
		rgb_to_color(mix(text, accent, 0.28)),
		rgb_to_color(rgb(248, 250, 255)),
	];

	UiTheme {
		accent: rgb_to_color(accent),
		accent_soft: rgb_to_color(accent_soft),
		accent_dim: rgb_to_color(accent_dim),
		bg: rgb_to_color(bg),
		panel: rgb_to_color(panel),
		panel_alt: rgb_to_color(panel_alt),
		text: rgb_to_color(text),
		muted: rgb_to_color(muted),
		border: rgb_to_color(border),
		selection: rgb_to_color(selection),
		special: rgb_to_color(special),
		type_color: rgb_to_color(type_color),
		ansi,
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

fn normalize_hex_color(value: &str) -> Option<String> {
	if value.len() != 7 || !value.starts_with('#') {
		return None;
	}

	parse_hex_color(value).map(|color| format!("#{}", color_hex(color)))
}

#[derive(Clone, Copy)]
struct RgbColor {
	r: u8,
	g: u8,
	b: u8,
}

fn rgb(r: u8, g: u8, b: u8) -> RgbColor {
	RgbColor { r, g, b }
}

fn color_to_rgb(color: Color) -> RgbColor {
	match color {
		Color::Rgb(r, g, b) => rgb(r, g, b),
		Color::Black => rgb(0, 0, 0),
		Color::White => rgb(255, 255, 255),
		Color::Gray => rgb(128, 128, 128),
		Color::DarkGray => rgb(64, 64, 64),
		Color::Red => rgb(255, 0, 0),
		Color::LightRed => rgb(255, 102, 102),
		Color::Green => rgb(0, 255, 0),
		Color::LightGreen => rgb(144, 238, 144),
		Color::Yellow => rgb(255, 255, 0),
		Color::LightYellow => rgb(255, 255, 153),
		Color::Blue => rgb(0, 0, 255),
		Color::LightBlue => rgb(173, 216, 230),
		Color::Magenta => rgb(255, 0, 255),
		Color::LightMagenta => rgb(255, 153, 255),
		Color::Cyan => rgb(0, 255, 255),
		Color::LightCyan => rgb(153, 255, 255),
		Color::Indexed(value) => rgb(value, value, value),
		Color::Reset => rgb(0, 0, 0),
	}
}

fn rgb_to_color(color: RgbColor) -> Color {
	Color::Rgb(color.r, color.g, color.b)
}

fn mix(a: RgbColor, b: RgbColor, ratio: f32) -> RgbColor {
	let ratio = ratio.clamp(0.0, 1.0);
	let blend = |lhs: u8, rhs: u8| -> u8 {
		(lhs as f32 * (1.0 - ratio) + rhs as f32 * ratio).round() as u8
	};

	rgb(blend(a.r, b.r), blend(a.g, b.g), blend(a.b, b.b))
}

fn color_hex(color: Color) -> String {
	let color = color_to_rgb(color);
	format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn blend_rgba_to_rgb(pixel: [u8; 4], background: RgbColor) -> RgbColor {
	let alpha = pixel[3] as f32 / 255.0;
	let blend = |foreground: u8, background: u8| -> u8 {
		(foreground as f32 * alpha + background as f32 * (1.0 - alpha)).round() as u8
	};

	rgb(
		blend(pixel[0], background.r),
		blend(pixel[1], background.g),
		blend(pixel[2], background.b),
	)
}

fn load_editor_preview(path: PathBuf) -> io::Result<EditorPreview> {
	if is_image_path(&path) {
		ImagePreview::load(path).map(EditorPreview::Image)
	} else {
		DocumentPreview::load(path).map(EditorPreview::Document)
	}
}

fn uses_editor_preview(path: &Path) -> bool {
	is_image_path(path) || is_pdf_path(path) || is_notebook_path(path)
}

fn is_image_path(path: &Path) -> bool {
	let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
		return false;
	};

	matches!(
		extension.to_ascii_lowercase().as_str(),
		"png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "ico" | "pbm"
	)
}

fn is_pdf_path(path: &Path) -> bool {
	path
		.extension()
		.and_then(|value| value.to_str())
		.is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
}

fn is_notebook_path(path: &Path) -> bool {
	path
		.extension()
		.and_then(|value| value.to_str())
		.is_some_and(|value| value.eq_ignore_ascii_case("ipynb"))
}

fn preview_lines_from_iter(lines: impl IntoIterator<Item = String>) -> (Vec<Line<'static>>, bool) {
	let mut preview = Vec::new();
	let mut truncated = false;

	for line in lines {
		if preview.len() >= DOCUMENT_PREVIEW_MAX_LINES {
			truncated = true;
			break;
		}
		preview.push(Line::from(line));
	}

	if preview.is_empty() {
		preview.push(Line::from("no preview available"));
	}

	(preview, truncated)
}

fn notebook_lines(source: Option<&serde_json::Value>) -> Vec<String> {
	match source {
		Some(serde_json::Value::String(text)) => text.lines().map(str::to_string).collect(),
		Some(serde_json::Value::Array(lines)) => lines
			.iter()
			.filter_map(serde_json::Value::as_str)
			.flat_map(|line| line.lines().map(str::to_string).collect::<Vec<_>>())
			.collect(),
		_ => Vec::new(),
	}
}

fn notebook_output_lines(outputs: Option<&serde_json::Value>) -> Vec<String> {
	let Some(outputs) = outputs.and_then(serde_json::Value::as_array) else {
		return Vec::new();
	};

	let mut lines = Vec::new();
	for output in outputs {
		match output.get("output_type").and_then(serde_json::Value::as_str) {
			Some("stream") => {
				let name = output
					.get("name")
					.and_then(serde_json::Value::as_str)
					.unwrap_or("stream");
				for line in notebook_lines(output.get("text")) {
					lines.push(format!("> {name}: {line}"));
				}
			}
			Some("execute_result") | Some("display_data") => {
				if let Some(data) = output.get("data") {
					for line in notebook_text_plain_lines(data.get("text/plain")) {
						lines.push(format!("> {line}"));
					}
				}
			}
			Some("error") => {
				for line in notebook_lines(output.get("traceback")) {
					lines.push(format!("! {line}"));
				}
			}
			_ => {}
		}
	}

	lines
}

fn notebook_text_plain_lines(value: Option<&serde_json::Value>) -> Vec<String> {
	match value {
		Some(serde_json::Value::String(text)) => text.lines().map(str::to_string).collect(),
		Some(serde_json::Value::Array(lines)) => lines
			.iter()
			.filter_map(serde_json::Value::as_str)
			.map(str::to_string)
			.collect(),
		_ => Vec::new(),
	}
}

fn codex_last_message_path() -> PathBuf {
	let stamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis();
	env::temp_dir().join(format!("veditor-codex-last-message-{stamp}.txt"))
}

fn nvim_theme_lua(ui: UiTheme) -> String {
	let ansi = ui.ansi.map(color_hex);
	format!(
		"local p={{bg='{bg}',panel='{panel}',panel_alt='{panel_alt}',text='{text}',muted='{muted}',accent='{accent}',accent_soft='{accent_soft}',accent_dim='{accent_dim}',special='{special}',type_='{type_color}',select='{selection}'}} local set=vim.api.nvim_set_hl set(0,'Normal',{{fg=p.text,bg=p.panel}}) set(0,'NormalNC',{{fg=p.text,bg=p.panel}}) set(0,'NormalFloat',{{fg=p.text,bg=p.panel_alt}}) set(0,'FloatBorder',{{fg=p.accent_dim,bg=p.panel_alt}}) set(0,'SignColumn',{{bg=p.panel}}) set(0,'EndOfBuffer',{{fg=p.panel,bg=p.panel}}) set(0,'LineNr',{{fg=p.muted,bg=p.panel}}) set(0,'CursorLineNr',{{fg=p.accent,bg=p.panel,bold=true}}) set(0,'CursorLine',{{bg=p.bg}}) set(0,'CursorColumn',{{bg=p.bg}}) set(0,'ColorColumn',{{bg=p.bg}}) set(0,'Visual',{{bg=p.select}}) set(0,'Search',{{fg=p.bg,bg=p.accent}}) set(0,'IncSearch',{{fg=p.bg,bg=p.accent_soft,bold=true}}) set(0,'MatchParen',{{fg=p.accent_soft,bg=p.bg,bold=true}}) set(0,'StatusLine',{{fg=p.bg,bg=p.accent,bold=true}}) set(0,'StatusLineNC',{{fg=p.text,bg=p.panel_alt}}) set(0,'VertSplit',{{fg=p.accent_dim,bg=p.panel}}) set(0,'WinSeparator',{{fg=p.accent_dim,bg=p.panel}}) set(0,'Pmenu',{{fg=p.text,bg=p.panel_alt}}) set(0,'PmenuSel',{{fg=p.bg,bg=p.accent}}) set(0,'Comment',{{fg=p.muted,italic=true}}) set(0,'Constant',{{fg=p.accent_soft}}) set(0,'String',{{fg=p.type_}}) set(0,'Character',{{fg=p.type_}}) set(0,'Number',{{fg=p.special}}) set(0,'Boolean',{{fg=p.special,bold=true}}) set(0,'Float',{{fg=p.special}}) set(0,'Identifier',{{fg=p.text}}) set(0,'Function',{{fg=p.accent_soft,bold=true}}) set(0,'Statement',{{fg=p.accent,bold=true}}) set(0,'Conditional',{{fg=p.accent,bold=true}}) set(0,'Repeat',{{fg=p.accent,bold=true}}) set(0,'Label',{{fg=p.accent}}) set(0,'Operator',{{fg=p.text}}) set(0,'Keyword',{{fg=p.accent,bold=true}}) set(0,'Exception',{{fg=p.special,bold=true}}) set(0,'PreProc',{{fg=p.type_}}) set(0,'Include',{{fg=p.type_}}) set(0,'Define',{{fg=p.type_}}) set(0,'Macro',{{fg=p.type_}}) set(0,'PreCondit',{{fg=p.type_}}) set(0,'Type',{{fg=p.type_,bold=true}}) set(0,'StorageClass',{{fg=p.type_}}) set(0,'Structure',{{fg=p.type_}}) set(0,'Typedef',{{fg=p.type_}}) set(0,'Special',{{fg=p.special}}) set(0,'SpecialChar',{{fg=p.special}}) set(0,'Delimiter',{{fg=p.accent_dim}}) set(0,'SpecialComment',{{fg=p.muted}}) set(0,'Todo',{{fg=p.bg,bg=p.accent_soft,bold=true}}) vim.g.terminal_color_0='{c0}' vim.g.terminal_color_1='{c1}' vim.g.terminal_color_2='{c2}' vim.g.terminal_color_3='{c3}' vim.g.terminal_color_4='{c4}' vim.g.terminal_color_5='{c5}' vim.g.terminal_color_6='{c6}' vim.g.terminal_color_7='{c7}' vim.g.terminal_color_8='{c8}' vim.g.terminal_color_9='{c9}' vim.g.terminal_color_10='{c10}' vim.g.terminal_color_11='{c11}' vim.g.terminal_color_12='{c12}' vim.g.terminal_color_13='{c13}' vim.g.terminal_color_14='{c14}' vim.g.terminal_color_15='{c15}'",
		bg = color_hex(ui.bg),
		panel = color_hex(ui.panel),
		panel_alt = color_hex(ui.panel_alt),
		text = color_hex(ui.text),
		muted = color_hex(ui.muted),
		accent = color_hex(ui.accent),
		accent_soft = color_hex(ui.accent_soft),
		accent_dim = color_hex(ui.accent_dim),
		special = color_hex(ui.special),
		type_color = color_hex(ui.type_color),
		selection = color_hex(ui.selection),
		c0 = ansi[0],
		c1 = ansi[1],
		c2 = ansi[2],
		c3 = ansi[3],
		c4 = ansi[4],
		c5 = ansi[5],
		c6 = ansi[6],
		c7 = ansi[7],
		c8 = ansi[8],
		c9 = ansi[9],
		c10 = ansi[10],
		c11 = ansi[11],
		c12 = ansi[12],
		c13 = ansi[13],
		c14 = ansi[14],
		c15 = ansi[15],
	)
}

fn build_nvim_theme_command(ui: UiTheme) -> String {
	format!("+lua {}", nvim_theme_lua(ui))
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
	io::Error::other(error.to_string())
}
