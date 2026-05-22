mod app;
mod slack;
mod socket;
mod state;
mod ui;

use std::env;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::{App, AppEvent, ControlFlow, Mode};
use crate::slack::SlackContext;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let user_token = env::var("SLACK_USER_TOKEN")
        .context("SLACK_USER_TOKEN (xoxp-… or xoxc-…) must be set")?;

    let slack = Arc::new(SlackContext::connect(user_token).await?);

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    let app = App::new(slack.clone(), tx.clone());

    spawn_loaders(slack.clone(), tx.clone());
    spawn_input(tx.clone());
    spawn_ticker(tx.clone());

    if let Ok(app_token) = env::var("SOCKET_MODE_APP_TOKEN") {
        let tx_sock = tx.clone();
        tokio::spawn(async move {
            // Don't surface socket-mode startup failures — polling still works.
            let _ = socket::run(app_token, tx_sock).await;
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture).ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, app, rx).await;

    disable_raw_mode().ok();
    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal.backend_mut().execute(DisableMouseCapture).ok();
    terminal.show_cursor().ok();

    res
}

fn spawn_loaders(slack: Arc<SlackContext>, tx: mpsc::UnboundedSender<AppEvent>) {
    let s1 = slack.clone();
    let t1 = tx.clone();
    tokio::spawn(async move {
        match s1.list_users().await {
            Ok(map) => {
                let _ = t1.send(AppEvent::UsersLoaded(map));
            }
            Err(e) => {
                let _ = t1.send(AppEvent::Error(format!("users.list: {e:#}")));
            }
        }
    });
    let s2 = slack.clone();
    let t2 = tx.clone();
    tokio::spawn(async move {
        match s2.list_conversations().await {
            Ok(list) => {
                let _ = t2.send(AppEvent::ConversationsLoaded(list));
            }
            Err(e) => {
                let _ = t2.send(AppEvent::Error(format!("conversations.list: {e:#}")));
            }
        }
    });
    let s3 = slack;
    let t3 = tx;
    tokio::spawn(async move {
        // users.prefs.get is undocumented; failures are non-fatal — sorting just won't
        // exclude muted convs if the token can't access it.
        if let Ok(set) = s3.list_muted_channels().await {
            let _ = t3.send(AppEvent::MutedChannelsLoaded(set));
        }
    });
}

fn spawn_input(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || loop {
        if let Ok(true) = event::poll(Duration::from_millis(250)) {
            match event::read() {
                Ok(Event::Key(k)) => {
                    if tx.send(AppEvent::Key(k)).is_err() {
                        return;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    let _ = tx.send(AppEvent::Resize);
                }
                _ => {}
            }
        }
    });
}

fn spawn_ticker(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        loop {
            interval.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                return;
            }
        }
    });
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut rx: mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    terminal.draw(|f| ui::draw(f, &mut app))?;

    while let Some(ev) = rx.recv().await {
        if dispatch(&mut app, ev) {
            return Ok(());
        }
        while let Ok(ev) = rx.try_recv() {
            if dispatch(&mut app, ev) {
                return Ok(());
            }
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
    }
    Ok(())
}

