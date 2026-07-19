use std::{
    env, fs, io,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{self, Command},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::{Terminal, backend::CrosstermBackend};
use salyut_bbs::{
    client::Client,
    protocol::{Board, BoardKind, Post, PostSummary, ProposalState},
};

mod ui;

const TITLE_MARKER: &str = "# Title goes below this line";
const BODY_MARKER: &str = "# Post body goes below this line";

#[derive(Parser)]
#[command(version, about = "salyut.one BBS client")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
}

enum Mode {
    Browse,
    View,
    Vote(usize),
    Confirm(ConfirmAction, bool),
}

#[derive(Clone, Copy)]
enum ConfirmAction {
    DeletePost,
    DeleteReply(i64),
    SetLocked(bool),
    WithdrawProposal,
}

struct Draft {
    target: EditorTarget,
    board_slug: String,
    title: String,
    body: String,
    creates_proposal: bool,
}

#[derive(Clone, Copy)]
enum EditorTarget {
    NewPost,
    EditPost(i64),
    NewReply(i64),
    EditReply { id: i64 },
    VetoProposal(i64),
    ImplementProposal(i64),
}

impl EditorTarget {
    fn returns_to_view(&self) -> bool {
        matches!(
            self,
            Self::NewReply(_)
                | Self::EditReply { .. }
                | Self::VetoProposal(_)
                | Self::ImplementProposal(_)
        )
    }

    fn needs_title(self) -> bool {
        matches!(self, Self::NewPost | Self::EditPost(_))
    }
}

impl Draft {
    fn new(
        target: EditorTarget,
        board_slug: String,
        title: String,
        body: String,
        creates_proposal: bool,
    ) -> Self {
        Self {
            target,
            board_slug,
            title,
            body,
            creates_proposal,
        }
    }

    fn contents(&self) -> String {
        if self.target.needs_title() {
            format!(
                "{TITLE_MARKER}\n{}\n{BODY_MARKER}\n{}\n",
                self.title, self.body
            )
        } else {
            format!("{BODY_MARKER}\n{}\n", self.body)
        }
    }

    fn apply(&mut self, contents: &str) -> Result<()> {
        let contents = contents
            .strip_prefix(if self.target.needs_title() {
                TITLE_MARKER
            } else {
                BODY_MARKER
            })
            .context("draft instructions are missing or were changed")?
            .strip_prefix('\n')
            .context("draft instructions must be followed by a newline")?;

        if self.target.needs_title() {
            let separator = format!("\n{BODY_MARKER}\n");
            let (title, body) = contents
                .split_once(&separator)
                .context("post body instruction is missing or was changed")?;
            let title = title.trim();
            if title.contains('\n') {
                bail!("title must be a single line");
            }
            self.title = title.to_owned();
            self.body = body.trim_end().to_owned();
        } else {
            self.body = contents.trim_end().to_owned();
        }
        Ok(())
    }

    fn edit(&mut self) -> Result<bool> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = env::temp_dir().join(format!("salyut-bbs-draft-{}-{nonce}.txt", process::id()));
        let initial = self.contents();
        let result = (|| {
            use io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("create draft {}", path.display()))?;
            file.write_all(initial.as_bytes())?;
            drop(file);
            let status = Command::new("sh")
                .args(["-c", r#"${VISUAL:-${EDITOR:-vi}} "$1""#, "salyut-bbs"])
                .arg(&path)
                .status()
                .context("start editor")?;
            if !status.success() {
                return Ok(false);
            }
            let contents = fs::read_to_string(&path)?;
            if contents.trim_end() == initial.trim_end() {
                return Ok(false);
            }
            self.apply(&contents)?;
            Ok(true)
        })();
        let _ = fs::remove_file(path);
        result
    }
}

struct App {
    client: Client,
    handle: String,
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
    let handle = client.handle()?;
    let boards = client.boards()?;
    if boards.is_empty() {
        bail!("daemon returned no boards");
    }
    let mut app = App {
        client,
        handle,
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

    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    resume(&mut terminal)?;
    let result = run(&mut terminal, &mut app);
    let cleanup = suspend(&mut terminal);
    result.and(cleanup)
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            handle_key(terminal, app, key)?;
        }
    }
    Ok(())
}

fn resume(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    terminal.clear()?;
    Ok(())
}

fn suspend(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    Ok(())
}

