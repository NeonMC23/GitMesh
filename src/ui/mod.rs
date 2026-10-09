//! Terminal user interface.
//!
//! ```text
//!   gitmesh ui           (alias: gitmesh tui)
//! ```
//!
//! The interface is a minimal everyday workflow: see the changes grouped by repository,
//! stage all, commit with one message, pull and push. See `docs/TUI.md` for the
//! keyboard reference.
//!
//! Guarantees of this module:
//!
//! * **No Git logic.** Every action calls [`crate::ops`], exactly like the CLI.
//! * **Testable without a terminal.** [`app`] is independent of `ratatui`/`crossterm`;
//!   this module only translates terminal events and runs the event loop.
//! * **Graceful degradation.** Without a terminal (pipes, CI) the command explains how to
//!   use the CLI instead of failing or hanging. Too-small terminals get a resize notice.

pub mod app;
pub mod render;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::tty::IsTty;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::error::Result;
use app::{Action, App, Key};

/// Run the terminal interface.
pub fn run(start: &Path, dry_run: bool) -> Result<()> {
    let mut app = App::new(start, dry_run)?;

    if !std::io::stdout().is_tty() {
        print_fallback(&app);
        return Ok(());
    }

    enable_raw_mode().map_err(|e| crate::Error::io(start.to_path_buf(), e))?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| crate::Error::io(start.to_path_buf(), e))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| crate::Error::Other(format!("could not start the terminal: {e}")))?;

    let result = event_loop(&mut terminal, &mut app);

    // Always restore the terminal, even when the event loop failed.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        draw(terminal, app)?;
        if app.quit {
            return Ok(());
        }
        if !event::poll(Duration::from_millis(250))
            .map_err(|e| crate::Error::Other(format!("terminal event error: {e}")))?
        {
            continue;
        }
        let event =
            event::read().map_err(|e| crate::Error::Other(format!("terminal event error: {e}")))?;
        let Event::Key(key) = event else {
            // Resize and other events: the next loop iteration redraws at the new size.
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(action) = handle_key(app, key) {
            // Show "working…" before the operation blocks the loop.
            app.busy = Some(action);
            draw(terminal, app)?;
            app.run_action(action)?;
        }
    }
}

fn draw<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &App) -> Result<()> {
    terminal
        .draw(|frame| render::draw(frame, app))
        .map(|_| ())
        .map_err(|e| crate::Error::Other(format!("could not draw: {e}")))
}

/// Translate one terminal key event and apply it. Returns the action to run now, if any.
///
/// The action is accepted through [`App::accept`] (which checks the message, staging and
/// confirmation) before it is returned, so a `Some` result is always ready to run.
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<Action> {
    let key = translate(key)?;
    app.dispatch(key)
}

/// Map a terminal key event to a [`Key`], or `None` for keys the interface does not use.
pub fn translate(key: KeyEvent) -> Option<Key> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char('c') if ctrl => Key::CtrlC,
        KeyCode::Char(_) if ctrl => return None,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    })
}

fn print_fallback(app: &App) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "gitmesh: no interactive terminal available.");
    let _ = writeln!(out);
    match &app.project {
        Some(project) => {
            let _ = writeln!(
                out,
                "Project '{}' is available at {}.",
                project.name,
                project.root.display()
            );
        }
        None => {
            let _ = writeln!(
                out,
                "No GitMesh project found at or above {}.",
                app.start_dir.display()
            );
        }
    }
    let _ = writeln!(
        out,
        "The terminal interface needs a TTY. The same functionality is available"
    );
    let _ = writeln!(out, "through the command line:");
    let _ = writeln!(out);
    let _ = writeln!(out, "  gitmesh status                   unified status");
    let _ = writeln!(
        out,
        "  gitmesh commit -m \"message\"      commit across repositories"
    );
    let _ = writeln!(
        out,
        "  gitmesh pull | gitmesh push      synchronise everything"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_only_the_keys_the_interface_uses() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(translate(plain(KeyCode::Char('s'))), Some(Key::Char('s')));
        assert_eq!(translate(plain(KeyCode::Enter)), Some(Key::Enter));
        assert_eq!(translate(plain(KeyCode::F(5))), None);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(translate(ctrl_c), Some(Key::CtrlC));
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(translate(ctrl_s), None);
    }
}
