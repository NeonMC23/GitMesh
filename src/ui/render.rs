//! Drawing for the terminal interface. No state changes happen here.
//!
//! Layout, top to bottom:
//!
//! ```text
//! GitMesh · project · branch · DRY RUN          header (2 lines)
//! /path/to/project · 5 changes · 1 staged
//! ┌ Changes by repository ───────────────┐
//! │ app  2 changes, 1 staged             │      grouped changes (flexible)
//! │   M  app/main.rs                     │
//! └──────────────────────────────────────┘
//! ┌ Commit message ──────────────────────┐
//! │ fix the parser                       │      one message field
//! └──────────────────────────────────────┘
//! [ s Stage all ] [ c Commit ] [ p Pull ] [ P Push ]
//! ┌ Result ──────────────────────────────┐      last operation, per repository
//! └──────────────────────────────────────┘
//! s stage all · c commit · p pull · …          footer: real shortcuts only
//! ```

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{App, ChangeLine, Focus, BUTTONS, MIN_HEIGHT, MIN_WIDTH, SHORTCUTS};
use crate::model::RepositoryRole;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

/// Draw one frame.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }
    if app.show_help {
        draw_help(frame, area);
        return;
    }
    if app.project.is_none() {
        draw_no_project(frame, app, area);
        return;
    }
    draw_project(frame, app, area);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let text = vec![
        Line::from(Span::styled(
            "Terminal too small for GitMesh",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "Needs at least {MIN_WIDTH}×{MIN_HEIGHT}, this is {}×{}.",
            area.width, area.height
        )),
        Line::from("Enlarge the window, or press q to quit."),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

fn draw_no_project(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = area.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            "No GitMesh project is open",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if let Some(error) = &app.open_error {
        lines.push(Line::from(Span::styled(
            fit(error, width),
            Style::default().fg(BAD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Create one with `gitmesh init` in your project directory",
    ));
    lines.push(Line::from(
        "(or use `gitmesh gui`), then open this screen again.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "q quit",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_project(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let result_height = result_height(app, area.height);
    let chunks = Layout::vertical([
        Constraint::Length(2),             // header
        Constraint::Min(3),                // changes
        Constraint::Length(3),             // message
        Constraint::Length(1),             // action bar
        Constraint::Length(result_height), // result
        Constraint::Length(1),             // footer
    ])
    .split(area);

    draw_header(frame, app, chunks[0]);
    draw_changes(frame, app, chunks[1]);
    draw_message(frame, app, chunks[2]);
    draw_actions(frame, app, chunks[3]);
    if result_height > 0 {
        draw_result(frame, app, chunks[4]);
    }
    draw_footer(frame, app, chunks[5]);
}

/// Rows given to the result panel: its content, bounded so changes keep room.
fn result_height(app: &App, total: u16) -> u16 {
    let Some(result) = &app.result else {
        return 0;
    };
    // Title + one row per line + the two borders, bounded so the changes list keeps room.
    let wanted = (result.lines.len() as u16 + 3).clamp(3, 12);
    // Keep at least six rows for the changes list.
    let budget = total.saturating_sub(2 + 3 + 1 + 1 + 6);
    wanted.min(budget.max(3))
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = area.width as usize;
    let project = app.project.as_ref();
    let name = project.map(|p| p.name.as_str()).unwrap_or("-");
    let mut first = vec![
        Span::styled(
            "GitMesh",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            fit(name, width.saturating_sub(30).max(8)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  branch ", Style::default().fg(MUTED)),
        Span::styled(
            fit(&app.header_branch(), width / 2),
            Style::default().fg(ACCENT),
        ),
    ];
    if app.dry_run {
        first.push(Span::raw("  "));
        first.push(Span::styled(
            " DRY RUN ",
            Style::default()
                .fg(Color::Black)
                .bg(WARN)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let root = project
        .map(|p| p.root.display().to_string())
        .unwrap_or_default();
    let (staged_files, staged_repos) = app.staged_summary();
    let summary = format!(
        "{} change(s) · {} staged in {} repositor{}",
        app.change_count(),
        staged_files,
        staged_repos,
        if staged_repos == 1 { "y" } else { "ies" }
    );
    let second = Line::from(vec![
        Span::styled(
            fit(
                &root,
                width.saturating_sub(summary.chars().count() + 3).max(8),
            ),
            Style::default().fg(MUTED),
        ),
        Span::styled("  ·  ", Style::default().fg(MUTED)),
        Span::raw(summary),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(first), second]), area);
}

fn draw_changes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(" Changes by repository ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let height = inner.height as usize;
    if app.changes.is_empty() {
        let text = if app.status.is_some() {
            "No changes. Every repository matches its last commit."
        } else {
            "Status not available yet. Press r to refresh."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::default().fg(MUTED)))),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    for line in app.changes.iter().skip(app.scroll).take(height) {
        lines.push(match line {
            ChangeLine::Repository {
                id,
                role,
                path,
                changes,
                staged,
                note,
            } => {
                let role_text = match role {
                    RepositoryRole::Root => "root",
                    RepositoryRole::External => "repo",
                };
                let mut spans = vec![
                    Span::styled(
                        fit(
                            &format!("{id} ({role_text}, {path})"),
                            width.saturating_sub(30).max(10),
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {changes} changed, {staged} staged"),
                        Style::default().fg(MUTED),
                    ),
                ];
                if let Some(note) = note {
                    spans.push(Span::styled(
                        format!("  ! {note}"),
                        Style::default().fg(BAD),
                    ));
                }
                Line::from(spans)
            }
            ChangeLine::File { code, path, staged } => {
                let color = if *staged { OK } else { WARN };
                let label = change_label(code);
                let path_width = width.saturating_sub(16).max(8);
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{code:<2}"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {label:<10}"), Style::default().fg(MUTED)),
                    Span::raw(fit(path, path_width)),
                ])
            }
        });
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Human word for a two-letter status code.
fn change_label(code: &str) -> &'static str {
    match code {
        "??" => "untracked",
        "UU" => "conflict",
        c if c.starts_with('A') => "added",
        c if c.starts_with('M') || c.ends_with('M') => "modified",
        c if c.starts_with('D') || c.ends_with('D') => "deleted",
        c if c.starts_with('R') || c.ends_with('R') => "renamed",
        c if c.starts_with('C') || c.ends_with('C') => "copied",
        c if c.starts_with('T') || c.ends_with('T') => "type changed",
        _ => "changed",
    }
}

fn draw_message(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Focus::Message;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
        .title(if focused {
            " Commit message (Tab: actions) "
        } else {
            " Commit message (Tab: edit) "
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let (text, placeholder) = if app.message.is_empty() {
        (
            "write the commit message for every repository with staged changes".to_string(),
            true,
        )
    } else {
        (app.message.clone(), false)
    };
    // Show the end of long messages so the cursor stays visible.
    let shown = tail(&text, width.saturating_sub(1).max(1));
    let style = if placeholder {
        Style::default().fg(MUTED)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(shown.clone(), style))),
        inner,
    );
    if focused && !placeholder {
        let col = shown.chars().count().min(width.saturating_sub(1)) as u16;
        frame.set_cursor_position((inner.x + col, inner.y));
    } else if focused {
        frame.set_cursor_position((inner.x, inner.y));
    }
}

fn draw_actions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans: Vec<Span<'_>> = Vec::new();
    for (index, button) in BUTTONS.iter().enumerate() {
        let selected = app.focus == Focus::Actions && app.button == index;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(
            format!(" {} {} ", button.key, button.action.label()),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    if app.busy.is_some() {
        spans.push(Span::styled(" working… ", Style::default().fg(WARN)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_result(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(result) = &app.result else {
        return;
    };
    let color = if result.problems { BAD } else { OK };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(" Result ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        result.title.clone(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))];
    // Rows left after the title. If the result does not fit, say so: a repository's
    // outcome must never disappear silently.
    let room = (inner.height as usize).saturating_sub(1);
    let (shown, hidden) = if result.lines.len() <= room {
        (result.lines.len(), 0)
    } else {
        // One row is reserved for the note that says how much is hidden.
        let shown = room.saturating_sub(1);
        (shown, result.lines.len() - shown)
    };
    for line in result.lines.iter().take(shown) {
        lines.push(Line::from(line.clone()));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("… {hidden} more line(s) hidden: enlarge the terminal to see them"),
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_footer(frame: &mut Frame<'_>, _app: &App, area: Rect) {
    let width = area.width as usize;
    let keys = [
        ("s", "stage all"),
        ("c", "commit"),
        ("p", "pull"),
        ("P", "push"),
        ("Tab", "message/actions"),
        ("d", "dry run"),
        ("?", "help"),
        ("q", "quit"),
    ];
    let mut text = String::new();
    for (key, label) in keys {
        let piece = format!("{key} {label}");
        let separator = if text.is_empty() { "" } else { "  " };
        if text.chars().count() + separator.len() + piece.chars().count() > width {
            break;
        }
        text.push_str(separator);
        text.push_str(&piece);
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(MUTED)))),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        "Keys",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    for (keys, what) in SHORTCUTS {
        lines.push(Line::from(format!("  {keys:<16} {what}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Commit: write a message, press c (or Enter) once to review, again to confirm.",
    ));
    lines.push(Line::from(
        "Stage all: stages each repository's own changes; never another's.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press any key to close.",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Fit text into `width` columns, keeping the start and marking the cut with "…".
pub fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(width - 1).collect();
    format!("{kept}…")
}

/// Keep the end of text, marking the cut with "…" at the start.
fn tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - (width - 1)).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_marks_cuts_and_never_panics_on_multibyte() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("héllo wörld", 5), "héll…");
        assert_eq!(fit("anything", 0), "");
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(tail("ab", 4), "ab");
    }
}

#[cfg(test)]
mod screen_tests {
    use super::*;
    use crate::testkit::RepoFixture;
    use crate::ui::app::{App, Key};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render one frame and return it as text rows.
    fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn populated_app(fixture: &RepoFixture) -> App {
        App::new(fixture.path(), false).expect("open")
    }

    fn busy_fixture() -> RepoFixture {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "src/shared.txt");
        let long_dir = "deeply/nested/".repeat(8);
        fixture.write(
            &format!("{long_dir}file-with-a-very-long-name-indeed.rs"),
            "x\n",
        );
        fixture.write("README.md", "root\n");
        fixture
    }

    #[test]
    fn draws_at_every_size_without_panicking() {
        let fixture = busy_fixture();
        let mut app = populated_app(&fixture);
        app.message =
            "a commit message that is considerably longer than the field itself allows".into();
        app.result = Some(crate::ui::app::ResultView {
            title: "Push: NOT everything succeeded".into(),
            lines: (0..30)
                .map(|i| {
                    format!("✗ engine  line {i} of a multi-line error that keeps going and going")
                })
                .collect(),
            problems: true,
        });
        for (w, h) in [
            (1, 1),
            (20, 5),
            (55, 13),
            (56, 14),
            (80, 24),
            (120, 40),
            (200, 60),
        ] {
            let rows = screen(&app, w, h);
            assert_eq!(rows.len(), h as usize);
        }
        app.show_help = true;
        screen(&app, 80, 24);
    }

    #[test]
    fn small_terminals_get_a_resize_notice_instead_of_a_cramped_layout() {
        let fixture = busy_fixture();
        let app = populated_app(&fixture);
        let text = screen(&app, 40, 10).join("\n");
        assert!(text.contains("Terminal too small"), "{text}");
        assert!(text.contains("40×10"), "{text}");
        let just_big_enough = screen(&app, MIN_WIDTH, MIN_HEIGHT).join("\n");
        assert!(
            just_big_enough.contains("Changes by repository"),
            "{just_big_enough}"
        );
    }

    #[test]
    fn the_main_screen_shows_the_advertised_actions_and_footer_keys() {
        let fixture = busy_fixture();
        let app = populated_app(&fixture);
        let text = screen(&app, 100, 30).join("\n");
        for label in [
            "Stage all",
            "Commit",
            "Pull",
            "Push",
            "Changes by repository",
            "Commit message",
        ] {
            assert!(text.contains(label), "missing '{label}':\n{text}");
        }
        assert!(text.contains("s stage all"), "{text}");
        assert!(text.contains("q quit"), "{text}");
    }

    #[test]
    fn long_paths_are_cut_with_an_ellipsis_not_wrapped_into_other_panels() {
        let fixture = busy_fixture();
        let app = populated_app(&fixture);
        let rows = screen(&app, 70, 24);
        assert!(rows.iter().any(|r| r.contains('…')), "{rows:#?}");
        // The frame borders stay in place on every row of the changes panel.
        assert!(rows.iter().any(|r| r.starts_with('┌') || r.contains('┌')));
    }

    #[test]
    fn the_message_field_shows_the_end_of_a_long_message() {
        let fixture = busy_fixture();
        let mut app = populated_app(&fixture);
        app.message = format!("{}END-OF-MESSAGE", "x".repeat(200));
        let text = screen(&app, 80, 24).join("\n");
        assert!(text.contains("END-OF-MESSAGE"), "{text}");
    }

    #[test]
    fn a_missing_project_is_explained() {
        let fixture = RepoFixture::new();
        let app = populated_app(&fixture);
        let text = screen(&app, 80, 24).join("\n");
        assert!(text.contains("No GitMesh project is open"), "{text}");
        assert!(text.contains("gitmesh init"), "{text}");
    }

    #[test]
    fn dry_run_is_labelled_on_screen() {
        let fixture = busy_fixture();
        let mut app = populated_app(&fixture);
        app.handle_key(Key::Char('d'));
        let text = screen(&app, 100, 30).join("\n");
        assert!(text.contains("DRY RUN"), "{text}");
    }
}
