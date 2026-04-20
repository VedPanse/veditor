# veditor

![Rust](https://img.shields.io/badge/Rust-Edition%202024-f74c00?logo=rust&logoColor=white)
![UI](https://img.shields.io/badge/UI-ratatui-111827)
![Editor](https://img.shields.io/badge/Editor-Neovim-57A143?logo=neovim&logoColor=white)
![License](https://img.shields.io/badge/License-MIT-blue.svg)

`veditor` is a keyboard-first terminal code editor built in Rust with a `ratatui` dashboard, an embedded `nvim` pane, an integrated shell, a project tree, and a Codex workflow panel.

## What It Does

- Uses real embedded `nvim` for editing instead of reimplementing a text buffer.
- Keeps an integrated shell in the same UI so you can run commands without leaving the editor.
- Shows a project tree with keyboard navigation, search, file creation, directory creation, and project switching.
- Restores the last workspace, open files, active file, accent, and mood between launches.
- Renders image previews, notebook previews, PDF previews, and README markdown previews directly in the editor pane.
- Includes a Codex chat area with markdown-aware rendering and change tracking.
- Supports accent theming and neon synthwave mood across the TUI and embedded editor styling.

## Features

- Embedded editor:
  - Real modal editing through `nvim`
  - External file refresh through `:checktime`
  - Theme sync between the dashboard and editor
- Project workflow:
  - Searchable project tree
  - File and directory creation from the tree
  - Project switching without leaving the app
  - Session restore for the last project
- Previews:
  - Images: `png`, `jpg`, `jpeg`, `gif`, `webp`, `bmp`, `tiff`, `ico`, `pbm`
  - Documents: `pdf`, `ipynb`
  - Markdown README preview in the editor pane
- Codex pane:
  - Inline prompt area
  - Markdown-aware output rendering
  - Change list and undo flow for Codex-applied edits
- Theming:
  - `:set accent` and `:get accent`
  - `:set mood neon` / `:set mood default`
  - Persistent accent and mood state

## Controls

- Global:
  - `Ctrl-W`: cycle focus between editor, terminal, project tree, and Codex
- Project tree:
  - `Up` / `Down`: move selection
  - `Enter`: open file or expand/collapse directory
  - Start typing: incremental search
  - `Backspace`: delete one search character
  - `Esc`: clear search or quit
  - `%`: create file in project root
  - `Ctrl-D`: create directory in project root
  - `Ctrl-N`: create file in selected directory
  - `Ctrl-Shift-N`: create directory in selected directory
  - `Cmd-O` on macOS / `Ctrl-O` elsewhere: open another project
- Codex:
  - `Enter`: send prompt
  - `Ctrl-Z`: undo the last Codex-generated change when available
  - `Up` / `Down` / `PageUp` / `PageDown`: scroll chat history
  - `Shift` or `Ctrl` with arrows/pages: scroll the change list
- Editor:
  - Text files open in embedded `nvim`
  - Previewable files open in a read-only preview pane

## Commands

- `:set accent #RRGGBB`
- `:set accent <alias>`
- `:set accent #RRGGBB register <alias>`
- `:get accent`
- `:set mood neon`
- `:set mood synthwave84`
- `:set mood default`
- `:get mood`
- `:set sound on`
- `:set sound off`
- `:get sound`

## Requirements

- Rust toolchain with `cargo`
- `nvim` available in `PATH`
- A Unix-like terminal environment

## Run From Source

```bash
cargo run
```

Open a specific project or file:

```bash
cargo run -- /path/to/project
```

## Install As A CLI

Install from the local checkout:

```bash
cargo install --path . --force
```

Install directly from GitHub:

```bash
cargo install --git https://github.com/VedPanse/veditor --force
```

Then run:

```bash
veditor
```

Or open a project immediately:

```bash
veditor /path/to/project
```

## Notes

- Session state is stored in `~/.veditor/session.json`.
- Global settings are stored in `~/.veditor/settings.json`.
- Hidden directories that start with `.` are omitted from the project tree and project picker.
- The editor is intentionally terminal-native: the goal is fast editing, navigation, previewing, and command execution in one process.
