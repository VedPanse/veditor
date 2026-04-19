//! Theme and color helpers for the TUI and embedded Neovim session.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::UiTheme;

/// Builds the full UI palette from a single accent color.
pub(crate) fn ui_theme(accent_hex: &str) -> UiTheme {
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

/// Parses a `#RRGGBB` color string into a ratatui color.
pub(crate) fn parse_hex_color(value: &str) -> Option<Color> {
    let value = value.trim_start_matches('#');
    if value.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&value[0..2], 16).ok()?;
    let g = u8::from_str_radix(&value[2..4], 16).ok()?;
    let b = u8::from_str_radix(&value[4..6], 16).ok()?;

    Some(Color::Rgb(r, g, b))
}

/// Canonicalizes accent hex values so persistence uses a stable `#RRGGBB` format.
pub(crate) fn normalize_hex_color(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !trimmed.starts_with('#') {
        return None;
    }

    let digits = trimmed.trim_start_matches('#');
    if digits.len() != 6 {
        return None;
    }

    let canonical = format!("#{digits}");
    parse_hex_color(&canonical).map(color_hex)
}

/// Converts a ratatui color into lowercase `#RRGGBB`.
pub(crate) fn color_hex(color: Color) -> String {
    let color = color_to_rgb(color);
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Blends an RGBA pixel onto a solid background for image previews.
pub(crate) fn blend_rgba_to_rgb(pixel: [u8; 4], background: RgbColor) -> RgbColor {
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

/// Formats a themed accent swatch line for command output.
pub(crate) fn accent_preview_line(
    prefix: &str,
    name: &str,
    hex: &str,
    ui: UiTheme,
) -> Line<'static> {
    let accent = parse_hex_color(hex).unwrap_or(ui.accent);
    let text_color = accent_contrast_color(accent);
    Line::from(vec![
        Span::styled(
            format!("{prefix}  "),
            Style::default()
                .fg(text_color)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(text_color)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().bg(accent)),
        Span::styled(
            hex.to_string(),
            Style::default()
                .fg(text_color)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Builds the startup `+lua` command that themes the embedded Neovim pane.
pub(crate) fn build_nvim_theme_command(ui: UiTheme) -> String {
    format!("+lua {}", nvim_theme_lua(ui))
}

#[derive(Clone, Copy)]
pub(crate) struct RgbColor {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

fn accent_contrast_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let luminance = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
            if luminance >= 140.0 {
                Color::Rgb(18, 18, 18)
            } else {
                Color::Rgb(245, 245, 245)
            }
        }
        _ => Color::White,
    }
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

pub(crate) fn nvim_theme_lua(ui: UiTheme) -> String {
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
