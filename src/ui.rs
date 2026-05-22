use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Mode, Section, VisibleRow};

pub fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20)])
        .split(outer[0]);

    draw_sidebar(f, app, main[0]);
    draw_chat(f, app, main[1]);
    draw_statusline(f, app, outer[1]);
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let searching = matches!(app.mode, Mode::SidebarSearch);
    let split = if searching || !app.sidebar_query.is_empty() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1)])
            .split(area)
    };

    let rows = app.flat_visible();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| match r {
            VisibleRow::Header(section, count) => {
                let label = match section {
                    Section::Unread => format!(" UNREAD ({count})"),
                    Section::Channels => format!(" CHANNELS ({count})"),
                    Section::External => format!(" EXTERNAL ({count})"),
                    Section::Groups => format!(" GROUPS ({count})"),
                    Section::Dms => format!(" DIRECT MESSAGES ({count})"),
                };
                ListItem::new(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ))
            }
            VisibleRow::Conv(i) => {
                let c = &app.convs[*i];
                let label = app.display_name(c);
                let unread = app.unread.get(&c.id).copied().unwrap_or(0);
                let label_style = if c.is_muted {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                let mut spans = vec![Span::raw(" "), Span::styled(label, label_style)];
                if unread > 0 {
                    spans.push(Span::raw(" "));
                    let badge_style = if c.is_muted {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    };
                    spans.push(Span::styled(format!("({unread})"), badge_style));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let title = format!(" {} ", app.slack.team);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

    let list_area = split[0];
    let inner_h = Block::default().borders(Borders::ALL).inner(list_area).height;
    app.sidebar_view_h = inner_h;
    f.render_stateful_widget(list, list_area, &mut app.sidebar_state);

    if searching || !app.sidebar_query.is_empty() {
        let style = if searching {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let prompt = format!("/{}", app.sidebar_query);
        let p = Paragraph::new(prompt).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" filter "),
        );
        f.render_widget(p, split[1]);
        if searching {
            let x = split[1].x + 2 + app.sidebar_query.chars().count() as u16;
            let y = split[1].y + 1;
            f.set_cursor_position((x, y));
        }
    }
}

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let input_h = input_area_height(app, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(input_h)])
        .split(area);

    let (title, body_lines) = match app.selected_conv() {
        Some(c) => {
            let mut title = format!(" {} ", app.display_name(c));
            if !app.chat_query.is_empty() {
                title.push_str(&format!("· match {}/{} for '{}' ",
                    app.chat_matches.iter().position(|i| Some(*i) == app.current_match_index())
                        .map(|i| i + 1).unwrap_or(0),
                    app.chat_matches.len(),
                    app.chat_query,
                ));
            }
            let lines = match app.messages.get(&c.id) {
                Some(msgs) => render_messages(app, msgs),
                None if app.loading.contains(&c.id) => vec![Line::from(Span::styled(
                    "loading…",
                    Style::default().fg(Color::DarkGray),
                ))],
                None => vec![Line::from(Span::styled(
                    "no messages",
                    Style::default().fg(Color::DarkGray),
                ))],
            };
            (title, lines)
        }
        None => (
            " (no channel) ".to_string(),
            vec![Line::from("no matches — Esc to clear, type to filter")],
        ),
    };

    let body_block = Block::default().borders(Borders::ALL).title(title);
    let inner = body_block.inner(chunks[0]);
    f.render_widget(body_block, chunks[0]);

    let body = Paragraph::new(body_lines).wrap(Wrap { trim: false });
    // Ask the widget itself how many rows it'll wrap to — our previous
    // char-count estimate undercounted because ratatui breaks at word
    // boundaries, which can push lines past a naive `ceil(chars / width)`
    // count and clip the most-recent message off the bottom.
    let total = body
        .line_count(inner.width.max(1))
        .min(u16::MAX as usize) as u16;
    let visible_h = inner.height;
    let max_scroll = total.saturating_sub(visible_h);
    let scroll = app.message_scroll.min(max_scroll);
    let effective_scroll = max_scroll.saturating_sub(scroll);

    f.render_widget(body.scroll((effective_scroll, 0)), inner);

    draw_input_or_search(f, app, chunks[1]);
    draw_mention_popup(f, app, chunks[0], chunks[1]);
}

