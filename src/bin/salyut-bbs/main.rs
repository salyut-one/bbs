use std::{io, path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use salyut_bbs::{
    client::Client,
    protocol::{Board, BoardKind, Post, PostSummary},
};

mod ui;

#[derive(Parser)]
#[command(version, about = "salyut.one BBS client")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
}

enum Mode {
    Browse,
    View,
    Edit(Editor),
    Vote(usize),
    Confirm(ConfirmAction, bool),
}

#[derive(Clone, Copy)]
enum ConfirmAction {
    DeletePost,
    DeleteReply(i64),
    SetLocked(bool),
}

struct Editor {
    target: EditorTarget,
    board_slug: String,
    title: String,
    body: String,
    options: String,
    creates_poll: bool,
    field: EditorField,
}

#[derive(Clone, Copy)]
enum EditorTarget {
    NewPost,
    EditPost(i64),
    NewReply(i64),
    EditReply { id: i64 },
}

impl EditorTarget {
    fn is_reply(&self) -> bool {
        matches!(self, Self::NewReply(_) | Self::EditReply { .. })
    }
}

#[derive(Clone, Copy)]
enum EditorField {
    Title,
    Body,
    Options,
}

struct App {
    client: Client,
    handle: String,
    groups: Vec<String>,
    boards: Vec<Board>,
    board_selected: usize,
    posts: Vec<PostSummary>,
    selected: usize,
    reply_selected: usize,
    viewed: Option<Post>,
    mode: Mode,
    message: String,
    quit: bool,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let client = Client::new(arguments.socket);
    let identity = client.identity()?;
    let boards = client.boards()?;
    if boards.is_empty() {
        bail!("daemon returned no boards");
    }
    let mut app = App {
        client,
        handle: identity.handle,
        groups: identity.groups,
        boards,
        board_selected: 0,
        posts: Vec::new(),
        selected: 0,
        reply_selected: 0,
        viewed: None,
        mode: Mode::Browse,
        message: String::new(),
        quit: false,
    };
    app.refresh();

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run(&mut terminal, &mut app);
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            handle_key(app, key);
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    let mode = std::mem::replace(&mut app.mode, Mode::Browse);
    app.mode = match mode {
        Mode::Browse => handle_browse_key(app, key),
        Mode::View => handle_view_key(app, key),
        Mode::Vote(mut selected) => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Mode::View,
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(poll) = app.viewed.as_ref().and_then(|post| post.poll.as_ref())
                    && selected + 1 < poll.options.len()
                {
                    selected += 1;
                }
                Mode::Vote(selected)
            }
            KeyCode::Up | KeyCode::Char('k') => Mode::Vote(selected.saturating_sub(1)),
            KeyCode::Enter => {
                app.cast_vote(selected);
                Mode::View
            }
            _ => Mode::Vote(selected),
        },
        Mode::Confirm(action, mut yes) => match key.code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab => {
                yes = !yes;
                Mode::Confirm(action, yes)
            }
            KeyCode::Enter => {
                if yes {
                    match action {
                        ConfirmAction::DeletePost => app.delete_selected(),
                        ConfirmAction::DeleteReply(id) => app.delete_reply(id),
                        ConfirmAction::SetLocked(locked) => app.set_locked(locked),
                    }
                }
                confirmation_destination(action)
            }
            KeyCode::Esc | KeyCode::Char('q') => confirmation_destination(action),
            _ => Mode::Confirm(action, yes),
        },
        Mode::Edit(mut editor) => {
            if key.code == KeyCode::Esc {
                if editor.target.is_reply() {
                    Mode::View
                } else {
                    Mode::Browse
                }
            } else if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                let is_reply = editor.target.is_reply();
                if app.save(&editor) {
                    if is_reply { Mode::View } else { Mode::Browse }
                } else {
                    Mode::Edit(editor)
                }
            } else {
                edit_key(&mut editor, key);
                Mode::Edit(editor)
            }
        }
    };
}

