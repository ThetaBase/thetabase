//! The terminal half: raw mode, the event loop, and drawing.
//!
//! Everything that decides anything lives in [`super::app`] and
//! [`super::render`], which have no terminal in them. What is left here is
//! plumbing, and it is deliberately thin — the rules that carry the safety
//! claim should not be reachable only by driving a terminal.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::app::{Action, Key, Mode, ReviewApp};
use super::render;

/// How often the queue is re-read while somebody is watching.
///
/// Five seconds: fast enough that a change proposed by an agent appears while
/// the reviewer is still at the screen, slow enough not to matter. Stops
/// entirely once the screen goes dormant — see `ReviewApp::should_poll`.
const POLL: Duration = Duration::from_secs(5);

/// How long the loop waits for a keypress before doing its housekeeping.
const TICK: Duration = Duration::from_millis(250);

/// Restores the terminal however the loop ends.
///
/// A panic inside the loop would otherwise leave somebody in raw mode with no
/// echo and no cursor, in a shell that appears to have stopped working. `Drop`
/// runs during an unwind; a tidy-up at the end of `run` does not.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Run the review screen until the reviewer quits.
///
/// `fetch` re-reads the queue and `act` carries out a decision. Both are passed
/// in rather than called directly so this loop has no opinion about transport —
/// and so it can be driven in a test without a socket.
pub fn run<F, A>(project: &str, mut app: ReviewApp, mut fetch: F, mut act: A) -> Result<(), String>
where
    F: FnMut() -> Result<Vec<theta_proto::wire::ReviewBatchWire>, String>,
    A: FnMut(Action) -> Result<String, String>,
{
    enable_raw_mode().map_err(|e| format!("cannot enter raw mode: {e}"))?;
    io::stdout()
        .execute(EnterAlternateScreen)
        .map_err(|e| format!("cannot open the review screen: {e}"))?;
    let _restore = Restore;

    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .map_err(|e| format!("cannot draw: {e}"))?;

    let mut last_poll = Instant::now();

    loop {
        terminal
            .draw(|frame| draw(frame, &app, project))
            .map_err(|e| format!("cannot draw: {e}"))?;

        if event::poll(TICK).map_err(|e| e.to_string())? {
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                // Key *presses* only. A terminal that reports releases would
                // otherwise deliver every keystroke twice, and on a screen
                // where one keystroke confirms a change that is not a cosmetic
                // problem.
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                let Some(translated) = translate(key.code) else {
                    continue;
                };

                match app.on_key(translated) {
                    Some(Action::Quit) => return Ok(()),
                    Some(Action::Refresh) => {
                        refresh(&mut app, &mut fetch);
                        last_poll = Instant::now();
                    }
                    Some(action) => match act(action) {
                        Ok(message) => {
                            app.report(message);
                            refresh(&mut app, &mut fetch);
                            last_poll = Instant::now();
                        }
                        Err(e) => app.report(e),
                    },
                    None => {}
                }
            }
        } else {
            app.tick(TICK.as_millis() as u64);
        }

        if app.should_poll() && last_poll.elapsed() >= POLL {
            refresh(&mut app, &mut fetch);
            last_poll = Instant::now();
        }
    }
}

/// Re-read the queue, keeping the screen up if the instance is unreachable.
///
/// A failed refresh is reported and the previous queue stays on screen. Tearing
/// the screen down because one poll failed would lose the reviewer's place over
/// a dropped connection.
fn refresh<F>(app: &mut ReviewApp, fetch: &mut F)
where
    F: FnMut() -> Result<Vec<theta_proto::wire::ReviewBatchWire>, String>,
{
    match fetch() {
        Ok(batches) => app.update(batches),
        Err(e) => app.report(format!("could not refresh: {e}")),
    }
}

fn translate(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

fn draw(frame: &mut ratatui::Frame, app: &ReviewApp, project: &str) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(render::title(app, project))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        rows[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);

    let queue: Vec<Line> = render::queue(app).into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(Text::from(queue)).block(Block::default().borders(Borders::RIGHT)),
        panes[0],
    );

    let detail: Vec<Line> = render::detail(app).into_iter().map(Line::from).collect();
    frame.render_widget(Paragraph::new(Text::from(detail)), panes[1]);

    let mut footer = vec![Line::from(render::footer(app))];
    if let Some(message) = app.message() {
        footer.insert(0, Line::from(message.to_string()));
    }
    frame.render_widget(Paragraph::new(Text::from(footer)), rows[2]);
}

/// Whether the screen is in a state where a keypress could answer a gate.
///
/// Exposed for the binary's own sanity check rather than used here.
pub fn awaiting_decision(app: &ReviewApp) -> bool {
    matches!(app.mode(), Mode::Browsing)
}