fn draw_mention_popup(f: &mut Frame, app: &App, body_area: Rect, input_area: Rect) {
    let Some(popup) = app.mention_popup.as_ref() else {
        return;
    };
    if popup.matches.is_empty() {
        return;
    }
    // Sit the popup directly above the input, anchored to the input's left edge.
    let max_rows = popup.matches.len().min(8) as u16;
    let height = max_rows + 2; // borders
    let max_label = popup
        .matches
        .iter()
        .map(|m| m.display.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let width = (max_label + 4).max(20).min(body_area.width.max(4));
    let x = input_area.x;
    let y = input_area.y.saturating_sub(height);
    // Clamp to the body area so we don't try to draw outside it.
    let popup_area = Rect {
        x,
        y: y.max(body_area.y),
        width: width.min(body_area.x + body_area.width - x),
        height: height.min(input_area.y - body_area.y),
    };
    if popup_area.height < 3 || popup_area.width < 4 {
        return;
    }

    let items: Vec<ListItem> = popup
        .matches
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if i == popup.selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(format!(" {} ", m.display), style))
        })
        .collect();

    let title = match popup.kind {
        crate::app::MentionKind::User => " mention (Ctrl+J/K · Tab/Enter · Esc) ",
        crate::app::MentionKind::Channel => " channel (Ctrl+J/K · Tab/Enter · Esc) ",
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title),
    );

    f.render_widget(Clear, popup_area);
    f.render_widget(list, popup_area);
}

// Width available inside the input box for text content.
fn input_inner_width(area: Rect) -> u16 {
    area.width.saturating_sub(2).max(1)
}

// Lay out the input across enough rows to show every character of `text`.
// Wraps on a per-character basis so cursor placement stays accurate — the
// stock Paragraph word-wrap would push partial words to the next row and
// our column math would no longer line up with what's rendered.
fn wrap_input(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for c in text.chars() {
        if c == '\n' {
            lines.push(std::mem::take(&mut current));
            count = 0;
            continue;
        }
        if count == width {
            lines.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(c);
        count += 1;
    }
    lines.push(current);
    lines
}

// Pick an input-box height that fits the current text. Caps at half the
// chat pane so a long draft can't squeeze the message list to nothing.
fn input_area_height(app: &App, chat_area: Rect) -> u16 {
    let text: &str = match app.mode {
        Mode::ChatSearch => app.chat_query.as_str(),
        _ => app.input.as_str(),
    };
    // ChatSearch prompts with a leading "?" so reserve a column for it.
    let prefix = if matches!(app.mode, Mode::ChatSearch) { 1 } else { 0 };
    // Build a fake string of `prefix` placeholders + actual text just to get
    // the wrapped row count right.
    let probe = "?".repeat(prefix) + text;
    let inner_w = input_inner_width(Rect { x: 0, y: 0, width: chat_area.width, height: 0 });
    let lines = wrap_input(&probe, inner_w).len().max(1) as u16;
    let max = (chat_area.height / 2).max(3);
    (lines + 2).clamp(3, max)
}

fn draw_input_or_search(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = input_inner_width(area);
    match app.mode {
        Mode::ChatSearch => {
            let prompt = format!("?{}", app.chat_query);
            let lines = wrap_input(&prompt, inner_w);
            let cursor_chars = 1 + app.chat_query.chars().count();
            let (cy, cx) = cursor_rowcol(cursor_chars, inner_w);
            let body = lines.join("\n");
            let p = Paragraph::new(body)
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" search messages (Enter · Esc · n/N) "),
                );
            f.render_widget(p, area);
            f.set_cursor_position((area.x + 1 + cx, area.y + 1 + cy));
        }
        Mode::Insert => {
            let lines = wrap_input(&app.input, inner_w);
            let (cy, cx) = cursor_rowcol_at(&app.input, app.input_cursor, inner_w);
            let body = lines.join("\n");
            let input = Paragraph::new(body)
                .style(Style::default().fg(Color::White))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" message (Enter send · Ctrl-J newline · Esc) "),
                );
            f.render_widget(input, area);
            f.set_cursor_position((area.x + 1 + cx, area.y + 1 + cy));
        }
        Mode::InputNormal => {
            let lines = wrap_input(&app.input, inner_w);
            let (cy, cx) = cursor_rowcol_at(&app.input, app.input_cursor, inner_w);
            let body = lines.join("\n");
            let input = Paragraph::new(body)
                .style(Style::default().fg(Color::White))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" message [NORMAL] (hjkl · w/b · 0/$ · i/a · dd · diw · ciw) "),
                );
            f.render_widget(input, area);
            f.set_cursor_position((area.x + 1 + cx, area.y + 1 + cy));
        }
        _ => {
            let lines = wrap_input(&app.input, inner_w);
            let body = lines.join("\n");
            let p = Paragraph::new(body)
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" i write · / find conv · ? find msg "),
                );
            f.render_widget(p, area);
        }
    }
}