fn handle_browse_key(app: &mut App, key: KeyEvent) -> Mode {
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Right | KeyCode::Char(']') | KeyCode::Tab => app.next_board(),
        KeyCode::Left | KeyCode::Char('[') | KeyCode::BackTab => app.previous_board(),
        KeyCode::Char('n') => {
            if !app.can_write_current_board() {
                app.message = app.write_denied_message();
            } else {
                let board = app.current_board();
                return Mode::Edit(Editor {
                    target: EditorTarget::NewPost,
                    board_slug: board.slug.clone(),
                    title: String::new(),
                    body: String::new(),
                    options: String::new(),
                    creates_poll: board.kind == BoardKind::Polls,
                    field: EditorField::Title,
                });
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = app.load_selected() {
                if post.author == app.handle {
                    return Mode::Edit(Editor {
                        target: EditorTarget::EditPost(post.id),
                        board_slug: post.board.slug,
                        title: post.title,
                        body: post.body,
                        options: String::new(),
                        creates_poll: false,
                        field: EditorField::Title,
                    });
                }
                app.message = "You can only edit your own posts".to_owned();
            }
        }
        KeyCode::Char('d') => {
            if app.selected_post().is_some() {
                return Mode::Confirm(ConfirmAction::DeletePost, false);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected + 1 < app.posts.len() {
                app.selected += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Enter => {
            if let Some(post) = app.load_selected() {
                app.viewed = Some(post);
                app.reply_selected = 0;
                return Mode::View;
            }
        }
        _ => {}
    }
    Mode::Browse
}

fn handle_view_key(app: &mut App, key: KeyEvent) -> Mode {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => Mode::Browse,
        KeyCode::Char('v') => {
            if app.viewed.as_ref().is_some_and(|post| post.locked) {
                app.message = "This proposal is locked".to_owned();
                return Mode::View;
            }
            if app
                .viewed
                .as_ref()
                .and_then(|post| post.poll.as_ref())
                .is_some_and(|poll| !poll.options.is_empty())
            {
                Mode::Vote(0)
            } else {
                app.message = "This post has no poll".to_owned();
                Mode::View
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = &app.viewed
                && post.author == app.handle
            {
                Mode::Edit(Editor {
                    target: EditorTarget::EditPost(post.id),
                    board_slug: post.board.slug.clone(),
                    title: post.title.clone(),
                    body: post.body.clone(),
                    options: String::new(),
                    creates_poll: false,
                    field: EditorField::Title,
                })
            } else {
                app.message = "You can only edit your own posts".to_owned();
                Mode::View
            }
        }
        KeyCode::Char('a') => {
            if let Some(post) = &app.viewed {
                if post.locked {
                    app.message = "This post is locked".to_owned();
                    return Mode::View;
                }
                return Mode::Edit(Editor {
                    target: EditorTarget::NewReply(post.id),
                    board_slug: post.board.slug.clone(),
                    title: String::new(),
                    body: String::new(),
                    options: String::new(),
                    creates_poll: false,
                    field: EditorField::Body,
                });
            }
            Mode::View
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(post) = &app.viewed
                && app.reply_selected + 1 < post.replies.len()
            {
                app.reply_selected += 1;
            }
            Mode::View
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.reply_selected = app.reply_selected.saturating_sub(1);
            Mode::View
        }
        KeyCode::Char('E') => {
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            let Some(reply) = post.replies.get(app.reply_selected) else {
                app.message = "There is no reply selected".to_owned();
                return Mode::View;
            };
            if reply.author != app.handle {
                app.message = "You can only edit your own replies".to_owned();
                return Mode::View;
            }
            Mode::Edit(Editor {
                target: EditorTarget::EditReply { id: reply.id },
                board_slug: post.board.slug.clone(),
                title: String::new(),
                body: reply.body.clone(),
                options: String::new(),
                creates_poll: false,
                field: EditorField::Body,
            })
        }
        KeyCode::Char('D') => {
            let Some(reply) = app
                .viewed
                .as_ref()
                .and_then(|post| post.replies.get(app.reply_selected))
            else {
                app.message = "There is no reply selected".to_owned();
                return Mode::View;
            };
            if reply.author != app.handle {
                app.message = "You can only delete your own replies".to_owned();
                return Mode::View;
            }
            Mode::Confirm(ConfirmAction::DeleteReply(reply.id), false)
        }
        KeyCode::Char('l') => {
            if !app.groups.iter().any(|group| group == "wheel") {
                app.message = "Locking posts requires Unix group wheel".to_owned();
                return Mode::View;
            }
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            Mode::Confirm(ConfirmAction::SetLocked(!post.locked), false)
        }
        _ => Mode::View,
    }
}

fn confirmation_destination(action: ConfirmAction) -> Mode {
    match action {
        ConfirmAction::DeletePost => Mode::Browse,
        ConfirmAction::DeleteReply(_) | ConfirmAction::SetLocked(_) => Mode::View,
    }
}

fn edit_key(editor: &mut Editor, key: KeyEvent) {
    if editor.target.is_reply() && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        return;
    }
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        editor.field = match (editor.field, editor.creates_poll, key.code) {
            (EditorField::Title, _, KeyCode::Tab) => EditorField::Body,
            (EditorField::Body, true, KeyCode::Tab) => EditorField::Options,
            (EditorField::Body, false, KeyCode::Tab) => EditorField::Title,
            (EditorField::Options, _, KeyCode::Tab) => EditorField::Title,
            (EditorField::Title, true, KeyCode::BackTab) => EditorField::Options,
            (EditorField::Title, false, KeyCode::BackTab) => EditorField::Body,
            (EditorField::Body, _, KeyCode::BackTab) => EditorField::Title,
            (EditorField::Options, _, KeyCode::BackTab) => EditorField::Body,
            _ => editor.field,
        };
        return;
    }
    let value = match editor.field {
        EditorField::Title => &mut editor.title,
        EditorField::Body => &mut editor.body,
        EditorField::Options => &mut editor.options,
    };
    match key.code {
        KeyCode::Backspace => {
            value.pop();
        }
        KeyCode::Enter if !matches!(editor.field, EditorField::Title) => value.push('\n'),
        KeyCode::Char(character)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::SUPER) =>
        {
            value.push(character);
        }
        _ => {}
    }
}

