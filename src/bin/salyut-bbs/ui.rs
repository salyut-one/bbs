use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, ConfirmAction, Editor, EditorField, Mode};

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, app, rows[0]);
    draw_board_description(frame, app, rows[1]);
    draw_post_list(frame, app, rows[2]);
    draw_help(frame, app, rows[3]);

    match &app.mode {
        Mode::View => draw_post(frame, app, centered(area, 88, 84), None),
        Mode::Vote(selected) => draw_post(frame, app, centered(area, 88, 84), Some(*selected)),
        Mode::Edit(editor) => draw_editor(frame, editor, centered(area, 90, 90)),
        Mode::Confirm(action, yes) => {
            draw_confirmation(frame, app, *action, *yes, centered(area, 54, 24))
        }
        Mode::Browse => {}
    }
}

fn draw_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut line = vec![
        Span::styled(
            " SALYUT.ONE BBS ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  @{}  ", app.handle)),
    ];
    for (index, board) in app.boards.iter().enumerate() {
        let style = if index == app.board_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::LightCyan)
        };
        line.push(Span::styled(format!(" {} ", board.name), style));
        line.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(line)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_board_description(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let board = app.current_board();
    let restriction = board
        .write_group
        .as_ref()
        .map(|group| format!(" · new threads: {group}"))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "/{} · {}{}",
            board.slug, board.description, restriction
        ))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_post_list(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let items = app.posts.iter().map(post_item).collect::<Vec<_>>();
    let mut state =
        ListState::default().with_selected((!app.posts.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" {} ", app.current_board().name))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("» "),
        area,
        &mut state,
    );
}

fn post_item(post: &salyut_bbs::protocol::PostSummary) -> ListItem<'_> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:>4} ", post.id),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(
                "{}{}{}",
                post.title,
                if post.is_poll { " ◉" } else { "" },
                if post.locked { " [locked]" } else { "" }
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  @{}  {}  {} repl{}",
            post.author,
            post.updated_at.format("%Y-%m-%d"),
            post.reply_count,
            if post.reply_count == 1 { "y" } else { "ies" },
        )),
    ]))
}