fn handle_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    key: KeyEvent,
) -> Result<()> {
    let mode = std::mem::replace(&mut app.mode, Mode::Browse);
    app.mode = match mode {
        Mode::Browse => handle_browse_key(terminal, app, key)?,
        Mode::View => handle_view_key(terminal, app, key)?,
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
                        ConfirmAction::WithdrawProposal => app.withdraw_proposal(),
                    }
                }
                confirmation_destination(action)
            }
            KeyCode::Esc | KeyCode::Char('q') => confirmation_destination(action),
            _ => Mode::Confirm(action, yes),
        },
    };
    Ok(())
}

fn handle_browse_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    key: KeyEvent,
) -> Result<Mode> {
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Right | KeyCode::Char(']') | KeyCode::Tab => app.next_board(),
        KeyCode::Left | KeyCode::Char('[') | KeyCode::BackTab => app.previous_board(),
        KeyCode::Char('n') => {
            let board = app.current_board();
            app.edit(
                terminal,
                Draft::new(
                    EditorTarget::NewPost,
                    board.slug.clone(),
                    String::new(),
                    String::new(),
                    board.kind == BoardKind::Polls,
                ),
            )?;
        }
        KeyCode::Char('e') => {
            if let Some(post) = app.load_selected() {
                app.edit(
                    terminal,
                    Draft::new(
                        EditorTarget::EditPost(post.id),
                        post.board.slug,
                        post.title,
                        post.body,
                        false,
                    ),
                )?;
            }
        }
        KeyCode::Char('d') => {
            if app
                .selected_post()
                .is_some_and(|post| post.proposal_state.is_some())
            {
                app.message = "Open a proposal and press w to withdraw it".to_owned();
            } else if app.selected_post().is_some() {
                return Ok(Mode::Confirm(ConfirmAction::DeletePost, false));
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
                return Ok(Mode::View);
            }
        }
        _ => {}
    }
    Ok(Mode::Browse)
}