// Row/column the cursor lands on after `chars_before` characters, given the
// same per-char wrapping `wrap_input` performs. When the cursor sits exactly
// at the wrap boundary we push it onto the next row so typing the next char
// doesn't visually jump.
fn cursor_rowcol(chars_before: usize, width: u16) -> (u16, u16) {
    let w = width.max(1) as usize;
    let row = chars_before / w;
    let col = chars_before % w;
    (row as u16, col as u16)
}

// Cursor position for text that may contain literal newlines (Ctrl-J inserts).
// Walks `text` up to `byte_pos` the same way `wrap_input` does so the caret
// stays aligned with what's actually rendered.
fn cursor_rowcol_at(text: &str, byte_pos: usize, width: u16) -> (u16, u16) {
    let w = width.max(1) as usize;
    let mut row: u16 = 0;
    let mut col: usize = 0;
    let limit = byte_pos.min(text.len());
    for (i, c) in text.char_indices() {
        if i >= limit {
            break;
        }
        if c == '\n' {
            row = row.saturating_add(1);
            col = 0;
            continue;
        }
        if col == w {
            row = row.saturating_add(1);
            col = 0;
        }
        col += 1;
    }
    (row, col as u16)
}

fn render_messages(app: &App, msgs: &[crate::slack::Msg]) -> Vec<Line<'static>> {
    use std::collections::{HashMap, HashSet};
    let q = app.chat_query.to_lowercase();
    let current = app.current_match_index();
    let mut out = Vec::new();
    let mut last_author: Option<String> = None;
    let mut last_time: Option<chrono::DateTime<chrono::Local>> = None;
    const REHEADER_GAP: chrono::Duration = chrono::Duration::minutes(20);

    // Group thread replies under their parents so a chronological reply that
    // arrived hours after the parent still reads as part of the same thread.
    // Orphan replies (parent not in `msgs`) render in place, indented, so the
    // user at least sees the reply with the thread marker.
    let parent_idx: HashMap<&str, usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.is_thread_reply())
        .map(|(i, m)| (m.ts.0.as_str(), i))
        .collect();
    let mut order: Vec<(usize, bool)> = Vec::with_capacity(msgs.len());
    let mut placed: HashSet<usize> = HashSet::new();
    for (i, m) in msgs.iter().enumerate() {
        if m.is_thread_reply() {
            let parent_present = m
                .thread_ts
                .as_ref()
                .map(|p| parent_idx.contains_key(p.0.as_str()))
                .unwrap_or(false);
            if parent_present {
                continue; // emitted under its parent below
            }
            if placed.insert(i) {
                order.push((i, true));
            }
        } else {
            if placed.insert(i) {
                order.push((i, false));
            }
            for (j, r) in msgs.iter().enumerate() {
                if placed.contains(&j) {
                    continue;
                }
                if let Some(p) = &r.thread_ts {
                    if p.0 == m.ts.0 {
                        order.push((j, true));
                        placed.insert(j);
                    }
                }
            }
        }
    }

    for &(idx, in_thread) in &order {
        let m = &msgs[idx];
        let is_match_line = !q.is_empty() && m.text.to_lowercase().contains(&q);
        let is_current = Some(idx) == current;
        let body_indent = if in_thread { "    ↳ " } else { "  " };
        let cont_indent = if in_thread { "      " } else { "  " };
        if m.subtype.is_some() {
            let body = if !m.text.is_empty() {
                m.text.clone()
            } else if let Some(sub) = &m.subtype {
                format!("({})", subtype_label(sub))
            } else {
                "(event)".to_string()
            };
            let body = format_slack_text(&body, app);
            out.push(Line::from(Span::styled(
                format!("{body_indent}· {body}"),
                Style::default().fg(Color::DarkGray),
            )));
            last_author = None;
            last_time = None;
            continue;
        }
        let author = app.author_label(m);
        let local = m.local_time();
        let time = local
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "??:??".to_string());
        let author_color = if app.is_bot_author(m) {
            Color::Yellow
        } else {
            Color::Cyan
        };
        let author_changed = Some(&author) != last_author.as_ref();
        let gap_exceeded = match (last_time, local) {
            (Some(prev), Some(curr)) => curr.signed_duration_since(prev) >= REHEADER_GAP,
            _ => false,
        };
        // Always re-print the header for thread replies — keeps the reply
        // attribution obvious even when the parent and reply happen to share
        // an author.
        if author_changed || gap_exceeded || in_thread {
            let header_indent = if in_thread { "    ↳ " } else { "" };
            out.push(Line::from(vec![
                Span::styled(
                    format!("{header_indent}{author}  "),
                    Style::default()
                        .fg(author_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(time, Style::default().fg(Color::DarkGray)),
            ]));
            last_author = Some(author);
        }
        if let Some(t) = local {
            last_time = Some(t);
        }
        let text = format_slack_text(&m.text, app);
        let mut first = true;
        for line in text.lines() {
            let style = if is_current {
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if is_match_line {
                Style::default().fg(Color::Yellow)
            } else if m.pending {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
            };
            // Body sits under the (re-printed) header, so always use cont_indent.
            let _ = first;
            first = false;
            out.push(Line::from(Span::styled(format!("{cont_indent}{line}"), style)));
        }
        if text.is_empty() {
            out.push(Line::from(cont_indent.to_string()));
        }
    }
    out
}

fn subtype_label(sub: &str) -> &str {
    match sub {
        "bot_message" => "bot message",
        "channel_join" => "joined",
        "channel_leave" => "left",
        "channel_topic" => "topic changed",
        "channel_purpose" => "purpose changed",
        "channel_name" => "renamed",
        "file_share" => "file",
        "message_changed" => "edited",
        "message_deleted" => "deleted",
        "thread_broadcast" => "thread",
        other => other,
    }
}

fn format_slack_text(text: &str, app: &App) -> String {
    // Slack message text needs three passes before it's terminal-friendly:
    // 1. expand `<…>` mention/link tokens to the human-readable form,
    // 2. decode HTML entities Slack escapes into the body (&amp; / &lt; / …),
    // 3. swap Slack's list-marker glyphs for ASCII so they read cleanly even
    //    in fonts without good Unicode coverage.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('>') {
            out.push_str(&resolve_token(&after[..end], app));
            rest = &after[end + 1..];
        } else {
            out.push('<');
            rest = after;
        }
    }
    out.push_str(rest);
    let out = decode_entities(&out);
    asciify_bullets(&out)
}

