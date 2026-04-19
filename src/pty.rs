//! PTY lifecycle, keyboard encoding, and terminal metrics helpers.

use crate::*;

impl TerminalMetrics {
    /// Creates a new terminal metrics accumulator for the current shell process tree.
    pub(crate) fn new(shell_pid: Option<u32>) -> Self {
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

    /// Returns terminal tree memory usage as a percentage of physical RAM.
    pub(crate) fn memory_percent(&self) -> f32 {
        if self.total_memory_bytes == 0 {
            return 0.0;
        }

        (self.mem_bytes as f64 / self.total_memory_bytes as f64 * 100.0) as f32
    }

    /// Formats the current and total memory footprint for the dashboard gauge label.
    pub(crate) fn memory_label(&self) -> String {
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
    /// Spawns the embedded Neovim editor pane with the current project theme applied.
    pub(crate) fn spawn_nvim(file_path: PathBuf, cwd: PathBuf, ui: UiTheme) -> io::Result<Self> {
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

    /// Spawns the integrated shell pane rooted at the selected project directory.
    pub(crate) fn spawn_shell(cwd: PathBuf) -> io::Result<Self> {
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

    /// Resizes the PTY to match the current widget rectangle.
    pub(crate) fn resize(&mut self, area: Rect) {
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

    /// Sends a normalized keyboard event into the PTY session.
    pub(crate) fn send_key(&mut self, key: KeyEvent) -> io::Result<()> {
        let payload = self.encode_key(key);
        if payload.is_empty() {
            return Ok(());
        }
        self.writer.write_all(&payload)?;
        self.writer.flush()
    }

    /// Sends pasted text, honoring bracketed paste mode when available.
    pub(crate) fn send_paste(&mut self, text: &str) -> io::Result<()> {
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

    /// Opens a file inside the embedded Neovim instance.
    pub(crate) fn open_file(&mut self, path: &Path) -> io::Result<()> {
        let escaped = escape_nvim_path(path);
        let command = format!("\x1b:drop {escaped}\r");
        self.writer.write_all(command.as_bytes())?;
        self.writer.flush()
    }

    /// Triggers Neovim's `:checktime` to pick up external file changes.
    pub(crate) fn checktime(&mut self) -> io::Result<()> {
        self.writer.write_all(b"\x1b:checktime\r")?;
        self.writer.flush()
    }

    /// Reapplies the current UI theme to Neovim highlights.
    pub(crate) fn apply_theme(&mut self, ui: UiTheme) -> io::Result<()> {
        let command = format!("\x1b:lua {}\r", nvim_theme_lua(ui));
        self.writer.write_all(command.as_bytes())?;
        self.writer.flush()
    }

    /// Dumps listed Neovim buffers to the configured snapshot file.
    pub(crate) fn dump_buffer_state(&mut self, path: &Path) -> io::Result<()> {
        let escaped = escape_lua_string(path);
        let command = format!("\x1b:lua _G.veditor_dump_buffers('{escaped}')\r");
        self.writer.write_all(command.as_bytes())?;
        self.writer.flush()
    }

    /// Returns whether the child process has already exited.
    pub(crate) fn is_exited(&mut self) -> bool {
        self.poll_exit().is_some()
    }

    /// Returns the child process id when available.
    pub(crate) fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Captures the current screen contents as styled ratatui lines.
    pub(crate) fn snapshot(&self, ui: UiTheme) -> PtySnapshot {
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
            KeyCode::Left => {
                bytes.extend_from_slice(if app_cursor { b"\x1bOD" } else { b"\x1b[D" })
            }
            KeyCode::Right => {
                bytes.extend_from_slice(if app_cursor { b"\x1bOC" } else { b"\x1b[C" })
            }
            KeyCode::Up => bytes.extend_from_slice(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
            KeyCode::Down => {
                bytes.extend_from_slice(if app_cursor { b"\x1bOB" } else { b"\x1b[B" })
            }
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

    /// Polls the child process for exit and memoizes the first observed status.
    pub(crate) fn poll_exit(&mut self) -> Option<String> {
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

impl Drop for PtyPane {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
