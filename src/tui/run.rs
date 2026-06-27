use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout, Write};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::core::{Controller, ControllerEvent, HandoffStatus, SessionHandle, UiEvent};

use super::SPINNER_TICK;
use super::app::{App, LogEntry, Mode, UiConfig};

pub async fn run<H: SessionHandle>(
    host: &H,
    cfg: UiConfig,
    initial: Controller,
    handoff_rx: Option<mpsc::Receiver<HandoffStatus>>,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, host, cfg, initial, handoff_rx).await;
    restore_terminal(&mut terminal)?;
    result
}

// Never resolves while detached (None), so the select! branch stays inert.
async fn recv_events(
    events: &mut Option<mpsc::Receiver<ControllerEvent>>,
) -> Option<ControllerEvent> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// Inert when no relay is configured.
async fn recv_handoff(rx: &mut Option<mpsc::Receiver<HandoffStatus>>) -> Option<HandoffStatus> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn run_loop<H: SessionHandle>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    host: &H,
    cfg: UiConfig,
    initial: Controller,
    mut handoff_rx: Option<mpsc::Receiver<HandoffStatus>>,
) -> Result<()> {
    let mut app = App::new(cfg);

    // Some while Foreground, None while Background; swapped at the transitions below.
    let mut events: Option<mpsc::Receiver<ControllerEvent>> = Some(initial.events);
    let mut ui_tx: Option<mpsc::Sender<UiEvent>> = Some(initial.ui_tx);

    let mut input_stream = EventStream::new();
    let mut tick = tokio::time::interval(SPINNER_TICK);
    // Skip missed ticks rather than burning CPU catching up.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    terminal.draw(|f| app.render(f))?;

    loop {
        let mut redraw = true;
        tokio::select! {
            maybe_event = recv_events(&mut events) => {
                match maybe_event {
                    Some(event) => {
                        app.handle_agent_event(event);
                        // Drain the queue before redrawing: a bulk replay floods the
                        // channel, and this collapses N re-renders into ~1.
                        if let Some(rx) = events.as_mut() {
                            while let Ok(event) = rx.try_recv() {
                                app.handle_agent_event(event);
                            }
                        }
                    }
                    None => break, // stream closed (session ended)
                }
            }
            maybe_status = recv_handoff(&mut handoff_rx) => {
                match maybe_status {
                    Some(status) => app.handoff_status = Some(status),
                    // Channel closed — stop polling so the branch doesn't spin.
                    None => {
                        handoff_rx = None;
                        redraw = false;
                    }
                }
            }
            maybe_input = input_stream.next() => {
                match maybe_input {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.mode {
                            Mode::Foreground => {
                                if let Some(tx) = ui_tx.as_ref() {
                                    app.handle_key(key, tx);
                                }
                            }
                            Mode::Background => app.handle_background_key(key),
                        }
                    }
                    Some(Ok(Event::Paste(text))) if app.mode == Mode::Foreground => {
                        app.handle_paste(text);
                    }
                    Some(Ok(Event::Mouse(ev))) if app.mode == Mode::Foreground => {
                        if !app.handle_mouse(ev) {
                            redraw = false; // motion/button we don't act on
                        }
                    }
                    Some(Ok(_)) => { redraw = false; }
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
            _ = tick.tick() => {
                if app.status.is_empty() {
                    redraw = false; // idle — no spinner to animate
                } else {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
            }
        }

        // Apply a transition here, where the host handle and channel slots live.
        if let Some(target) = app.pending_transition.take() {
            match target {
                Mode::Background => {
                    // detach() fires the host's handoff hook (bind socket / dial relay).
                    host.detach();
                    events = None;
                    ui_tx = None;
                    app.enter_background();
                }
                Mode::Foreground => {
                    // Force-reclaim; only the owner can take a session a guest holds
                    // (attach_force is a plain attach for guests).
                    match host.attach_force().await {
                        Some(c) => {
                            app.enter_foreground();
                            events = Some(c.events);
                            ui_tx = Some(c.ui_tx);
                        }
                        None => {
                            // Owner force fails only if the broker is gone; guest attach
                            // fails when another controller holds the session.
                            let why = if app.is_owner {
                                "could not foreground — session has ended"
                            } else {
                                "could not foreground — another controller holds the session"
                            };
                            app.push(LogEntry::Warn(why.into()));
                        }
                    }
                }
            }
        }

        if app.quit {
            break;
        }
        if redraw {
            terminal.draw(|f| app.render(f))?;
        }
    }

    Ok(())
}

// Raw mode is owned separately (it stays on across a pair-screen suspend), so it's
// not toggled here.
fn enter_screen<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // Opt into the kitty keyboard protocol so Ctrl/Shift+Enter arrive as distinct
    // events. Best-effort: unsupported terminals ignore the CSI, so don't error.
    let _ = execute!(
        w,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        ),
    );
    Ok(())
}

fn leave_screen<W: Write>(w: &mut W) -> io::Result<()> {
    let _ = execute!(w, PopKeyboardEnhancementFlags);
    execute!(
        w,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_screen(&mut stdout)?;

    // Restore the terminal on panic so a bug doesn't wreck the user's shell.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = leave_screen(&mut io::stdout());
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    leave_screen(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}