fn resolve_token(token: &str, app: &App) -> String {
    let (body, label) = match token.split_once('|') {
        Some((b, l)) => (b, Some(l)),
        None => (token, None),
    };
    if let Some(rest) = body.strip_prefix('@') {
        let uid = slack_morphism::SlackUserId(rest.to_string());
        if let Some(u) = app.users.get(&uid) {
            return format!("@{}", u.display);
        }
        return format!("@{}", label.unwrap_or(rest));
    }
    if let Some(rest) = body.strip_prefix('#') {
        let cid = slack_morphism::SlackChannelId(rest.to_string());
        if let Some(c) = app.convs.iter().find(|c| c.id == cid) {
            return format!("#{}", c.name);
        }
        return format!("#{}", label.unwrap_or(rest));
    }
    // Slack broadcast tokens: <!here>, <!channel>, <!everyone>, <!subteam^ID|name>
    if let Some(rest) = body.strip_prefix('!') {
        let name = label.unwrap_or_else(|| rest.split('^').next().unwrap_or(rest));
        return format!("@{name}");
    }
    // Slack always provides a human label for URLs/mailto/tel when one exists;
    // prefer it. Otherwise strip the URI scheme so plain emails/phones read
    // naturally (mailto:foo@bar.com → foo@bar.com).
    if let Some(l) = label {
        return l.to_string();
    }
    if let Some(addr) = body.strip_prefix("mailto:") {
        return addr.to_string();
    }
    if let Some(num) = body.strip_prefix("tel:") {
        return num.to_string();
    }
    body.to_string()
}

