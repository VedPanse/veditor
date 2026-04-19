//! Preview loading and rendering helpers for non-text editor content.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use image::{GenericImageView, ImageReader, imageops::FilterType};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::{
    DOCUMENT_PREVIEW_MAX_LINES, DocumentPreview, EditorPreview, ImagePreview, ImagePreviewCache,
    UiTheme, io_error,
    theme::{RgbColor, blend_rgba_to_rgb},
};

impl ImagePreview {
    /// Loads an image preview from disk and defers terminal rasterization until render time.
    pub(crate) fn load(path: PathBuf) -> io::Result<Self> {
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

    /// Returns cached terminal lines for the current image viewport.
    pub(crate) fn lines(&mut self, width: u16, height: u16, ui: UiTheme) -> &[Line<'static>] {
        let refresh = self.cache.as_ref().is_none_or(|cache| {
            cache.width != width || cache.height != height || cache.panel != ui.panel
        });
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
    /// Loads a supported document preview from disk.
    pub(crate) fn load(path: PathBuf) -> io::Result<Self> {
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

/// Loads a preview-capable editor target from disk.
pub(crate) fn load_editor_preview(path: PathBuf) -> io::Result<EditorPreview> {
    if is_image_path(&path) {
        ImagePreview::load(path).map(EditorPreview::Image)
    } else {
        DocumentPreview::load(path).map(EditorPreview::Document)
    }
}

/// Returns whether the editor target should use the preview pane instead of Neovim.
pub(crate) fn uses_editor_preview(path: &Path) -> bool {
    is_image_path(path) || is_pdf_path(path) || is_notebook_path(path)
}

/// Rasterizes an image into terminal-friendly half-block lines.
pub(crate) fn build_image_preview_lines(
    image: &image::DynamicImage,
    width: u16,
    height: u16,
    ui: UiTheme,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let target_height = usize::from(height.max(1) * 2);
    let resized = image.resize_exact(
        u32::from(width.max(1)),
        u32::try_from(target_height).unwrap_or(1),
        FilterType::Triangle,
    );

    let background = rgb_from_color(ui.panel);
    let mut lines = Vec::with_capacity(height as usize);
    for y in (0..target_height).step_by(2) {
        let mut spans = Vec::with_capacity(width as usize);
        for x in 0..usize::from(width) {
            let top = resized.get_pixel(x as u32, y as u32).0;
            let bottom = if y + 1 < target_height {
                resized.get_pixel(x as u32, (y + 1) as u32).0
            } else {
                [top[0], top[1], top[2], 0]
            };
            let fg = color_from_rgb(blend_rgba_to_rgb(top, background));
            let bg = color_from_rgb(blend_rgba_to_rgb(bottom, background));
            spans.push(Span::styled("\u{2580}", Style::default().fg(fg).bg(bg)));
        }
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(" "));
    }

    lines
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
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
}

fn is_notebook_path(path: &Path) -> bool {
    path.extension()
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
        match output
            .get("output_type")
            .and_then(serde_json::Value::as_str)
        {
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

fn rgb_from_color(color: Color) -> RgbColor {
    match color {
        Color::Rgb(r, g, b) => RgbColor { r, g, b },
        Color::Black => RgbColor { r: 0, g: 0, b: 0 },
        Color::White => RgbColor {
            r: 255,
            g: 255,
            b: 255,
        },
        Color::Gray => RgbColor {
            r: 128,
            g: 128,
            b: 128,
        },
        Color::DarkGray => RgbColor {
            r: 64,
            g: 64,
            b: 64,
        },
        Color::Red => RgbColor { r: 255, g: 0, b: 0 },
        Color::LightRed => RgbColor {
            r: 255,
            g: 102,
            b: 102,
        },
        Color::Green => RgbColor { r: 0, g: 255, b: 0 },
        Color::LightGreen => RgbColor {
            r: 144,
            g: 238,
            b: 144,
        },
        Color::Yellow => RgbColor {
            r: 255,
            g: 255,
            b: 0,
        },
        Color::LightYellow => RgbColor {
            r: 255,
            g: 255,
            b: 153,
        },
        Color::Blue => RgbColor { r: 0, g: 0, b: 255 },
        Color::LightBlue => RgbColor {
            r: 173,
            g: 216,
            b: 230,
        },
        Color::Magenta => RgbColor {
            r: 255,
            g: 0,
            b: 255,
        },
        Color::LightMagenta => RgbColor {
            r: 255,
            g: 153,
            b: 255,
        },
        Color::Cyan => RgbColor {
            r: 0,
            g: 255,
            b: 255,
        },
        Color::LightCyan => RgbColor {
            r: 153,
            g: 255,
            b: 255,
        },
        Color::Indexed(value) => RgbColor {
            r: value,
            g: value,
            b: value,
        },
        Color::Reset => RgbColor { r: 0, g: 0, b: 0 },
    }
}

fn color_from_rgb(color: RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}