fn handle_view_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    key: KeyEvent,
) -> Result<Mode> {
    Ok(match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => Mode::Browse,
        KeyCode::Char('v') => {
            if let Some(proposal) = app.viewed.as_ref().and_then(|post| post.proposal.as_ref())
                && proposal.state != ProposalState::Voting
            {
                app.message = format!(
                    "Voting is closed; this proposal is {}",
                    proposal.state.label()
                );
                return Ok(Mode::View);
            }
            if app.viewed.as_ref().is_some_and(|post| {
                post.proposal
                    .as_ref()
                    .is_some_and(|proposal| proposal.state == ProposalState::Voting)
                    && post
                        .poll
                        .as_ref()
                        .is_some_and(|poll| !poll.options.is_empty())
            }) {
                Mode::Vote(0)
            } else {
                app.message = "This post has no poll".to_owned();
                Mode::View
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = &app.viewed {
                app.edit(
                    terminal,
                    Draft::new(
                        EditorTarget::EditPost(post.id),
                        post.board.slug.clone(),
                        post.title.clone(),
                        post.body.clone(),
                        false,
                    ),
                )?;
            }
            Mode::View
        }
        KeyCode::Char('a') => {
            if let Some(post) = &app.viewed {
                app.edit(
                    terminal,
                    Draft::new(
                        EditorTarget::NewReply(post.id),
                        post.board.slug.clone(),
                        String::new(),
                        String::new(),
                        false,
                    ),
                )?;
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
        KeyCode::Char('u') => {
            let Some(post) = &app.viewed else {
                return Ok(Mode::View);
            };
            let Some(reply) = post.replies.get(app.reply_selected) else {
                app.message = "There is no reply selected".to_owned();
                return Ok(Mode::View);
            };
            app.edit(
                terminal,
                Draft::new(
                    EditorTarget::EditReply { id: reply.id },
                    post.board.slug.clone(),
                    String::new(),
                    reply.body.clone(),
                    false,
                ),
            )?;
            Mode::View
        }
        KeyCode::Char('d') => {
            let Some(reply) = app
                .viewed
                .as_ref()
                .and_then(|post| post.replies.get(app.reply_selected))
            else {
                app.message = "There is no reply selected".to_owned();
                return Ok(Mode::View);
            };
            Mode::Confirm(ConfirmAction::DeleteReply(reply.id), false)
        }
        KeyCode::Char('l') => {
            let Some(post) = &app.viewed else {
                return Ok(Mode::View);
            };
            Mode::Confirm(ConfirmAction::SetLocked(!post.locked), false)
        }
        KeyCode::Char('w') => {
            if app.viewed.is_none() {
                return Ok(Mode::View);
            }
            Mode::Confirm(ConfirmAction::WithdrawProposal, false)
        }
        KeyCode::Char('x') | KeyCode::Char('i') => {
            let Some(post) = &app.viewed else {
                return Ok(Mode::View);
            };
            let target = if key.code == KeyCode::Char('x') {
                EditorTarget::VetoProposal(post.id)
            } else {
                EditorTarget::ImplementProposal(post.id)
            };
            app.edit(
                terminal,
                Draft::new(
                    target,
                    post.board.slug.clone(),
                    String::new(),
                    String::new(),
                    false,
                ),
            )?;
            Mode::View
        }
        _ => Mode::View,
    })
}

fn confirmation_destination(action: ConfirmAction) -> Mode {
    match action {
        ConfirmAction::DeletePost => Mode::Browse,
        ConfirmAction::DeleteReply(_)
        | ConfirmAction::SetLocked(_)
        | ConfirmAction::WithdrawProposal => Mode::View,
    }
}

impl App {
    fn current_board(&self) -> &Board {
        &self.boards[self.board_selected]
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

    fn edit(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        mut draft: Draft,
    ) -> Result<()> {
        suspend(terminal)?;
        let edited = draft.edit();
        resume(terminal)?;
        if !edited? {
            self.message = "Edit cancelled".to_owned();
            return Ok(());
        }
        let result = match draft.target {
            EditorTarget::EditPost(id) => self.client.update_post(id, &draft.title, &draft.body),
            EditorTarget::NewReply(post_id) => self.client.create_reply(post_id, &draft.body),
            EditorTarget::EditReply { id } => self.client.update_reply(id, &draft.body),
            EditorTarget::NewPost if draft.creates_proposal => {
                self.client
                    .create_proposal(&draft.board_slug, &draft.title, &draft.body)
            }
            EditorTarget::VetoProposal(id) => self.client.veto_proposal(id, &draft.body),
            EditorTarget::ImplementProposal(id) => {
                self.client.mark_proposal_implemented(id, &draft.body)
            }
            EditorTarget::NewPost => {
                self.client
                    .create_post(&draft.board_slug, &draft.title, &draft.body)
            }
        };
        match result {
            Ok(post) => {
                self.message = match draft.target {
                    EditorTarget::EditPost(_) => "Post updated",
                    EditorTarget::NewPost if draft.creates_proposal => "Proposal created",
                    EditorTarget::NewPost => "Post created",
                    EditorTarget::NewReply(_) => "Reply posted",
                    EditorTarget::EditReply { .. } => "Reply updated",
                    EditorTarget::VetoProposal(_) => "Proposal vetoed",
                    EditorTarget::ImplementProposal(_) => "Proposal marked implemented",
                }
                .to_owned();
                if draft.target.returns_to_view() {
                    self.viewed = Some(post);
                } else {
                    self.refresh();
                }
            }
            Err(error) => self.message = error.to_string(),
        }
        Ok(())
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

    fn withdraw_proposal(&mut self) {
        let Some(post) = &self.viewed else { return };
        match self.client.withdraw_proposal(post.id) {
            Ok(post) => {
                self.viewed = Some(post);
                self.message = "Proposal withdrawn".to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_draft_uses_labelled_title_and_body_sections() {
        let draft = Draft::new(
            EditorTarget::NewPost,
            "general".to_owned(),
            "Creative title".to_owned(),
            "Lorem ipsum dolor sit amet...".to_owned(),
            false,
        );

        assert_eq!(
            draft.contents(),
            "# Title goes below this line\n\
             Creative title\n\
             # Post body goes below this line\n\
             Lorem ipsum dolor sit amet...\n"
        );
    }

    #[test]
    fn post_draft_parser_removes_instruction_lines() {
        let mut draft = Draft::new(
            EditorTarget::NewPost,
            "general".to_owned(),
            String::new(),
            String::new(),
            false,
        );

        draft
            .apply(
                "# Title goes below this line\n\
                 A better title\n\
                 # Post body goes below this line\n\
                 First paragraph.\n\nSecond paragraph.\n",
            )
            .unwrap();

        assert_eq!(draft.title, "A better title");
        assert_eq!(draft.body, "First paragraph.\n\nSecond paragraph.");
    }

    #[test]
    fn body_only_drafts_use_the_body_instruction() {
        let mut draft = Draft::new(
            EditorTarget::NewReply(1),
            "general".to_owned(),
            String::new(),
            "Original reply".to_owned(),
            false,
        );

        assert_eq!(
            draft.contents(),
            "# Post body goes below this line\nOriginal reply\n"
        );
        draft
            .apply("# Post body goes below this line\nEdited reply\n")
            .unwrap();
        assert_eq!(draft.body, "Edited reply");
    }
}