fn decode_entities(s: &str) -> String {
    // Slack escapes &, <, > in message text — these are the only entities the
    // API documents emitting, but tolerate a couple of common extras too.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        let matched = [
            ("&amp;", "&"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&#x27;", "'"),
            ("&nbsp;", " "),
        ]
        .iter()
        .find(|(needle, _)| tail.starts_with(needle))
        .copied();
        if let Some((needle, replacement)) = matched {
            out.push_str(replacement);
            rest = &tail[needle.len()..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

fn asciify_bullets(s: &str) -> String {
    // Slack renders rich_text_list items into the plain `text` field as Unicode
    // bullets keyed by indent level. Map them to ASCII so the chat reads the
    // same on every terminal/font without relying on Unicode glyph fidelity.
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2022}' => out.push('*'),                  // •
            '\u{25E6}' | '\u{25AA}' | '\u{25AB}' => out.push('-'), // ◦ ▪ ▫
            '\u{25B8}' | '\u{25B6}' | '\u{25B9}' => out.push('>'), // ▸ ▶ ▹
            '\u{2013}' | '\u{2014}' => out.push('-'),     // – —
            '\u{2018}' | '\u{2019}' => out.push('\''),    // ‘ ’
            '\u{201C}' | '\u{201D}' => out.push('"'),     // “ ”
            '\u{2026}' => out.push_str("..."),            // …
            '\u{00A0}' => out.push(' '),                  // NBSP
            other => out.push(other),
        }
    }
    out
}

fn draw_statusline(f: &mut Frame, app: &App, area: Rect) {
    let (mode_label, mode_style) = match app.mode {
        Mode::Normal => (
            " NORMAL ",
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        Mode::Insert => (
            " INSERT ",
            Style::default().bg(Color::Green).fg(Color::Black),
        ),
        Mode::InputNormal => (
            " N-MSG  ",
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Mode::SidebarSearch => (
            " /CONV  ",
            Style::default().bg(Color::Yellow).fg(Color::Black),
        ),
        Mode::ChatSearch => (
            " ?MSG   ",
            Style::default().bg(Color::Yellow).fg(Color::Black),
        ),
    };
    let mut spans = vec![
        Span::styled(mode_label, mode_style.add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    if let Some(err) = app.error_text() {
        spans.push(Span::styled(
            format!("⚠ {err}"),
            Style::default().fg(Color::Red),
        ));
    } else {
        spans.push(Span::styled(
            &app.status,
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        "j/k · gg/G · i · / ? n N · q",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
