use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use image::{DynamicImage, GenericImageView};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    DefaultTerminal,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Gauge, List, ListItem, ListState, Paragraph, Sparkline, Wrap},
};
use vt100::{Color as VtColor, Parser};

mod app;
mod codex;
mod persistence;
mod preview;
mod project;
mod pty;
mod render;
mod theme;

use persistence::{
    load_global_settings, load_saved_session, nvim_snapshot_path, parse_nvim_buffer_state,
    save_global_settings, save_saved_session, session_state_path,
};
use preview::{load_editor_preview, uses_editor_preview};
use render::render;
use theme::{
    accent_preview_line, build_nvim_theme_command, color_hex, normalize_hex_color, nvim_theme_lua,
    ui_theme,
};

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

// Core application state shared across the internal modules.

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
    accent_registry: BTreeMap<String, String>,
    command_output: Option<CommandOutput>,
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
    codex_history_area: Option<Rect>,
    codex_change_list_area: Option<Rect>,
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
    last_change_set: Option<CodexChangeSet>,
    history_scroll: usize,
    change_scroll: usize,
}

struct ChatMessage {
    role: ChatRole,
    content: String,
    pending_request_id: Option<u64>,
}

#[derive(Clone)]
struct CodexChangeSet {
    working_root: PathBuf,
    files: Vec<CodexChangedFile>,
    reverse_patch: Option<String>,
}

#[derive(Clone)]
struct CodexChangedFile {
    path: PathBuf,
    additions: usize,
    deletions: usize,
}

struct CommandOutput {
    title: String,
    lines: Vec<Line<'static>>,
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
    reply: Result<CodexResponse, String>,
}

struct SessionState {
    root: PathBuf,
    open_files: Vec<PathBuf>,
    active_file: Option<PathBuf>,
    accent_hex: Option<String>,
    accent_registry: BTreeMap<String, String>,
}

struct GlobalSettings {
    accent_hex: Option<String>,
    accent_registry: BTreeMap<String, String>,
}

struct NvimBufferState {
    files: Vec<PathBuf>,
    current: Option<PathBuf>,
}

struct CodexResponse {
    reply: String,
    change_set: Option<CodexChangeSet>,
}

struct CodexExecResponse {
    reply: String,
    turn_diff: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
struct WorkspaceFileState {
    len: u64,
    modified_ms: Option<u128>,
}

// Startup, path resolution, and keyboard helpers shared across modules.

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
    !key.modifiers
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

fn find_first_project_file(dir: &Path) -> Option<PathBuf> {
    let read_dir = fs::read_dir(dir).ok()?;
    let mut entries = read_dir.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
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

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn clamp_scroll(scroll: usize, total_lines: usize, viewport_height: u16) -> usize {
    let visible = viewport_height as usize;
    if visible == 0 || total_lines <= visible {
        return 0;
    }
    scroll.min(total_lines.saturating_sub(visible))
}

// Project discovery and workspace traversal helpers.

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
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
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

// Codex diff parsing and change-set normalization helpers.

fn capture_workspace_manifest(root: &Path) -> io::Result<HashMap<PathBuf, WorkspaceFileState>> {
    let mut manifest = HashMap::new();
    collect_workspace_manifest(root, root, &mut manifest)?;
    Ok(manifest)
}

fn collect_workspace_manifest(
    root: &Path,
    dir: &Path,
    manifest: &mut HashMap<PathBuf, WorkspaceFileState>,
) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_workspace_manifest(root, &path, manifest)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis());
        manifest.insert(
            relative.to_path_buf(),
            WorkspaceFileState {
                len: metadata.len(),
                modified_ms,
            },
        );
    }

    Ok(())
}

fn workspace_changed_paths(
    before: &HashMap<PathBuf, WorkspaceFileState>,
    after: &HashMap<PathBuf, WorkspaceFileState>,
) -> Vec<PathBuf> {
    let mut paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(path) != after.get(path))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn build_codex_change_set(
    project_root: &Path,
    working_project: &Path,
    before_manifest: &HashMap<PathBuf, WorkspaceFileState>,
    turn_diff: Option<&str>,
) -> Option<CodexChangeSet> {
    let after_manifest = capture_workspace_manifest(working_project).ok()?;
    let changed_paths = workspace_changed_paths(before_manifest, &after_manifest);
    if changed_paths.is_empty() {
        return None;
    }

    let reverse_patch = turn_diff
        .and_then(|diff| normalize_codex_turn_diff(diff, working_project))
        .filter(|diff| !diff.trim().is_empty());
    let diff_stats = reverse_patch
        .as_deref()
        .map(|diff| parse_codex_diff_stats(diff, working_project))
        .unwrap_or_default();
    let mut files = changed_paths
        .into_iter()
        .map(|relative_path| {
            let path = working_project.join(&relative_path);
            let (additions, deletions) = diff_stats.get(&path).copied().unwrap_or((0, 0));
            CodexChangedFile {
                path,
                additions,
                deletions,
            }
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|file| relative_to_root(project_root, &file.path));

    Some(CodexChangeSet {
        working_root: working_project.to_path_buf(),
        files,
        reverse_patch,
    })
}

fn normalize_codex_turn_diff(diff: &str, working_project: &Path) -> Option<String> {
    let mut normalized = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (left, right) = rest.split_once(' ')?;
            let left = normalize_git_diff_side(left, working_project)?;
            let right = normalize_git_diff_side(right, working_project)?;
            normalized.push(format!("diff --git {left} {right}"));
        } else if let Some(path) = line.strip_prefix("--- ") {
            normalized.push(format!(
                "--- {}",
                normalize_diff_header_path(path, working_project)?
            ));
        } else if let Some(path) = line.strip_prefix("+++ ") {
            normalized.push(format!(
                "+++ {}",
                normalize_diff_header_path(path, working_project)?
            ));
        } else {
            normalized.push(line.to_string());
        }
    }

    if normalized.is_empty() {
        return None;
    }

    let mut text = normalized.join("\n");
    text.push('\n');
    Some(text)
}

