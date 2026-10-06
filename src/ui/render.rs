//! Rendering of the terminal UI.
//!
//! Pure drawing: every value comes from the [`App`](super::app::App) state machine.
//! The layout is deliberately simple — one tree, one status panel, one message log —
//! because the point of the interface is that the user sees one project.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::app::{App, InputKind, Screen};
use crate::model::RepositoryState;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

/// Draw the whole interface.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(if app.report.is_some() || !app.log.is_empty() {
                10
            } else {
                3
            }),
            Constraint::Length(if app.mode.is_some() { 3 } else { 2 }),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    match app.screen {
        Screen::Setup => draw_setup(frame, app, chunks[1]),
        Screen::Project => draw_project(frame, app, chunks[1]),
    }
    draw_log(frame, app, chunks[2]);
    if app.mode.is_some() {
        draw_input(frame, app, chunks[3]);
    } else {
        draw_hints(frame, app, chunks[3]);
    }

    if app.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = match (&app.project, app.screen) {
        (Some(project), _) => format!(" {} ", project.name),
        (None, Screen::Setup) => " GitMesh setup ".to_string(),
        _ => " GitMesh ".to_string(),
    };
    let mut spans = vec![
        Span::styled(
            "gitmesh",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            title.trim().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{}", app.start_dir.display()),
            Style::default().fg(MUTED),
        ),
        Span::raw("  "),
    ];
    if app.screen == Screen::Project {
        spans.push(Span::styled(
            format!("branch {}", app.logical_branch()),
            Style::default().fg(ACCENT),
        ));
    }
    if app.dry_run {
        spans.push(Span::styled("  [DRY RUN]", Style::default().fg(WARN)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" one project, one status ");
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_setup(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    frame.render_widget(
        tree_widget(app, " project tree (Enter/E: path, A: mark repository) "),
        chunks[0],
    );

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "No GitMesh project here yet.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw("Setup steps:"),
        Line::raw("  1. choose the project directory (Enter)"),
        Line::raw("  2. select a directory in the tree"),
        Line::raw("  3. press A to make it an independent repository"),
        Line::raw("     (or leave it in the root repository)"),
        Line::raw("  4. press W to save .gitmesh/project.toml"),
        Line::raw(""),
        Line::styled(
            "GitMesh never moves or modifies your files.",
            Style::default().fg(MUTED),
        ),
    ];
    if let Some(scan) = &app.scan {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "{} Git repository(ies) found in this tree",
                scan.repositories.len()
            ),
            Style::default().fg(ACCENT),
        ));
        for notice in scan.notices.iter().take(3) {
            lines.push(Line::styled(notice.clone(), Style::default().fg(WARN)));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" setup ")),
        chunks[1],
    );
}

fn draw_project(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    frame.render_widget(tree_widget(app, " project tree "), chunks[0]);

    let rows_panel = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(8)])
        .split(chunks[1]);

    frame.render_widget(status_widget(app), rows_panel[0]);
    frame.render_widget(changes_widget(app), rows_panel[1]);
}

fn tree_widget<'a>(app: &'a App, title: &'a str) -> Paragraph<'a> {
    let mut lines: Vec<Line> = Vec::new();
    for (index, row) in app.rows.iter().enumerate() {
        let selected = index == app.selected;
        let indent = "  ".repeat(row.depth);
        let mut spans = vec![Span::raw(indent)];
        let name_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(row.name.clone(), name_style));

        if row.is_external {
            let id = row.repository_id.clone().unwrap_or_else(|| "repo".into());
            spans.push(Span::styled(
                format!("  [repo {id}]"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        } else if row.is_repository_root {
            spans.push(Span::styled(
                "  [git repository - unassigned]",
                Style::default().fg(WARN),
            ));
        } else if row.relative_path == std::path::Path::new(".") {
            spans.push(Span::styled(
                "  [root repository]",
                Style::default().fg(MUTED),
            ));
        }
        if row.file_count > 0 {
            spans.push(Span::styled(
                format!("  {} file(s)", row.file_count),
                Style::default().fg(MUTED),
            ));
        }
        if row.truncated {
            spans.push(Span::styled("  …", Style::default().fg(MUTED)));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "(empty directory)",
            Style::default().fg(MUTED),
        ));
    }
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_string()),
    )
}

