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

fn build_tokens(msg: &ChatMessage) -> Vec<Token> {
    let mut tokens = Vec::new();

    for badge in &msg.badges {
        tokens.push(Token {
            text: format!("[{badge}]"),
            style: Style::default().fg(Color::DarkGray),
        });
    }

    let (r, g, b) = msg
        .color
        .filter(|&(r, g, b)| perceived_brightness(r, g, b) >= 60)
        .unwrap_or_else(|| name_color(&msg.display_name));
    tokens.push(Token {
        text: format!("{}:", msg.display_name),
        style: Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD),
    });

    for word in msg.text.split_whitespace() {
        tokens.push(Token {
            text: word.to_string(),
            style: Style::default().fg(Color::White),
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

fn message_to_lines(msg: &ChatMessage, width: usize) -> Vec<Line<'static>> {
    let tokens = build_tokens(msg);
    let wrapped = wrap_tokens(tokens, width);
    wrapped
        .into_iter()
        .map(|line_tokens| {
            let mut spans = Vec::new();
            for (i, tok) in line_tokens.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
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

    let chat_block = Block::default().borders(Borders::ALL).title(" chat ");
    let inner: Rect = chat_block.inner(chunks[1]);
    frame.render_widget(chat_block, chunks[1]);

    let width = inner.width as usize;
    let viewport = inner.height as usize;

    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for msg in &app.messages {
        all_lines.extend(message_to_lines(msg, width));
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

    let footer = Line::from(vec![
        Span::raw(format!("{} messages  ", app.messages.len())),
        Span::styled(
            "↑/↓ PgUp/PgDn Home/End scroll   q quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[2]);
}
