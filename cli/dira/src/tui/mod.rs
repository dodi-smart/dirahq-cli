//! `dira watch` — an interactive, auto-refreshing terminal dashboard over the
//! same `Status` snapshot the one-shot `dira status` prints. The plain-text
//! output is untouched; this is purely an additive view.
//!
//! The loop polls `Request::Status` over the UDS on the data interval, but
//! redraws on a faster frame tick and **interpolates** engaged-session timers
//! client-side between polls (see [`dashboard::tick`]) so counters tick smoothly
//! second-by-second instead of jumping once per poll. Each poll reconciles to the
//! daemon's authoritative numbers. If the daemon is unreachable we render a
//! "daemon not running" state and keep retrying — never crash, never leave the
//! terminal in raw mode.
//!
//! TODO(subscribe): polling is fine for now. A future `Request::Subscribe`
//! streaming push from the daemon would replace the data poll; the frame tick +
//! interpolation here would not need to change.

mod dashboard;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use dashboard::Conn;
use dira_core::protocol::{Request, Response, StatusView};
use dira_core::Config;
use futures_util::StreamExt; // `.next()` on crossterm's `EventStream`.
use ratatui::DefaultTerminal;
use std::path::Path;
use std::time::Duration;

/// How often we repaint, independent of the (possibly slower) data poll. A short
/// frame makes the live-tail timers tick smoothly: `now` advances every frame, so
/// each engaged session's `now - last_activity` grows between polls.
const FRAME: Duration = Duration::from_millis(250);

/// The latest daemon state — a fresh snapshot, or the last error string.
enum Live {
    Up(StatusView),
    Down(String),
}

impl Live {
    fn capture(result: PollResult) -> Self {
        match result {
            PollResult::Up(snap) => Live::Up(snap),
            PollResult::Down(err) => Live::Down(err),
        }
    }
}

/// Run the dashboard until the user quits. Installs a panic hook so a panic
/// during the draw loop still restores the terminal before unwinding.
pub async fn run(config: &Config, interval: Duration) -> Result<()> {
    install_panic_hook();
    // `try_init` (vs `init`) lets us fail cleanly with a message instead of
    // panicking when there's no TTY (piped output, CI, etc.).
    let mut terminal = ratatui::try_init()
        .map_err(|e| anyhow::anyhow!("could not open a terminal for `dira watch`: {e}"))?;
    let result = event_loop(&mut terminal, config, interval).await;
    ratatui::restore();
    result
}

/// Wrap the existing panic hook so the terminal is always restored first.
fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    config: &Config,
    interval: Duration,
) -> Result<()> {
    let mut events = EventStream::new();
    // Two cadences: a data poll (authoritative snapshot) and a faster repaint
    // that interpolates between polls. Polling no faster than the repaint.
    let mut poll = tokio::time::interval(interval.max(FRAME));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame = tokio::time::interval(FRAME);
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume both immediate first ticks; we poll + draw once up front below.
    poll.tick().await;
    frame.tick().await;

    let idle = config.idle_seconds as i64;
    let mut live = Live::capture(poll_status(&config.socket_path).await);

    loop {
        draw(terminal, &live, idle)?;

        tokio::select! {
            _ = poll.tick() => {
                live = Live::capture(poll_status(&config.socket_path).await);
            }
            _ = frame.tick() => {
                // Just repaint — `draw` re-interpolates from the current snapshot.
            }
            maybe_event = events.next() => {
                // Stream errors / closure are non-fatal — the daemon poll is
                // what matters, input is best-effort.
                if let Some(Ok(event)) = maybe_event {
                    if should_quit(&event) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Outcome of one status poll: the parsed view, or a human error string.
enum PollResult {
    Up(StatusView),
    Down(String),
}

fn draw(terminal: &mut DefaultTerminal, live: &Live, idle: i64) -> Result<()> {
    terminal.draw(|frame| match live {
        Live::Up(snap) => {
            // Grow engaged sessions by their live tail (`now - last_activity`,
            // clamped to idle) so timers tick smoothly; the next poll reconciles
            // to the daemon's settled values.
            let view = dashboard::tick(snap, time::OffsetDateTime::now_utc(), idle);
            dashboard::draw(frame, &Conn::Up(&view));
        }
        Live::Down(err) => dashboard::draw(frame, &Conn::Down(err)),
    })?;
    Ok(())
}

/// Poll the daemon once. Any transport/protocol failure becomes a `Down` state
/// rather than an error, so the loop keeps running and retrying.
async fn poll_status(socket: &Path) -> PollResult {
    match crate::client::send(socket, &Request::Status).await {
        Ok(Response::Status(s)) => PollResult::Up(s),
        Ok(Response::Error { message }) => PollResult::Down(message),
        Ok(_) => PollResult::Down("unexpected response from daemon".to_string()),
        Err(e) => PollResult::Down(e.to_string()),
    }
}

/// `q`, `Esc`, or `Ctrl-C` quit.
fn should_quit(event: &Event) -> bool {
    if let Event::Key(key) = event {
        // Ignore key-release events on terminals that report them.
        if key.kind == KeyEventKind::Release {
            return false;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn quit_keys() {
        assert!(should_quit(&key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(should_quit(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(should_quit(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn non_quit_keys() {
        assert!(!should_quit(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!should_quit(&key(KeyCode::Char('x'), KeyModifiers::NONE)));
        assert!(!should_quit(&key(KeyCode::Up, KeyModifiers::NONE)));
    }

    #[test]
    fn ignores_key_release() {
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(!should_quit(&release));
    }
}
