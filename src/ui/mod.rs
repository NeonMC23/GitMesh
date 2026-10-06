//! Terminal user interface.
//!
//! ```text
//!   gitmesh ui           (alias: gitmesh tui)
//! ```
//!
//! Guarantees of this module:
//!
//! * **No Git logic.** Every action calls [`crate::ops`] / [`crate::analyzer`], exactly
//!   like the CLI. There is one implementation of every operation.
//! * **No terminal requirement for tests.** The state machine in
//!   [`app`](self::app) is independent of `ratatui`/`crossterm`; [`run`] only adds the
//!   event loop and drawing.
//! * **Graceful degradation.** Without a terminal (pipes, CI, the sandboxed preview)
//!   the command explains how to use the CLI instead of failing or hanging.

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
use app::{App, InputKind, Screen};

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
        terminal
            .draw(|frame| render::draw(frame, app))
            .map_err(|e| crate::Error::Other(format!("could not draw: {e}")))?;
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
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                handle_key(app, key)?;
            }
        }
    }
}

/// Apply one keypress to the application state.
pub fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // ---- modal input ------------------------------------------------------
    if let Some(kind) = app.mode {
        match key.code {
            KeyCode::Esc => app.cancel_input(),
            KeyCode::Enter => app.submit_input()?,
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.input.clear()
            }
            KeyCode::Char(c) => {
                // `r` is only special outside input mode, so every character is typed.
                app.input.push(c);
            }
            _ => {}
        }
        let _ = kind;
        return Ok(());
    }

    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.show_help = !app.show_help,
        KeyCode::Esc => app.show_help = false,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('d') => {
            app.dry_run = !app.dry_run;
            let state = if app.dry_run { "on" } else { "off" };
            app.log(format!("dry-run mode {state}"));
        }
        KeyCode::Char('s') => {
            app.refresh_scan()?;
            app.refresh_status()?;
            app.log("refreshed");
        }
        KeyCode::Char('r') => {
            if let Err(err) = app.reload_project() {
                app.log(format!("could not reload: {err}"));
            }
        }
        KeyCode::Char('w') => {
            if let Err(err) = app.create_project_from_scan() {
                app.log(format!("could not save the configuration: {err}"));
            }
        }
        KeyCode::Char('a') => {
            if let Err(err) = app.assign_selected() {
                app.log(format!("{err}"));
            }
        }
        KeyCode::Char('i') => app.begin_input(InputKind::RepositoryId),
        KeyCode::Char('u') => app.begin_input(InputKind::RemoteUrl),
        KeyCode::Char('c') => app.begin_input(InputKind::CommitMessage),
        KeyCode::Char('b') => app.begin_input(InputKind::CheckoutBranch),
        KeyCode::Char('n') => app.begin_input(InputKind::NewBranch),
        KeyCode::Char('m') => app.begin_input(InputKind::MergeBranch),
        KeyCode::Char('f') => {
            if let Err(err) = app.run_fetch() {
                app.log(format!("{err}"));
            }
        }
        KeyCode::Char('p') => {
            if let Err(err) = app.run_pull() {
                app.log(format!("{err}"));
            }
        }
        KeyCode::Char('P') => {
            if let Err(err) = app.run_push() {
                app.log(format!("{err}"));
            }
        }
        KeyCode::Enter => match app.screen {
            Screen::Setup => app.begin_input(InputKind::Directory),
            Screen::Project => {
                if let Err(err) = app.refresh_status() {
                    app.log(format!("{err}"));
                }
            }
        },
        KeyCode::Char('e') => app.begin_input(InputKind::Directory),
        _ => {}
    }
    Ok(())
}

/// Printed when there is no terminal to draw on.
fn print_fallback(app: &App) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "gitmesh: no interactive terminal available.");
    let _ = writeln!(out);
    match (&app.project, app.screen) {
        (Some(project), _) => {
            let _ = writeln!(
                out,
                "Project '{}' is available at {}.",
                project.name,
                project.root.display()
            );
        }
        _ => {
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
    let _ = writeln!(out, "  gitmesh init                     create a project");
    let _ = writeln!(out, "  gitmesh discover                 inspect the tree");
    let _ = writeln!(
        out,
        "  gitmesh configure add <dir>      mark an external repository"
    );
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
    use crate::testkit::RepoFixture;

    fn press(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    #[test]
    fn navigation_and_quit_work() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let mut app = App::new(fixture.path(), false).unwrap();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Char('?'));
        assert!(app.show_help);
        press(&mut app, KeyCode::Char('?'));
        assert!(!app.show_help);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn commit_flow_through_keystrokes() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/a.rs", "y");
        let mut app = App::new(fixture.path(), false).unwrap();

        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.mode, Some(InputKind::CommitMessage));
        for c in "typed message".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, None);
        assert!(fixture
            .git_ok(".", &["log", "-1", "--pretty=%s"])
            .contains("typed message"));
        assert!(fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .contains("typed message"));
    }

    #[test]
    fn input_can_be_cancelled() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        let mut app = App::new(fixture.path(), false).unwrap();
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, None);
        assert!(app.input.is_empty());
    }

    #[test]
    fn dry_run_toggle_is_visible_in_state() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let mut app = App::new(fixture.path(), false).unwrap();
        assert!(!app.dry_run);
        press(&mut app, KeyCode::Char('d'));
        assert!(app.dry_run);
        fixture.write("src/a.rs", "x");
        press(&mut app, KeyCode::Char('c'));
        for c in "dry".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        // The commit was simulated, not performed.
        let log = fixture.git_ok(".", &["log", "--oneline"]);
        assert!(!log.contains("dry"));
    }

    #[test]
    fn setup_screen_flow_assigns_a_repository() {
        let fixture = RepoFixture::new();
        fixture.init_repo("engine");
        fixture.write("engine/README.md", "# engine\n");
        fixture.commit("engine", "init");
        let mut app = App::new(fixture.path(), false).unwrap();
        assert_eq!(app.screen, Screen::Setup);
        press(&mut app, KeyCode::Char('w')); // save configuration
        assert_eq!(app.screen, Screen::Project);

        let index = app
            .rows
            .iter()
            .position(|row| row.relative_path == std::path::Path::new("engine"))
            .unwrap();
        app.selected = index;
        press(&mut app, KeyCode::Char('a')); // mark as repository
        assert!(app.project.as_ref().unwrap().repository("engine").is_some());

        // Renaming through the input flow works too.
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Enter);
        assert!(app.project.as_ref().unwrap().repository("ex").is_some());
    }
}