fn status_widget<'a>(app: &'a App) -> Table<'a> {
    let header = Row::new(vec!["REPOSITORY", "PATH", "BRANCH", "STATE"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .status
        .as_ref()
        .map(|status| {
            status
                .repositories
                .iter()
                .map(|state| {
                    let style = if !state.is_usable() {
                        Style::default().fg(BAD)
                    } else if state.has_conflicts() {
                        Style::default().fg(WARN)
                    } else {
                        Style::default()
                    };
                    Row::new(vec![
                        state.id.clone(),
                        state.relative_path.clone(),
                        branch_of(state),
                        state.summary(),
                    ])
                    .style(style)
                })
                .collect()
        })
        .unwrap_or_default();

    Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" unified status "),
    )
}

fn changes_widget<'a>(app: &'a App) -> Paragraph<'a> {
    let mut lines: Vec<Line> = Vec::new();
    let Some(project) = &app.project else {
        return Paragraph::new(lines);
    };
    let Some(status) = &app.status else {
        return Paragraph::new(lines);
    };
    let analyzer = crate::analyzer::Analyzer::new(project, &app.runner);
    let changes = analyzer.owned_changes(status);
    if changes.is_empty() {
        lines.push(Line::styled(
            "no changes: the project is clean",
            Style::default().fg(OK),
        ));
    }
    for change in changes.iter().take(20) {
        let (label, style) = if change.is_conflict() {
            ("conflict", Style::default().fg(BAD))
        } else if change.entry.untracked {
            ("new", Style::default().fg(OK))
        } else if change.entry.staged {
            ("staged", Style::default().fg(ACCENT))
        } else {
            ("modified", Style::default().fg(WARN))
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<10}", change.repository_id),
                Style::default().fg(MUTED),
            ),
            Span::styled(format!("{label:<9} "), style),
            Span::raw(change.logical_path.clone()),
        ]));
    }
    if changes.len() > 20 {
        lines.push(Line::styled(
            format!("... and {} more", changes.len() - 20),
            Style::default().fg(MUTED),
        ));
    }
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" changes by owning repository "),
    )
}

fn draw_log(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .log
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(|line| {
            let style = if line.contains('✗') {
                Style::default().fg(BAD)
            } else if line.contains('!') {
                Style::default().fg(WARN)
            } else if line.contains('✓') {
                Style::default().fg(OK)
            } else {
                Style::default()
            };
            ListItem::new(Line::raw(line.clone())).style(style)
        })
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" activity ")),
        area,
    );
}

fn draw_hints(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let hints = match app.screen {
        Screen::Setup => "Enter: open directory   A: mark/unmark repository   I: rename   U: remote   W: save configuration   S: rescan   ?: help   Q: quit",
        Screen::Project => "C: commit   P: pull   Shift-P: push   F: fetch   S: refresh   N: new branch   B: switch branch   M: merge   A: unassign   D: dry-run   R: reload   ?: help   Q: quit",
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hints, Style::default().fg(MUTED))),
        area,
    );
}

fn draw_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let kind = app.mode.unwrap_or(InputKind::Directory);
    let text = format!("{}: {}", kind.prompt(), app.input);
    let cursor_x = text.chars().count() as u16;
    frame.render_widget(
        Paragraph::new(Line::raw(text)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Enter to confirm, Esc to cancel "),
        ),
        area,
    );
    frame.set_cursor_position((area.x + 2 + cursor_x, area.y + 1));
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 78, 20);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::styled(
            "GitMesh - one logical project over many physical repositories",
            Style::default().fg(ACCENT),
        ),
        Line::raw(""),
        Line::raw("Navigation      j/k or arrows: move in the tree"),
        Line::raw("                Enter: open a directory (setup) / confirm input"),
        Line::raw("Configuration   A: mark or unmark the selected directory as an independent"),
        Line::raw("                   physical repository (saved to .gitmesh/project.toml)"),
        Line::raw("                I: rename the repository   U: set its remote URL"),
        Line::raw("                W: save the configuration"),
        Line::raw("Everyday work   C: commit all changes with one message"),
        Line::raw("                P: pull every repository   Shift-P: push every repository"),
        Line::raw("                F: fetch   S: refresh status"),
        Line::raw("Branches        N: create and switch to a branch everywhere"),
        Line::raw("                B: switch to an existing branch   M: merge a branch"),
        Line::raw("Safety          D: dry-run on/off (nothing is changed)"),
        Line::raw("                Q: quit   ?: close this help"),
        Line::raw(""),
        Line::styled(
            "GitMesh never discards local work and never reimplements Git.",
            Style::default().fg(MUTED),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" help ")),
        popup,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn branch_of(state: &RepositoryState) -> String {
    if !state.is_usable() {
        return "-".to_string();
    }
    state.head().label()
}