fn dispatch(app: &mut App, ev: AppEvent) -> bool {
    match ev {
        AppEvent::Key(k) => matches!(handle_key(app, k), ControlFlow::Quit),
        other => matches!(app.on_event(other), ControlFlow::Quit),
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> ControlFlow {
    if key.kind != KeyEventKind::Press {
        return ControlFlow::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return ControlFlow::Quit;
    }

    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Insert => handle_insert(app, key),
        Mode::InputNormal => handle_input_normal(app, key),
        Mode::SidebarSearch => handle_sidebar_search(app, key),
        Mode::ChatSearch => handle_chat_search(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) -> ControlFlow {
    let was_pending_g = app.pending_g;
    let was_pending_z = app.pending_z;
    let was_pending_d = app.pending_d;
    app.pending_g = false;
    app.pending_z = false;
    app.pending_d = false;

    match key.code {
        KeyCode::Char('q') => return ControlFlow::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') => {
            if was_pending_g {
                app.jump_first_conv();
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => app.jump_last_conv(),
        KeyCode::Char('z') => {
            if was_pending_z {
                app.center_selection();
            } else {
                app.pending_z = true;
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.jump_section(1);
        }
        KeyCode::Char('d') => {
            if was_pending_d {
                app.hide_selected();
            } else {
                app.pending_d = true;
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.jump_section(-1);
        }
        KeyCode::PageUp => {
            app.message_scroll = app.message_scroll.saturating_add(10);
        }
        KeyCode::PageDown => {
            app.message_scroll = app.message_scroll.saturating_sub(10);
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = Mode::Insert;
        }
        KeyCode::Char('/') => app.enter_sidebar_search(),
        KeyCode::Char('?') => app.enter_chat_search(),
        KeyCode::Char('n') => app.next_chat_match(true),
        KeyCode::Char('N') => app.next_chat_match(false),
        KeyCode::Char('r') => app.poll_selected(),
        KeyCode::Esc => {
            app.sidebar_query.clear();
            app.chat_query.clear();
            app.chat_matches.clear();
        }
        _ => {}
    }
    ControlFlow::Continue
}

fn handle_insert(app: &mut App, key: KeyEvent) -> ControlFlow {
    let popup_open = app.mention_popup.is_some();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Mention-popup navigation takes priority over normal insert keys.
    if popup_open {
        match key.code {
            KeyCode::Char('j') if ctrl => {
                app.mention_move(1);
                return ControlFlow::Continue;
            }
            KeyCode::Char('k') if ctrl => {
                app.mention_move(-1);
                return ControlFlow::Continue;
            }
            KeyCode::Down => {
                app.mention_move(1);
                return ControlFlow::Continue;
            }
            KeyCode::Up => {
                app.mention_move(-1);
                return ControlFlow::Continue;
            }
            KeyCode::Tab => {
                app.accept_mention();
                return ControlFlow::Continue;
            }
            KeyCode::Enter => {
                // While the popup is open, Enter accepts the selected match
                // rather than sending. Press Esc to dismiss the popup if you
                // wanted to send literal "@text" instead.
                app.accept_mention();
                return ControlFlow::Continue;
            }
            KeyCode::Esc => {
                app.close_mention_popup();
                return ControlFlow::Continue;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            app.close_mention_popup();
            if app.input.is_empty() {
                app.mode = Mode::Normal;
            } else {
                app.input_clamp_for_normal();
                app.mode = Mode::InputNormal;
            }
        }
        KeyCode::Enter => {
            let _ = app.submit_input();
        }
        KeyCode::Backspace => {
            app.input_backspace();
            if popup_open {
                app.update_mention_query();
            }
        }
        KeyCode::Left => app.input_cursor_left(),
        KeyCode::Right => app.input_cursor_right(),
        KeyCode::Home => app.input_cursor_line_start(),
        KeyCode::End => app.input_cursor_line_end(),
        KeyCode::Char('j') if ctrl => {
            app.input_insert_char('\n');
            app.close_mention_popup();
        }
        KeyCode::Char('u') if ctrl => {
            app.input_clear();
            app.close_mention_popup();
        }
        KeyCode::Char('w') if ctrl => {
            app.input_delete_word_back();
            app.close_mention_popup();
        }
        KeyCode::Char(c) => {
            app.input_insert_char(c);
            if popup_open {
                app.update_mention_query();
            } else if c == '@' || c == '#' {
                app.maybe_open_mention(c);
            }
        }
        _ => {}
    }
    ControlFlow::Continue
}

fn handle_input_normal(app: &mut App, key: KeyEvent) -> ControlFlow {
    let was_pending_d = app.pending_input_d;
    let was_pending_di = app.pending_input_di;
    let was_pending_c = app.pending_input_c;
    let was_pending_ci = app.pending_input_ci;
    app.pending_input_d = false;
    app.pending_input_di = false;
    app.pending_input_c = false;
    app.pending_input_ci = false;

    match key.code {
        KeyCode::Esc => {
            if app.input.is_empty() {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char('h') | KeyCode::Left => app.input_cursor_left(),
        KeyCode::Char('l') | KeyCode::Right => app.input_cursor_right(),
        KeyCode::Char('j') | KeyCode::Down => app.input_cursor_down(),
        KeyCode::Char('k') | KeyCode::Up => app.input_cursor_up(),
        KeyCode::Char('0') | KeyCode::Home => app.input_cursor_line_start(),
        KeyCode::Char('$') | KeyCode::End => app.input_cursor_line_end(),
        KeyCode::Char('b') => app.input_word_back(),
        KeyCode::Char('w') => {
            if was_pending_di {
                app.input_delete_inner_word();
            } else if was_pending_ci {
                app.input_delete_inner_word();
                app.mode = Mode::Insert;
            } else {
                app.input_word_forward();
            }
        }
        KeyCode::Char('i') => {
            if was_pending_d {
                app.pending_input_d = false;
                app.pending_input_di = true;
            } else if was_pending_c {
                app.pending_input_c = false;
                app.pending_input_ci = true;
            } else {
                app.mode = Mode::Insert;
            }
        }
        KeyCode::Char('a') => {
            app.input_cursor_append();
            app.mode = Mode::Insert;
        }
        KeyCode::Char('d') => {
            if was_pending_d {
                app.input_delete_line();
            } else {
                app.pending_input_d = true;
            }
        }
        KeyCode::Char('c') => {
            app.pending_input_c = true;
        }
        _ => {}
    }
    ControlFlow::Continue
}

fn handle_sidebar_search(app: &mut App, key: KeyEvent) -> ControlFlow {
    match key.code {
        KeyCode::Esc => app.cancel_search(),
        KeyCode::Enter => app.accept_search(),
        KeyCode::Backspace => app.sidebar_search_backspace(),
        KeyCode::Down | KeyCode::Char('j')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_selection(1);
        }
        KeyCode::Up | KeyCode::Char('k')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_selection(-1);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.sidebar_query.clear();
        }
        KeyCode::Char(c) => app.sidebar_search_input(c),
        _ => {}
    }
    ControlFlow::Continue
}

fn handle_chat_search(app: &mut App, key: KeyEvent) -> ControlFlow {
    match key.code {
        KeyCode::Esc => app.cancel_search(),
        KeyCode::Enter => app.accept_search(),
        KeyCode::Backspace => app.chat_search_backspace(),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat_query.clear();
        }
        KeyCode::Char(c) => app.chat_search_input(c),
        _ => {}
    }
    ControlFlow::Continue
}