fn normalize_git_diff_side(side: &str, working_project: &Path) -> Option<String> {
    if let Some(path) = side.strip_prefix("a/") {
        return Some(format!("a/{}", normalize_diff_path(path, working_project)?));
    }
    if let Some(path) = side.strip_prefix("b/") {
        return Some(format!("b/{}", normalize_diff_path(path, working_project)?));
    }
    Some(normalize_diff_path(side, working_project)?)
}

fn normalize_diff_header_path(path: &str, working_project: &Path) -> Option<String> {
    if path == "/dev/null" {
        return Some(path.to_string());
    }
    if let Some(value) = path.strip_prefix("a/") {
        return Some(format!(
            "a/{}",
            normalize_diff_path(value, working_project)?
        ));
    }
    if let Some(value) = path.strip_prefix("b/") {
        return Some(format!(
            "b/{}",
            normalize_diff_path(value, working_project)?
        ));
    }
    Some(normalize_diff_path(path, working_project)?)
}

fn normalize_diff_path(path: &str, working_project: &Path) -> Option<String> {
    let raw = path.trim();
    if raw == "/dev/null" {
        return Some(raw.to_string());
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        let relative = candidate.strip_prefix(working_project).ok()?;
        return Some(relative.to_string_lossy().replace('\\', "/"));
    }
    Some(raw.trim_start_matches("./").to_string())
}

fn parse_codex_diff_stats(diff: &str, working_project: &Path) -> HashMap<PathBuf, (usize, usize)> {
    let mut stats = HashMap::new();
    let mut current_path = None;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some((_, new_path)) = rest.split_once(" b/") {
                let path = working_project.join(new_path);
                stats.entry(path.clone()).or_insert((0, 0));
                current_path = Some(path);
            }
            continue;
        }

        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }

        let Some(path) = current_path.as_ref() else {
            continue;
        };

        if line.starts_with('+') {
            if let Some((additions, _)) = stats.get_mut(path) {
                *additions += 1;
            }
        } else if line.starts_with('-') {
            if let Some((_, deletions)) = stats.get_mut(path) {
                *deletions += 1;
            }
        }
    }

    stats
}

fn parse_codex_exec_stdout(stdout: &str) -> (Option<String>, Option<String>) {
    let mut turn_diff = None;
    let mut last_agent_message = None;

    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("msg") else {
            continue;
        };
        let Some(kind) = message.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match kind {
            "turn_diff" => {
                if let Some(diff) = message
                    .get("unified_diff")
                    .and_then(serde_json::Value::as_str)
                {
                    turn_diff = Some(diff.to_string());
                }
            }
            "agent_message" => {
                if let Some(text) = message.get("message").and_then(serde_json::Value::as_str) {
                    last_agent_message = Some(text.to_string());
                }
            }
            _ => {}
        }
    }

    (turn_diff, last_agent_message)
}

// External process and system metrics helpers.

fn request_codex_reply(
    working_project: &Path,
    transcript: &str,
) -> Result<CodexExecResponse, String> {
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
            "workspace-write",
            "--color",
            "never",
            "--json",
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

    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let last_message = fs::read_to_string(&output_path)
        .ok()
        .map(|text| text.trim().to_string());
    let _ = fs::remove_file(&output_path);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let (turn_diff, last_agent_message) = parse_codex_exec_stdout(&stdout);

    if output.status.success() {
        if let Some(message) = last_message
            .as_ref()
            .filter(|message| !message.is_empty())
            .cloned()
        {
            return Ok(CodexExecResponse {
                reply: message,
                turn_diff,
            });
        }
        if let Some(message) = last_agent_message
            .as_ref()
            .filter(|message| !message.is_empty())
            .cloned()
        {
            return Ok(CodexExecResponse {
                reply: message,
                turn_diff,
            });
        }
    }

    if let Some(message) = last_message
        .as_ref()
        .filter(|message| !message.is_empty())
        .cloned()
    {
        return Ok(CodexExecResponse {
            reply: message,
            turn_diff,
        });
    }
    if let Some(message) = last_agent_message
        .as_ref()
        .filter(|message| !message.is_empty())
        .cloned()
    {
        return Ok(CodexExecResponse {
            reply: message,
            turn_diff,
        });
    }
    if !stderr.is_empty() {
        return Err(stderr);
    }
    let stdout = stdout.trim().to_string();
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
            || (busiest.pid == root_pid
                && process.pid != root_pid
                && process.cpu_percent >= busiest.cpu_percent)
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

// Small shared utility helpers.

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

fn codex_last_message_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    env::temp_dir().join(format!("veditor-codex-last-message-{stamp}.txt"))
}

fn codex_undo_patch_path() -> io::Result<PathBuf> {
    let Some(path) = session_state_path() else {
        return Err(io_error("session path unavailable"));
    };
    let parent = path
        .parent()
        .ok_or_else(|| io_error("session directory unavailable"))?;
    Ok(parent.join("codex-undo.patch"))
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use crate::theme::normalize_hex_color;

    #[test]
    fn normalize_hex_color_keeps_single_hash() {
        assert_eq!(normalize_hex_color("#123AbC"), Some("#123abc".to_string()));
    }

    #[test]
    fn normalize_hex_color_recovers_double_hash_values() {
        assert_eq!(normalize_hex_color("##123AbC"), Some("#123abc".to_string()));
    }
}
