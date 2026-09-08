use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ConnStatus};
use crate::irc::ChatMessage;

const NAME_PALETTE: [(u8, u8, u8); 15] = [
    (255, 0, 0),
    (0, 0, 255),
    (0, 255, 0),
    (178, 34, 34),
    (255, 127, 80),
    (154, 205, 50),
    (255, 69, 0),
    (46, 139, 87),
    (218, 165, 32),
    (210, 105, 30),
    (95, 158, 160),
    (30, 144, 255),
    (255, 105, 180),
    (138, 43, 226),
    (0, 255, 127),
];

struct Token {
    text: String,
    style: Style,
}

fn perceived_brightness(r: u8, g: u8, b: u8) -> u32 {
    (299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000
}

fn name_color(name: &str) -> (u8, u8, u8) {
    let hash = name.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    NAME_PALETTE[(hash as usize) % NAME_PALETTE.len()]
}

const MOD_HIGHLIGHT_BG: Color = Color::Rgb(20, 60, 30);
const SUB_HIGHLIGHT_BG: Color = Color::Rgb(55, 25, 65);

fn is_mod(msg: &ChatMessage) -> bool {
    msg.badges.iter().any(|b| b == "MOD" || b == "BROADCASTER")
}

fn is_sub(msg: &ChatMessage) -> bool {
    msg.badges.iter().any(|b| b == "SUB")
}

fn highlight_bg(msg: &ChatMessage, app: &App) -> Option<Color> {
    if app.highlight_mods && is_mod(msg) {
        Some(MOD_HIGHLIGHT_BG)
    } else if app.highlight_subs && is_sub(msg) {
        Some(SUB_HIGHLIGHT_BG)
    } else {
        None
    }
}

fn build_tokens(msg: &ChatMessage, bg: Option<Color>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let apply_bg = |style: Style| match bg {
        Some(color) => style.bg(color),
        None => style,
    };

    for badge in &msg.badges {
        tokens.push(Token {
            text: format!("[{badge}]"),
            style: apply_bg(Style::default().fg(Color::DarkGray)),
        });
    }

    let (r, g, b) = msg
        .color
        .filter(|&(r, g, b)| perceived_brightness(r, g, b) >= 60)
        .unwrap_or_else(|| name_color(&msg.display_name));
    tokens.push(Token {
        text: format!("{}:", msg.display_name),
        style: apply_bg(Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD)),
    });

    for word in msg.text.split_whitespace() {
        tokens.push(Token {
            text: word.to_string(),
            style: apply_bg(Style::default().fg(Color::White)),
        });
    }

    tokens
}

fn hard_break(token: Token, width: usize) -> Vec<Token> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut remaining: &str = &token.text;
    while remaining.width() > width {
        let mut take_len = 0;
        let mut take_width = 0;
        for c in remaining.chars() {
            let cw = UnicodeWidthStr::width(c.to_string().as_str());
            if take_width + cw > width {
                break;
            }
            take_width += cw;
            take_len += c.len_utf8();
        }
        if take_len == 0 {
            take_len = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(remaining.len());
        }
        let (chunk, rest) = remaining.split_at(take_len);
        out.push(Token {
            text: chunk.to_string(),
            style: token.style,
        });
        remaining = rest;
    }
    out.push(Token {
        text: remaining.to_string(),
        style: token.style,
    });
    out
}

fn wrap_tokens(tokens: Vec<Token>, width: usize) -> Vec<Vec<Token>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Token>> = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut current_width = 0usize;

    for token in tokens {
        let tw = token.text.width();
        if tw > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let mut pieces = hard_break(token, width);
            let last = pieces.pop();
            for piece in pieces {
                let pw = piece.text.width();
                lines.push(vec![piece]);
                let _ = pw;
            }
            if let Some(last) = last {
                current_width = last.text.width();
                current.push(last);
            }
            continue;
        }

        let sep = if current.is_empty() { 0 } else { 1 };
        if current_width + sep + tw <= width {
            current_width += sep + tw;
            current.push(token);
        } else {
            lines.push(std::mem::take(&mut current));
            current_width = tw;
            current.push(token);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn message_to_lines(msg: &ChatMessage, width: usize, bg: Option<Color>) -> Vec<Line<'static>> {
    let tokens = build_tokens(msg, bg);
    let wrapped = wrap_tokens(tokens, width);
    let space_style = match bg {
        Some(color) => Style::default().bg(color),
        None => Style::default(),
    };
    wrapped
        .into_iter()
        .map(|line_tokens| {
            let mut spans = Vec::new();
            for (i, tok) in line_tokens.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ", space_style));
                }
                spans.push(Span::styled(tok.text, tok.style));
            }
            Line::from(spans)
        })
        .collect()
}

fn status_line(app: &App) -> Line<'static> {
    let (dot_color, text) = match &app.status {
        ConnStatus::Connecting => (Color::Yellow, "connecting...".to_string()),
        ConnStatus::Connected => (Color::Green, "connected".to_string()),
        ConnStatus::Disconnected(reason) => (Color::Red, format!("disconnected: {reason}")),
    };
    Line::from(vec![
        Span::styled("#", Style::default().fg(Color::Gray)),
        Span::styled(
            app.channel.clone(),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("●", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::raw(text),
    ])
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    frame.render_widget(Paragraph::new(status_line(app)), chunks[0]);

    let effective_filter = app.effective_filter().to_string();
    let title = if effective_filter.is_empty() {
        " chat ".to_string()
    } else {
        format!(" chat · filter: {effective_filter} ")
    };
    let chat_block = Block::default().borders(Borders::ALL).title(title);
    let inner: Rect = chat_block.inner(chunks[1]);
    frame.render_widget(chat_block, chunks[1]);

    let width = inner.width as usize;
    let viewport = inner.height as usize;

    let matched: Vec<&ChatMessage> = app.messages.iter().filter(|m| app.message_matches(m)).collect();

    let mut all_lines: Vec<Line<'static>> = Vec::new();
    if matched.is_empty() && !effective_filter.is_empty() {
        all_lines.push(Line::from(Span::styled(
            format!("no messages match \"{effective_filter}\""),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for msg in &matched {
            all_lines.extend(message_to_lines(msg, width, highlight_bg(msg, app)));
        }
    }

    app.last_total_lines = all_lines.len();
    app.last_viewport = viewport;
    let max_skip = app.last_total_lines.saturating_sub(viewport);
    if app.pinned_to_bottom {
        app.skip = max_skip;
    } else {
        app.skip = app.skip.min(max_skip);
    }

    let text = Text::from(all_lines);
    let paragraph = Paragraph::new(text).scroll((app.skip as u16, 0));
    frame.render_widget(paragraph, inner);

    let footer = if let Some(buf) = &app.filter_input {
        Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(buf.clone()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
            Span::styled(
                "   Enter apply · Esc cancel · Ctrl+U clear",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        let count = if app.filter.is_empty() {
            format!("{} messages  ", app.messages.len())
        } else {
            format!("{}/{} messages  ", matched.len(), app.messages.len())
        };
        let mods_style = if app.highlight_mods {
            Style::default().fg(Color::White).bg(MOD_HIGHLIGHT_BG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let subs_style = if app.highlight_subs {
            Style::default().fg(Color::White).bg(SUB_HIGHLIGHT_BG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Line::from(vec![
            Span::raw(count),
            Span::styled(
                "/ filter   ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(" M mods ", mods_style),
            Span::raw(" "),
            Span::styled(" S subs ", subs_style),
            Span::styled(
                "   ↑/↓ PgUp/PgDn Home/End scroll   q quit",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(footer), chunks[2]);
}