fn draw_help(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[/]", Style::default().fg(Color::LightCyan)),
            Span::raw(" board  "),
            Span::styled("↑/↓", Style::default().fg(Color::LightCyan)),
            Span::raw(" move  "),
            Span::styled("Enter", Style::default().fg(Color::LightCyan)),
            Span::raw(" read  "),
            Span::styled("n/e/d", Style::default().fg(Color::LightCyan)),
            Span::raw(" new/edit/delete  "),
            Span::styled("r/q", Style::default().fg(Color::LightCyan)),
            Span::raw(format!(" refresh/quit  │ {}", app.message)),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_post(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect, vote_selected: Option<usize>) {
    let Some(post) = &app.viewed else { return };
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::styled(
            post.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(format!(
            "/{} · by @{} · {} · post #{}",
            post.board.slug,
            post.author,
            post.updated_at.format("%Y-%m-%d %H:%M UTC"),
            post.id
        )),
        Line::raw(""),
        Line::raw(post.body.clone()),
    ];
    if post.locked {
        lines.insert(
            2,
            Line::styled("[LOCKED]", Style::default().fg(Color::LightCyan)),
        );
    }
    if let Some(poll) = &post.poll {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Poll · {} total vote(s)", poll.total_votes),
            Style::default().fg(Color::LightCyan),
        ));
        for (index, option) in poll.options.iter().enumerate() {
            let marker = if vote_selected == Some(index) {
                "»"
            } else if poll.my_vote == Some(option.id) {
                "●"
            } else {
                " "
            };
            let percent = (u64::from(option.votes) * 100)
                .checked_div(u64::from(poll.total_votes))
                .unwrap_or(0);
            lines.push(Line::raw(format!(
                "{marker} {} — {} vote(s), {percent}%",
                option.label, option.votes
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("Replies · {}", post.replies.len()),
        Style::default().fg(Color::LightCyan),
    ));
    for (index, reply) in post.replies.iter().enumerate() {
        let marker = if index == app.reply_selected {
            "»"
        } else {
            " "
        };
        lines.push(Line::styled(
            format!(
                "{marker} #{} · @{} · {}",
                reply.id,
                reply.author,
                reply.updated_at.format("%Y-%m-%d %H:%M UTC")
            ),
            Style::default().fg(Color::DarkGray),
        ));
        for body_line in reply.body.lines() {
            lines.push(Line::raw(format!("    {body_line}")));
        }
    }
    lines.push(Line::raw(""));
    let help = if vote_selected.is_some() {
        "↑/↓: choose · Enter: vote/change vote · Esc: cancel".to_owned()
    } else {
        let mut commands = vec!["Esc/q: close".to_owned()];
        if !post.locked {
            commands.push("a: reply".to_owned());
        }
        if post.poll.is_some() && !post.locked {
            commands.push("v: vote".to_owned());
        }
        if post.author == app.handle {
            commands.push("e: edit post".to_owned());
        }
        if !post.replies.is_empty() {
            commands.push("↑/↓: select reply".to_owned());
            if post
                .replies
                .get(app.reply_selected)
                .is_some_and(|reply| reply.author == app.handle)
            {
                commands.push("E/D: edit/delete reply".to_owned());
            }
        }
        if app.groups.iter().any(|group| group == "wheel") {
            commands.push(if post.locked {
                "l: unlock".to_owned()
            } else {
                "l: lock".to_owned()
            });
        }
        commands.join(" · ")
    };
    lines.push(Line::styled(help, Style::default().fg(Color::DarkGray)));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Post ").borders(Borders::ALL)),
        area,
    );
}

fn draw_editor(frame: &mut ratatui::Frame<'_>, editor: &Editor, area: Rect) {
    frame.render_widget(Clear, area);
    if editor.target.is_reply() {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(2)])
            .split(area);
        frame.render_widget(
            Paragraph::new(editor.body.as_str())
                .wrap(Wrap { trim: false })
                .block(editor_block(" Reply ", true)),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "/{} · Ctrl-S: save · Esc: cancel",
                editor.board_slug
            ))
            .style(Style::default().fg(Color::DarkGray)),
            sections[1],
        );
        return;
    }
    let constraints = if editor.creates_poll {
        vec![
            Constraint::Length(3),
            Constraint::Percentage(45),
            Constraint::Percentage(35),
            Constraint::Length(2),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ]
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    frame.render_widget(
        Paragraph::new(editor.title.as_str()).block(editor_block(
            " Title ",
            matches!(editor.field, EditorField::Title),
        )),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(editor.body.as_str())
            .wrap(Wrap { trim: false })
            .block(editor_block(
                " Body ",
                matches!(editor.field, EditorField::Body),
            )),
        sections[1],
    );
    let help_index = if editor.creates_poll {
        frame.render_widget(
            Paragraph::new(editor.options.as_str())
                .wrap(Wrap { trim: false })
                .block(editor_block(
                    " Poll options — one per line ",
                    matches!(editor.field, EditorField::Options),
                )),
            sections[2],
        );
        3
    } else {
        2
    };
    frame.render_widget(
        Paragraph::new(format!(
            "/{} · Tab: switch field · Ctrl-S: save · Esc: cancel",
            editor.board_slug
        ))
        .style(Style::default().fg(Color::DarkGray)),
        sections[help_index],
    );
}

fn editor_block(title: &'static str, selected: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if selected {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default()
        })
}

fn draw_confirmation(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    action: ConfirmAction,
    yes: bool,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let question = match action {
        ConfirmAction::DeletePost => {
            let title = app
                .selected_post()
                .map(|post| post.title.as_str())
                .unwrap_or("this post");
            format!("Delete “{title}”?")
        }
        ConfirmAction::DeleteReply(id) => format!("Delete reply #{id}?"),
        ConfirmAction::SetLocked(locked) => {
            let title = app
                .viewed
                .as_ref()
                .map(|post| post.title.as_str())
                .unwrap_or("this post");
            if locked {
                format!("Lock “{title}”?")
            } else {
                format!("Unlock “{title}”?")
            }
        }
    };
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::LightCyan);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw(question),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" Yes ", if yes { selected } else { normal }),
                Span::raw("    "),
                Span::styled(" No ", if yes { normal } else { selected }),
            ]),
            Line::raw(""),
            Line::styled(
                "←/→ or Tab: choose · Enter: confirm · Esc: cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title(" Confirm ").borders(Borders::ALL)),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let rows = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(rows[1])[1]
}
