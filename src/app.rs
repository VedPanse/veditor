//! Application lifecycle, event handling, commands, and session coordination.

use crate::*;

impl App {
    /// Constructs the application state for either a requested path or the last saved session.
    pub(crate) fn new(requested_path: Option<PathBuf>) -> io::Result<Self> {
        let global_settings = load_global_settings();
        let saved_session = load_saved_session();
        let saved_accent_hex = global_settings
            .as_ref()
            .and_then(|settings| settings.accent_hex.as_deref())
            .and_then(normalize_hex_color)
            .or_else(|| {
                saved_session
                    .as_ref()
                    .and_then(|session| session.accent_hex.as_deref())
                    .and_then(normalize_hex_color)
            })
            .unwrap_or_else(|| ACCENT_COLOR.to_string());
        let saved_mood = global_settings
            .as_ref()
            .and_then(|settings| settings.mood.as_deref())
            .and_then(normalize_theme_mood)
            .or_else(|| {
                saved_session
                    .as_ref()
                    .and_then(|session| session.mood.as_deref())
                    .and_then(normalize_theme_mood)
            })
            .unwrap_or(ThemeMood::Default);
        let saved_accent_registry = global_settings
            .as_ref()
            .map(|settings| settings.accent_registry.clone())
            .filter(|registry| !registry.is_empty())
            .or_else(|| {
                saved_session
                    .as_ref()
                    .map(|session| session.accent_registry.clone())
            })
            .unwrap_or_default();
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
                requested_path.parent().unwrap_or(&cwd).to_path_buf()
            };
            (root, None, Some(requested_path))
        } else {
            let root = saved_session
                .as_ref()
                .map(|session| session.root.clone())
                .unwrap_or(cwd);
            (root, saved_session, None)
        };
        let ui = ui_theme(&saved_accent_hex, saved_mood);
        let accent_registry = saved_accent_registry;
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
            accent_hex: saved_accent_hex,
            mood: saved_mood,
            ui,
            accent_registry,
            command_output: None,
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
            keyboard_audio: KeyboardAudio::new(),
            codex_history_area: None,
            codex_change_list_area: None,
        }
        .with_metrics();
        app.restore_session_files(restored_files, restored_active, initial_editor_target)?;
        let _ = app.persist_session_state(false);
        Ok(app)
    }

    /// Advances background state, polls PTY exits, and refreshes transient UI data.
    pub(crate) fn tick(&mut self) {
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

    /// Dispatches a terminal event into the active focus target.
    pub(crate) fn handle_event(&mut self, event: Event) -> AppAction {
        play_keyboard_sound_for_event(&mut self.keyboard_audio, &event);

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
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => AppAction::Continue,
            _ => AppAction::Continue,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> AppAction {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self
                    .codex_change_list_area
                    .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                {
                    self.codex_chat.scroll_change_list(-3);
                } else if self
                    .codex_history_area
                    .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                {
                    self.codex_chat.scroll_history(-3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .codex_change_list_area
                    .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                {
                    self.codex_chat.scroll_change_list(3);
                } else if self
                    .codex_history_area
                    .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                {
                    self.codex_chat.scroll_history(3);
                }
            }
            _ => {}
        }

        AppAction::Continue
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
                    self.status_message =
                        format!("no match for {}", self.project_tree.search_query);
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
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
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
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
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

                if let Err(error) = self
                    .commit_project_picker_selection(key.modifiers.contains(KeyModifiers::SHIFT))
                {
                    self.status_message = format!("switch failed: {error}");
                }
            }
            KeyCode::Backspace
                if self
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
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
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
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Err(error) = self.undo_last_codex_change() {
                    self.status_message = format!("undo failed: {error}");
                }
                AppAction::Continue
            }
            KeyCode::Up
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.scroll_change_list(-1);
                AppAction::Continue
            }
            KeyCode::Up => {
                self.codex_chat.scroll_history(-1);
                AppAction::Continue
            }
            KeyCode::Down
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.scroll_change_list(1);
                AppAction::Continue
            }
            KeyCode::Down => {
                self.codex_chat.scroll_history(1);
                AppAction::Continue
            }
            KeyCode::PageUp
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.scroll_change_list(-6);
                AppAction::Continue
            }
            KeyCode::PageUp => {
                self.codex_chat.scroll_history(-6);
                AppAction::Continue
            }
            KeyCode::PageDown
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.scroll_change_list(6);
                AppAction::Continue
            }
            KeyCode::PageDown => {
                self.codex_chat.scroll_history(6);
                AppAction::Continue
            }
            KeyCode::Home
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.change_scroll = 0;
                AppAction::Continue
            }
            KeyCode::Home => {
                self.codex_chat.history_scroll = 0;
                AppAction::Continue
            }
            KeyCode::End
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.codex_chat.change_scroll = usize::MAX;
                AppAction::Continue
            }
            KeyCode::End => {
                self.codex_chat.history_scroll = usize::MAX;
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
                    "select a project. enter opens project, shift+enter opens directory"
                        .to_string();
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
        self.command_output = None;
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
                self.status_message = format!(
                    "tree search: {}",
                    relative_to_root(&self.project_tree.root, &path)
                );
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
                    self.status_message = format!(
                        "project search: {}",
                        relative_to_root(&picker.current_dir, &path)
                    );
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
        let Some(command) = self.command_prompt.clone() else {
            return Ok(());
        };

        let trimmed = command.trim().to_string();
        let parts = trimmed.split_whitespace().collect::<Vec<_>>();
        match parts.as_slice() {
            [":set", "mood"] => {
                self.apply_mood_command(ThemeMood::Synthwave84)?;
                self.command_prompt = None;
                self.command_output = Some(self.build_mood_output());
                self.status_message = "mood set to synthwave84".to_string();
                Ok(())
            }
            [":set", "mood", value] => {
                let mood = normalize_theme_mood(value).ok_or_else(|| {
                    io_error("usage: :set mood | :set mood synthwave84 | :set mood default")
                })?;
                self.apply_mood_command(mood)?;
                self.command_prompt = None;
                self.command_output = Some(self.build_mood_output());
                self.status_message = format!("mood set to {}", theme_mood_name(mood));
                Ok(())
            }
            [":set", "accent", value] => {
                let hex = self
                    .resolve_accent_value(value)
                    .ok_or_else(|| io_error("usage: :set accent #RRGGBB | :set accent <NAME>"))?;
                self.apply_accent_command(&hex)?;
                self.command_prompt = None;
                self.command_output = Some(CommandOutput {
                    title: "accent".to_string(),
                    lines: vec![accent_preview_line("active", value, &hex, self.ui)],
                });
                self.status_message = format!("accent set to {hex}");
                Ok(())
            }
            [":set", "accent", hex, "register", name] => {
                let hex = normalize_hex_color(hex)
                    .ok_or_else(|| io_error("usage: :set accent #RRGGBB register <NAME>"))?;
                self.register_accent(name, &hex);
                self.apply_accent_command(&hex)?;
                self.command_prompt = None;
                self.command_output = Some(CommandOutput {
                    title: "registered accent".to_string(),
                    lines: vec![accent_preview_line("saved", name, &hex, self.ui)],
                });
                self.status_message = format!("registered accent {name} as {hex}");
                Ok(())
            }
            [":get", "accent"] => {
                self.command_output = Some(self.build_accent_registry_output());
                self.command_prompt = None;
                self.status_message = "listed registered accent colors".to_string();
                Ok(())
            }
            [":get", "mood"] => {
                self.command_output = Some(self.build_mood_output());
                self.command_prompt = None;
                self.status_message = format!("current mood {}", theme_mood_name(self.mood));
                Ok(())
            }
            _ => Err(io_error("unknown command")),
        }
    }

    fn apply_accent_command(&mut self, hex: &str) -> io::Result<()> {
        self.accent_hex = hex.to_string();
        self.ui = ui_theme(&self.accent_hex, self.mood);
        if !self.nvim.is_exited() {
            self.nvim.apply_theme(self.ui)?;
        }
        let _ = self.persist_session_state(false);
        Ok(())
    }

    fn apply_mood_command(&mut self, mood: ThemeMood) -> io::Result<()> {
        self.mood = mood;
        self.ui = ui_theme(&self.accent_hex, self.mood);
        if !self.nvim.is_exited() {
            self.nvim.apply_theme(self.ui)?;
        }
        let _ = self.persist_session_state(false);
        Ok(())
    }

    fn resolve_accent_value(&self, value: &str) -> Option<String> {
        if let Some(hex) = normalize_hex_color(value) {
            return Some(hex);
        }

        let needle = value.trim();
        self.accent_registry
            .iter()
            .find_map(|(name, hex)| name.eq_ignore_ascii_case(needle).then(|| hex.clone()))
    }

    fn register_accent(&mut self, name: &str, hex: &str) {
        let needle = name.trim();
        if needle.is_empty() {
            return;
        }

        if let Some(existing_name) = self
            .accent_registry
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(needle))
            .cloned()
        {
            self.accent_registry.remove(&existing_name);
        }
        self.accent_registry
            .insert(needle.to_string(), hex.to_string());
    }

    fn build_accent_registry_output(&self) -> CommandOutput {
        let mut lines = Vec::new();
        if self.accent_registry.is_empty() {
            lines.push(Line::styled(
                "no registered accents",
                Style::default().fg(self.ui.muted),
            ));
        } else {
            for (name, hex) in &self.accent_registry {
                lines.push(accent_preview_line("accent", name, hex, self.ui));
            }
        }

        CommandOutput {
            title: "registered accents".to_string(),
            lines,
        }
    }

    fn build_mood_output(&self) -> CommandOutput {
        let badge = match self.mood {
            ThemeMood::Default => "calm",
            ThemeMood::Synthwave84 => "neon",
        };
        let description = match self.mood {
            ThemeMood::Default => "standard palette using the active accent",
            ThemeMood::Synthwave84 => "synthwave84 glow enabled over the active accent",
        };

        CommandOutput {
            title: "mood".to_string(),
            lines: vec![
                Line::from(vec![
                    Span::styled(
                        "mood  ",
                        Style::default()
                            .fg(self.ui.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        theme_mood_name(self.mood).to_string(),
                        Style::default()
                            .fg(self.ui.bg)
                            .bg(self.ui.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(badge, Style::default().fg(self.ui.special)),
                ]),
                Line::styled(description, Style::default().fg(self.ui.muted)),
            ],
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
            self.codex_chat.clear_change_set();
            self.codex_chat.history_scroll = 0;
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
        self.codex_chat.history_scroll = usize::MAX;
        self.status_message = format!("sending codex request for {working_label}");

        let tx = self.codex_tx.clone();
        let project_root = self.project_tree.root.clone();
        thread::spawn(move || {
            let before_manifest = capture_workspace_manifest(&working_project).ok();
            let reply = request_codex_reply(&working_project, &transcript).map(|response| {
                let change_set = before_manifest.as_ref().and_then(|before_manifest| {
                    build_codex_change_set(
                        &project_root,
                        &working_project,
                        before_manifest,
                        response.turn_diff.as_deref(),
                    )
                });
                CodexResponse {
                    reply: response.reply,
                    change_set,
                }
            });
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
            self.pending_codex_request = self
                .pending_codex_request
                .filter(|id| *id != result.request_id);
            match result.reply {
                Ok(response) => {
                    let changed_files = response
                        .change_set
                        .as_ref()
                        .map(|change_set| change_set.files.len());
                    self.codex_chat
                        .resolve_pending(result.request_id, response.reply);
                    self.codex_chat.set_change_set(response.change_set);
                    self.codex_chat.history_scroll = usize::MAX;
                    self.refresh_after_codex_reply();
                    self.status_message = changed_files
                        .map(|count| match count {
                            0 => "codex replied".to_string(),
                            1 => "codex replied and changed 1 file".to_string(),
                            _ => format!("codex replied and changed {count} files"),
                        })
                        .unwrap_or_else(|| "codex replied".to_string());
                }
                Err(error) => {
                    self.codex_chat
                        .resolve_pending(result.request_id, format!("request failed: {error}"));
                    self.status_message = "codex request failed".to_string();
                }
            }
        }
    }

    fn refresh_after_codex_reply(&mut self) {
        let selected_path = self
            .project_tree
            .visible
            .get(self.project_tree.selected)
            .map(|entry| entry.path.clone())
            .filter(|path| path.exists())
            .or_else(|| self.active_file.clone().filter(|path| path.exists()))
            .or_else(|| Some(self.project_tree.root.clone()));
        self.project_tree.refresh(selected_path);

        if let Some(active_file) = self.active_file.clone() {
            if uses_editor_preview(&active_file) {
                if active_file.exists() {
                    if let Ok(preview) = load_editor_preview(active_file.clone()) {
                        self.editor_preview = Some(preview);
                    }
                } else {
                    self.editor_preview = None;
                    self.active_file = None;
                }
            } else if !self.nvim.is_exited() {
                let _ = self.nvim.checktime();
            }
        } else if !self.nvim.is_exited() {
            let _ = self.nvim.checktime();
        }

        let _ = self.persist_session_state(false);
    }

    fn undo_last_codex_change(&mut self) -> io::Result<()> {
        if self.pending_codex_request.is_some() {
            self.status_message = "wait for codex to finish before undo".to_string();
            return Ok(());
        }

        let Some(change_set) = self.codex_chat.last_change_set.clone() else {
            self.status_message = "no codex changes to undo".to_string();
            return Ok(());
        };
        let Some(reverse_patch) = change_set.reverse_patch.clone() else {
            self.status_message = "undo unavailable for last codex change".to_string();
            return Ok(());
        };

        let patch_path = codex_undo_patch_path()?;
        if let Some(parent) = patch_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&patch_path, reverse_patch)?;

        let output = Command::new("git")
            .arg("apply")
            .arg("-R")
            .arg("--unsafe-paths")
            .arg(&patch_path)
            .current_dir(&change_set.working_root)
            .output()?;
        let _ = fs::remove_file(&patch_path);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            return Err(io_error(if detail.is_empty() {
                format!("undo failed with status {}", output.status)
            } else {
                format!("undo failed: {detail}")
            }));
        }

        self.codex_chat.clear_change_set();
        self.codex_chat
            .push_assistant("undid the last codex change.");
        self.refresh_after_codex_reply();
        self.status_message = "undid last codex change".to_string();
        Ok(())
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
            && !self
                .open_files
                .iter()
                .any(|path| path == &initial_editor_target)
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

    pub(crate) fn persist_session_state(&mut self, sync_nvim: bool) -> io::Result<()> {
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
            accent_hex: Some(self.accent_hex.clone()),
            mood: Some(theme_mood_name(self.mood).to_string()),
            accent_registry: self.accent_registry.clone(),
        })?;
        save_global_settings(&GlobalSettings {
            accent_hex: Some(self.accent_hex.clone()),
            mood: Some(theme_mood_name(self.mood).to_string()),
            accent_registry: self.accent_registry.clone(),
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
