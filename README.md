# veditor

`veditor` is a keyboard-first terminal code editor built in Rust with `ratatui`, an embedded `nvim` pane, a project tree, an integrated shell, and a minimal Codex prompt area.

## Features

- Embedded `nvim` for real modal editing instead of a custom text buffer.
- Embedded terminal pane for running project commands without leaving the editor.
- Project tree with keyboard navigation, file and directory creation, and project switching.
- Session restore for the last project root, open files, and active file.
- Image preview support for common formats such as `png`, `jpg`, `jpeg`, `gif`, `webp`, and `bmp`.
- Accent-driven theming across the dashboard and embedded editor surface.

## Controls

- `Ctrl-W`: cycle focus between panes.
- Project tree:
  - `Up` / `Down`: move selection
  - `Enter`: open file or expand directory
  - `%`: create file in project root
  - `Ctrl-D`: create directory in project root
  - `Ctrl-N`: create file in selected directory
  - `Ctrl-Shift-N`: create directory in selected directory
  - `Cmd-O` on macOS / `Ctrl-O` elsewhere: open another project
- Editor:
  - text files open in embedded `nvim`
  - image files open in a read-only preview pane

## Running

```bash
cargo run
```

## Notes

- Session state is stored in `~/.veditor/session.json`.
- Hidden directories that start with `.` are omitted from the project tree and project picker.
- The current Codex pane is intentionally minimal and only renders an input field.