impl App {
    fn current_board(&self) -> &Board {
        &self.boards[self.board_selected]
    }

    fn can_write_current_board(&self) -> bool {
        self.current_board()
            .write_group
            .as_ref()
            .is_none_or(|group| self.groups.iter().any(|candidate| candidate == group))
    }

    fn write_denied_message(&self) -> String {
        self.current_board().write_group.as_ref().map_or_else(
            || format!("You cannot write to {}", self.current_board().name),
            |group| {
                format!(
                    "Writing to {} requires Unix group {group}",
                    self.current_board().name
                )
            },
        )
    }

    fn next_board(&mut self) {
        self.board_selected = (self.board_selected + 1) % self.boards.len();
        self.selected = 0;
        self.refresh();
    }

    fn previous_board(&mut self) {
        self.board_selected = self
            .board_selected
            .checked_sub(1)
            .unwrap_or(self.boards.len() - 1);
        self.selected = 0;
        self.refresh();
    }

    fn refresh(&mut self) {
        let board = self.current_board().slug.clone();
        match self.client.posts(&board, 200, 0) {
            Ok(posts) => {
                self.posts = posts;
                self.selected = if self.posts.is_empty() {
                    0
                } else {
                    self.selected.min(self.posts.len() - 1)
                };
                self.message = format!("{} post(s)", self.posts.len());
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn selected_post(&self) -> Option<&PostSummary> {
        self.posts.get(self.selected)
    }

    fn load_selected(&mut self) -> Option<Post> {
        let id = self.selected_post()?.id;
        match self.client.post(id) {
            Ok(post) => post,
            Err(error) => {
                self.message = error.to_string();
                None
            }
        }
    }

    fn save(&mut self, editor: &Editor) -> bool {
        let result = match editor.target {
            EditorTarget::EditPost(id) => self.client.update_post(id, &editor.title, &editor.body),
            EditorTarget::NewReply(post_id) => self.client.create_reply(post_id, &editor.body),
            EditorTarget::EditReply { id } => self.client.update_reply(id, &editor.body),
            EditorTarget::NewPost if editor.creates_poll => self.client.create_poll(
                &editor.board_slug,
                &editor.title,
                &editor.body,
                editor
                    .options
                    .lines()
                    .map(str::trim)
                    .filter(|option| !option.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
            EditorTarget::NewPost => {
                self.client
                    .create_post(&editor.board_slug, &editor.title, &editor.body)
            }
        };
        match result {
            Ok(post) => {
                self.message = match editor.target {
                    EditorTarget::EditPost(_) => "Post updated",
                    EditorTarget::NewPost if editor.creates_poll => "Proposal created",
                    EditorTarget::NewPost => "Post created",
                    EditorTarget::NewReply(_) => "Reply posted",
                    EditorTarget::EditReply { .. } => "Reply updated",
                }
                .to_owned();
                if editor.target.is_reply() {
                    self.viewed = Some(post);
                } else {
                    self.refresh();
                }
                true
            }
            Err(error) => {
                self.message = error.to_string();
                false
            }
        }
    }

    fn delete_reply(&mut self, id: i64) {
        match self.client.delete_reply(id) {
            Ok(post_id) => match self.client.post(post_id) {
                Ok(Some(post)) => {
                    self.reply_selected = self
                        .reply_selected
                        .min(post.replies.len().saturating_sub(1));
                    self.viewed = Some(post);
                    self.message = "Reply deleted".to_owned();
                }
                Ok(None) => self.message = "Post not found".to_owned(),
                Err(error) => self.message = error.to_string(),
            },
            Err(error) => self.message = error.to_string(),
        }
    }

    fn set_locked(&mut self, locked: bool) {
        let Some(post) = &self.viewed else { return };
        match self.client.set_post_locked(post.id, locked) {
            Ok(post) => {
                self.message = if post.locked {
                    "Post locked"
                } else {
                    "Post unlocked"
                }
                .to_owned();
                self.viewed = Some(post);
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn cast_vote(&mut self, selected: usize) {
        let Some(post) = self.viewed.as_ref() else {
            return;
        };
        let Some(option) = post
            .poll
            .as_ref()
            .and_then(|poll| poll.options.get(selected))
        else {
            return;
        };
        match self.client.vote(post.id, option.id) {
            Ok(post) => {
                self.viewed = Some(post);
                self.message = "Vote recorded; voting again changes your choice".to_owned();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn delete_selected(&mut self) {
        let Some(id) = self.selected_post().map(|post| post.id) else {
            return;
        };
        match self.client.delete_post(id) {
            Ok(()) => {
                self.message = "Post deleted".to_owned();
                self.refresh();
            }
            Err(error) => self.message = error.to_string(),
        }
    }
}
