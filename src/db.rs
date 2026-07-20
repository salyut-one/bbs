use std::io::Read;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::protocol::{
    Board, BoardKind, MailDelivery, Poll, PollOption, Post, PostSummary, Proposal, ProposalEvent,
    ProposalState, Reply,
};

const MAX_TITLE_CHARS: usize = 120;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_TRANSITION_NOTE_BYTES: usize = 4 * 1024;
const MAX_MAIL_ERROR_BYTES: usize = 4 * 1024;
const MAX_MESSAGE_ID_BYTES: usize = 998;
const MAIL_LEASE_MINUTES: i64 = 5;
const MAIL_MAX_ATTEMPTS: u32 = 3;
pub const PROPOSAL_VOTING_DAYS: i64 = 7;
const PROPOSAL_OPTIONS: [&str; 3] = ["For", "Against", "Abstain"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailRecipient {
    pub uid: u32,
    pub username: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportedMailReply {
    pub post_id: i64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportedMailPost {
    pub post_id: i64,
    pub duplicate: bool,
}

pub struct MailPostImport<'a> {
    pub board: &'a Board,
    pub uid: u32,
    pub author: &'a str,
    pub message_id: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub recipients: &'a [MailRecipient],
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let mut connection =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;

                 CREATE TABLE IF NOT EXISTS boards (
                     id           INTEGER PRIMARY KEY,
                     slug         TEXT NOT NULL UNIQUE,
                     name         TEXT NOT NULL,
                     description  TEXT NOT NULL,
                     kind         TEXT NOT NULL CHECK (kind IN ('discussion', 'polls')),
                     write_group  TEXT
                 );

                 INSERT INTO boards
                     (id, slug, name, description, kind, write_group)
                 VALUES
                     (1, 'general', 'General',
                      'Anything that does not fit elsewhere.', 'discussion', NULL),
                     (2, 'updates', 'Updates',
                      'Service notices and maintenance updates.', 'discussion', 'wheel'),
                     (3, 'proposals', 'Proposals',
                      'Seven-day votes about changes to salyut.one.', 'polls', NULL)
                 ON CONFLICT(id) DO UPDATE SET
                     slug = excluded.slug,
                     name = excluded.name,
                     description = excluded.description,
                     kind = excluded.kind,
                     write_group = excluded.write_group;

                 CREATE TABLE IF NOT EXISTS posts (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     board_id    INTEGER NOT NULL REFERENCES boards(id),
                     author_uid  INTEGER NOT NULL,
                     author      TEXT NOT NULL,
                     title       TEXT NOT NULL,
                     body        TEXT NOT NULL,
                     locked      INTEGER NOT NULL DEFAULT 0,
                     created_at  TEXT NOT NULL,
                     updated_at  TEXT NOT NULL
                 );",
            )
            .context("initialize database schema")?;

        if !has_column(&connection, "posts", "board_id")? {
            connection
                .execute_batch(
                    "ALTER TABLE posts ADD COLUMN board_id INTEGER;
                     UPDATE posts SET board_id = 1 WHERE board_id IS NULL;",
                )
                .context("migrate existing posts into the General board")?;
        }
        if !has_column(&connection, "posts", "locked")? {
            connection
                .execute(
                    "ALTER TABLE posts ADD COLUMN locked INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .context("add post locking")?;
        }

        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS posts_board_updated_at
                     ON posts(board_id, updated_at DESC, id DESC);

                 CREATE TABLE IF NOT EXISTS poll_options (
                     id        INTEGER PRIMARY KEY AUTOINCREMENT,
                     post_id   INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                     label     TEXT NOT NULL,
                     position  INTEGER NOT NULL,
                     UNIQUE(post_id, position)
                 );

                 CREATE TABLE IF NOT EXISTS poll_votes (
                     post_id    INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                     option_id  INTEGER NOT NULL REFERENCES poll_options(id) ON DELETE CASCADE,
                     voter_uid  INTEGER NOT NULL,
                     voted_at   TEXT NOT NULL,
                     PRIMARY KEY(post_id, voter_uid)
                 );

                 CREATE INDEX IF NOT EXISTS poll_votes_option
                     ON poll_votes(option_id);

                 CREATE TABLE IF NOT EXISTS replies (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     post_id     INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                     author_uid  INTEGER NOT NULL,
                     author      TEXT NOT NULL,
                     body        TEXT NOT NULL,
                     created_at  TEXT NOT NULL,
                     updated_at  TEXT NOT NULL
                 );

                 CREATE INDEX IF NOT EXISTS replies_post_created_at
                     ON replies(post_id, created_at, id);

                 CREATE TABLE IF NOT EXISTS proposals (
                     post_id     INTEGER PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
                     state       TEXT NOT NULL CHECK (state IN (
                         'voting', 'accepted', 'rejected', 'withdrawn',
                         'vetoed', 'implemented'
                     )),
                     opens_at    TEXT NOT NULL,
                     closes_at   TEXT NOT NULL,
                     closed_at   TEXT
                 );

                 CREATE INDEX IF NOT EXISTS proposals_due
                     ON proposals(state, closes_at);

                 CREATE TABLE IF NOT EXISTS proposal_events (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     post_id     INTEGER NOT NULL REFERENCES proposals(post_id) ON DELETE CASCADE,
                     from_state  TEXT,
                     to_state    TEXT NOT NULL,
                     actor_uid   INTEGER,
                     actor       TEXT,
                     reason      TEXT,
                     created_at  TEXT NOT NULL
                 );

                 CREATE INDEX IF NOT EXISTS proposal_events_post
                     ON proposal_events(post_id, created_at, id);

                 CREATE TABLE IF NOT EXISTS mail_preferences (
                     board_id          INTEGER NOT NULL REFERENCES boards(id),
                     user_uid          INTEGER NOT NULL,
                     username          TEXT NOT NULL,
                     subscribed        INTEGER NOT NULL DEFAULT 1,
                     unsubscribe_token TEXT NOT NULL UNIQUE,
                     updated_at        TEXT NOT NULL,
                     PRIMARY KEY(board_id, user_uid)
                 );

                 CREATE TABLE IF NOT EXISTS mail_thread_tokens (
                     post_id     INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                     user_uid    INTEGER NOT NULL,
                     token       TEXT NOT NULL UNIQUE,
                     created_at  TEXT NOT NULL,
                     PRIMARY KEY(post_id, user_uid)
                 );

                 CREATE TABLE IF NOT EXISTS mail_events (
                     id           INTEGER PRIMARY KEY AUTOINCREMENT,
                     board_id     INTEGER NOT NULL REFERENCES boards(id),
                     post_id      INTEGER NOT NULL,
                     reply_id     INTEGER,
                     author       TEXT NOT NULL,
                     subject      TEXT NOT NULL,
                     body         TEXT NOT NULL,
                     message_id   TEXT NOT NULL UNIQUE,
                     in_reply_to  TEXT,
                     created_at   TEXT NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS mail_deliveries (
                     id               INTEGER PRIMARY KEY AUTOINCREMENT,
                     event_id         INTEGER NOT NULL REFERENCES mail_events(id) ON DELETE CASCADE,
                     user_uid         INTEGER NOT NULL,
                     recipient        TEXT NOT NULL,
                     state            TEXT NOT NULL CHECK (
                          state IN ('pending', 'leased', 'retry', 'delivered', 'cancelled',
                                    'failed')
                     ),
                     attempts         INTEGER NOT NULL DEFAULT 0,
                     available_at     TEXT NOT NULL,
                     lease_until      TEXT,
                     last_error       TEXT,
                     delivered_at     TEXT,
                     UNIQUE(event_id, user_uid)
                 );

                 CREATE INDEX IF NOT EXISTS mail_deliveries_ready
                     ON mail_deliveries(state, available_at, lease_until, id);

                 CREATE TABLE IF NOT EXISTS mail_inbound (
                     message_id   TEXT PRIMARY KEY,
                     post_id      INTEGER NOT NULL,
                     reply_id     INTEGER NOT NULL,
                     received_at  TEXT NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS mail_post_inbound (
                     message_id   TEXT PRIMARY KEY,
                     post_id      INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                     received_at  TEXT NOT NULL
                 );",
            )
            .context("initialize poll, reply, proposal, and mail schema")?;

        migrate_legacy_proposals(&mut connection)?;
        migrate_mail_delivery_states(&mut connection)?;

        Ok(Self { connection })
    }

    pub fn boards(&self) -> Result<Vec<Board>> {
        let mut statement = self.connection.prepare(
            "SELECT id, slug, name, description, kind, write_group
             FROM boards ORDER BY id",
        )?;
        let rows = statement.query_map([], board_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read boards")
    }

    pub fn board(&self, slug: &str) -> Result<Option<Board>> {
        self.connection
            .query_row(
                "SELECT id, slug, name, description, kind, write_group
                 FROM boards WHERE slug = ?1",
                [slug],
                board_from_row,
            )
            .optional()
            .context("read board")
    }

    pub fn list(
        &self,
        board_slug: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Option<Vec<PostSummary>>> {
        let Some(board) = self.board(board_slug)? else {
            return Ok(None);
        };
        let limit = limit.clamp(1, 200);
        let mut statement = self.connection.prepare(
            "SELECT p.id, b.slug, p.author, p.title,
                    EXISTS(SELECT 1 FROM poll_options po WHERE po.post_id = p.id),
                    pr.state, p.locked,
                    (SELECT COUNT(*) FROM replies r WHERE r.post_id = p.id),
                    p.created_at, p.updated_at
             FROM posts p
             JOIN boards b ON b.id = p.board_id
             LEFT JOIN proposals pr ON pr.post_id = p.id
             WHERE p.board_id = ?1
             ORDER BY p.updated_at DESC, p.id DESC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(params![board.id, limit, offset], |row| {
            Ok(PostSummary {
                id: row.get(0)?,
                board_slug: row.get(1)?,
                author: row.get(2)?,
                title: row.get(3)?,
                is_poll: row.get(4)?,
                proposal_state: row
                    .get::<_, Option<String>>(5)?
                    .map(parse_proposal_state)
                    .transpose()?,
                locked: row.get(6)?,
                reply_count: row.get(7)?,
                created_at: parse_timestamp(row.get::<_, String>(8)?)?,
                updated_at: parse_timestamp(row.get::<_, String>(9)?)?,
            })
        })?;
        Ok(Some(
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("read posts")?,
        ))
    }

    pub fn get(&self, id: i64, viewer_uid: u32) -> Result<Option<Post>> {
        let Some(mut post) = self
            .connection
            .query_row(
                "SELECT p.id, b.id, b.slug, b.name, b.description, b.kind,
                        b.write_group, p.author, p.title, p.body,
                        p.locked, p.created_at, p.updated_at
                 FROM posts p
                 JOIN boards b ON b.id = p.board_id
                 WHERE p.id = ?1",
                [id],
                post_from_row,
            )
            .optional()
            .context("read post")?
        else {
            return Ok(None);
        };
        if post.board.kind == BoardKind::Polls {
            post.poll = Some(self.poll(id, viewer_uid)?);
            post.proposal = self.proposal(id)?;
        }
        post.replies = self.replies(id)?;
        Ok(Some(post))
    }

    pub fn create(
        &mut self,
        board: &Board,
        uid: u32,
        author: &str,
        title: &str,
        body: &str,
    ) -> Result<Post> {
        self.create_with_mail(board, uid, author, title, body, &[])
    }

    pub fn create_with_mail(
        &mut self,
        board: &Board,
        uid: u32,
        author: &str,
        title: &str,
        body: &str,
        recipients: &[MailRecipient],
    ) -> Result<Post> {
        validate_post(title, body)?;
        let timestamp = Utc::now().to_rfc3339();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO posts
                 (board_id, author_uid, author, title, body, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![board.id, uid, author, title.trim(), body, timestamp],
        )?;
        let id = transaction.last_insert_rowid();
        enqueue_mail_event(
            &transaction,
            board,
            id,
            None,
            author,
            title.trim(),
            body,
            &format!("<bbs-post-{id}@salyut.one>"),
            None,
            &timestamp,
            recipients,
        )?;
        transaction.commit()?;
        self.get(id, uid)?.context("newly created post disappeared")
    }

    pub fn create_proposal(
        &mut self,
        board: &Board,
        uid: u32,
        author: &str,
        title: &str,
        body: &str,
    ) -> Result<Post> {
        self.create_proposal_with_mail(board, uid, author, title, body, &[])
    }

    pub fn create_proposal_with_mail(
        &mut self,
        board: &Board,
        uid: u32,
        author: &str,
        title: &str,
        body: &str,
        recipients: &[MailRecipient],
    ) -> Result<Post> {
        validate_post(title, body)?;
        let opened_at = Utc::now();
        let timestamp = opened_at.to_rfc3339();
        let closes_at = (opened_at + Duration::days(PROPOSAL_VOTING_DAYS)).to_rfc3339();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO posts
                 (board_id, author_uid, author, title, body, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![board.id, uid, author, title.trim(), body, timestamp],
        )?;
        let id = transaction.last_insert_rowid();
        {
            let mut statement = transaction.prepare(
                "INSERT INTO poll_options (post_id, label, position)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (position, option) in PROPOSAL_OPTIONS.iter().enumerate() {
                statement.execute(params![id, option, position as i64])?;
            }
        }
        transaction.execute(
            "INSERT INTO proposals (post_id, state, opens_at, closes_at)
             VALUES (?1, 'voting', ?2, ?3)",
            params![id, timestamp, closes_at],
        )?;
        transaction.execute(
            "INSERT INTO proposal_events
                 (post_id, from_state, to_state, actor_uid, actor, created_at)
             VALUES (?1, NULL, 'voting', ?2, ?3, ?4)",
            params![id, uid, author, timestamp],
        )?;
        enqueue_mail_event(
            &transaction,
            board,
            id,
            None,
            author,
            title.trim(),
            body,
            &format!("<bbs-post-{id}@salyut.one>"),
            None,
            &timestamp,
            recipients,
        )?;
        transaction.commit()?;
        self.get(id, uid)?
            .context("newly created proposal disappeared")
    }

    pub fn update(&mut self, uid: u32, id: i64, title: &str, body: &str) -> Result<Option<Post>> {
        validate_post(title, body)?;
        let changed = self.connection.execute(
            "UPDATE posts SET title = ?1, body = ?2, updated_at = ?3
             WHERE id = ?4 AND author_uid = ?5",
            params![title.trim(), body, Utc::now().to_rfc3339(), id, uid],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(id, uid)
    }

    pub fn delete(&mut self, uid: u32, id: i64) -> Result<bool> {
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "DELETE FROM posts WHERE id = ?1 AND author_uid = ?2",
            params![id, uid],
        )?;
        if changed != 0 {
            transaction.execute(
                "UPDATE mail_deliveries
                 SET state = 'cancelled', lease_until = NULL
                 WHERE state IN ('pending', 'retry')
                   AND event_id IN (SELECT id FROM mail_events WHERE post_id = ?1)",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok(changed != 0)
    }

    pub fn vote(&mut self, uid: u32, post_id: i64, option_id: i64) -> Result<Option<Post>> {
        let timestamp = Utc::now().to_rfc3339();
        let transaction = self.connection.transaction()?;
        let valid: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM poll_options po
                 JOIN posts p ON p.id = po.post_id
                 JOIN boards b ON b.id = p.board_id
                 WHERE po.id = ?1 AND po.post_id = ?2
                   AND b.kind = 'polls'
                   AND EXISTS (
                       SELECT 1 FROM proposals pr
                       WHERE pr.post_id = p.id AND pr.state = 'voting'
                         AND pr.closes_at > ?3
                   )
             )",
            params![option_id, post_id, timestamp],
            |row| row.get(0),
        )?;
        if !valid {
            return Ok(None);
        }
        transaction.execute(
            "INSERT INTO poll_votes (post_id, option_id, voter_uid, voted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(post_id, voter_uid) DO UPDATE SET
                 option_id = excluded.option_id,
                 voted_at = excluded.voted_at",
            params![post_id, option_id, uid, timestamp],
        )?;
        transaction.commit()?;
        self.get(post_id, uid)
    }

    pub fn create_reply(
        &mut self,
        uid: u32,
        author: &str,
        post_id: i64,
        body: &str,
    ) -> Result<Option<Post>> {
        self.create_reply_with_mail(uid, author, post_id, body, &[])
    }

    pub fn create_reply_with_mail(
        &mut self,
        uid: u32,
        author: &str,
        post_id: i64,
        body: &str,
        recipients: &[MailRecipient],
    ) -> Result<Option<Post>> {
        validate_body(body)?;
        let timestamp = Utc::now().to_rfc3339();
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT INTO replies
                 (post_id, author_uid, author, body, created_at, updated_at)
             SELECT id, ?1, ?2, ?3, ?4, ?4
             FROM posts WHERE id = ?5 AND locked = 0",
            params![uid, author, body, timestamp, post_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let reply_id = transaction.last_insert_rowid();
        let (board, title) = mail_post_context(&transaction, post_id)?
            .context("post disappeared while creating reply")?;
        transaction.execute(
            "UPDATE posts SET updated_at = ?1 WHERE id = ?2",
            params![timestamp, post_id],
        )?;
        enqueue_mail_event(
            &transaction,
            &board,
            post_id,
            Some(reply_id),
            author,
            &format!("Re: {title}"),
            body,
            &format!("<bbs-reply-{reply_id}@salyut.one>"),
            Some(&format!("<bbs-post-{post_id}@salyut.one>")),
            &timestamp,
            recipients,
        )?;
        transaction.commit()?;
        self.get(post_id, uid)
    }

    pub fn update_reply(&mut self, uid: u32, id: i64, body: &str) -> Result<Option<i64>> {
        validate_body(body)?;
        let post_id = self.reply_post_id(id)?;
        let changed = self.connection.execute(
            "UPDATE replies SET body = ?1, updated_at = ?2
             WHERE id = ?3 AND author_uid = ?4",
            params![body, Utc::now().to_rfc3339(), id, uid],
        )?;
        Ok((changed != 0).then_some(post_id).flatten())
    }

    pub fn delete_reply(&mut self, uid: u32, id: i64) -> Result<Option<i64>> {
        let post_id = self.reply_post_id(id)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "DELETE FROM replies WHERE id = ?1 AND author_uid = ?2",
            params![id, uid],
        )?;
        if changed != 0 {
            transaction.execute(
                "UPDATE mail_deliveries
                 SET state = 'cancelled', lease_until = NULL
                 WHERE state IN ('pending', 'retry')
                   AND event_id IN (SELECT id FROM mail_events WHERE reply_id = ?1)",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok((changed != 0).then_some(post_id).flatten())
    }

    pub fn reply_owner_uid(&self, id: i64) -> Result<Option<u32>> {
        self.connection
            .query_row(
                "SELECT author_uid FROM replies WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .context("read reply owner")
    }

    pub fn set_locked(&mut self, id: i64, locked: bool, viewer_uid: u32) -> Result<Option<Post>> {
        let changed = self.connection.execute(
            "UPDATE posts SET locked = ?1 WHERE id = ?2",
            params![locked, id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(id, viewer_uid)
    }

    pub fn finalize_due_proposals(&mut self, now: DateTime<Utc>) -> Result<usize> {
        let due = {
            let mut statement = self.connection.prepare(
                "SELECT post_id FROM proposals
                 WHERE state = 'voting' AND closes_at <= ?1
                 ORDER BY post_id",
            )?;
            statement
                .query_map([now.to_rfc3339()], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut finalized = 0;
        for post_id in due {
            let (for_votes, against_votes, _) = self.proposal_tally(post_id)?;
            let state = if for_votes > against_votes {
                ProposalState::Accepted
            } else {
                ProposalState::Rejected
            };
            if self.transition_proposal(
                post_id,
                ProposalState::Voting,
                state,
                None,
                None,
                None,
                now,
            )? {
                finalized += 1;
            }
        }
        Ok(finalized)
    }

    pub fn withdraw_proposal(
        &mut self,
        post_id: i64,
        uid: u32,
        actor: &str,
    ) -> Result<Option<Post>> {
        let now = Utc::now();
        if !self.transition_proposal(
            post_id,
            ProposalState::Voting,
            ProposalState::Withdrawn,
            Some(uid),
            Some(actor),
            None,
            now,
        )? {
            return Ok(None);
        }
        self.get(post_id, uid)
    }

    pub fn veto_proposal(
        &mut self,
        post_id: i64,
        uid: u32,
        actor: &str,
        reason: &str,
    ) -> Result<Option<Post>> {
        validate_transition_note("veto reason", reason)?;
        let now = Utc::now();
        if !self.transition_proposal(
            post_id,
            ProposalState::Accepted,
            ProposalState::Vetoed,
            Some(uid),
            Some(actor),
            Some(reason.trim()),
            now,
        )? {
            return Ok(None);
        }
        self.get(post_id, uid)
    }

    pub fn mark_proposal_implemented(
        &mut self,
        post_id: i64,
        uid: u32,
        actor: &str,
        note: &str,
    ) -> Result<Option<Post>> {
        validate_transition_note("implementation note", note)?;
        let now = Utc::now();
        if !self.transition_proposal(
            post_id,
            ProposalState::Accepted,
            ProposalState::Implemented,
            Some(uid),
            Some(actor),
            Some(note.trim()),
            now,
        )? {
            return Ok(None);
        }
        self.get(post_id, uid)
    }

    pub fn owner_uid(&self, id: i64) -> Result<Option<u32>> {
        self.connection
            .query_row("SELECT author_uid FROM posts WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .context("read post owner")
    }

    fn poll(&self, post_id: i64, viewer_uid: u32) -> Result<Poll> {
        let mut statement = self.connection.prepare(
            "SELECT po.id, po.label, COUNT(pv.voter_uid)
             FROM poll_options po
             LEFT JOIN poll_votes pv ON pv.option_id = po.id
             WHERE po.post_id = ?1
             GROUP BY po.id, po.label, po.position
             ORDER BY po.position",
        )?;
        let options = statement
            .query_map([post_id], |row| {
                Ok(PollOption {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    votes: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total_votes = options.iter().map(|option| option.votes).sum();
        let my_vote = self
            .connection
            .query_row(
                "SELECT option_id FROM poll_votes
                 WHERE post_id = ?1 AND voter_uid = ?2",
                params![post_id, viewer_uid],
                |row| row.get(0),
            )
            .optional()?;
        Ok(Poll {
            options,
            total_votes,
            my_vote,
        })
    }

    fn proposal(&self, post_id: i64) -> Result<Option<Proposal>> {
        let Some((state, opens_at, closes_at, closed_at)) = self
            .connection
            .query_row(
                "SELECT state, opens_at, closes_at, closed_at
                 FROM proposals WHERE post_id = ?1",
                [post_id],
                |row| {
                    Ok((
                        parse_proposal_state(row.get(0)?)?,
                        parse_timestamp(row.get(1)?)?,
                        parse_timestamp(row.get(2)?)?,
                        row.get::<_, Option<String>>(3)?
                            .map(parse_timestamp)
                            .transpose()?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut statement = self.connection.prepare(
            "SELECT id, from_state, to_state, actor_uid, actor, reason, created_at
             FROM proposal_events
             WHERE post_id = ?1
             ORDER BY created_at, id",
        )?;
        let events = statement
            .query_map([post_id], |row| {
                Ok(ProposalEvent {
                    id: row.get(0)?,
                    from_state: row
                        .get::<_, Option<String>>(1)?
                        .map(parse_proposal_state)
                        .transpose()?,
                    to_state: parse_proposal_state(row.get(2)?)?,
                    actor_uid: row.get(3)?,
                    actor: row.get(4)?,
                    reason: row.get(5)?,
                    created_at: parse_timestamp(row.get(6)?)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(Proposal {
            state,
            opens_at,
            closes_at,
            closed_at,
            events,
        }))
    }

    fn proposal_tally(&self, post_id: i64) -> Result<(u32, u32, u32)> {
        let mut statement = self.connection.prepare(
            "SELECT po.position, COUNT(pv.voter_uid)
             FROM poll_options po
             LEFT JOIN poll_votes pv ON pv.option_id = po.id
             WHERE po.post_id = ?1
             GROUP BY po.id, po.position
             ORDER BY po.position",
        )?;
        let mut tally = [0_u32; 3];
        for row in statement.query_map([post_id], |row| {
            Ok((row.get::<_, usize>(0)?, row.get::<_, u32>(1)?))
        })? {
            let (position, votes) = row?;
            if position < 2 {
                tally[position] = tally[position].saturating_add(votes);
            } else {
                tally[2] = tally[2].saturating_add(votes);
            }
        }
        Ok((tally[0], tally[1], tally[2]))
    }

    #[allow(clippy::too_many_arguments)]
    fn transition_proposal(
        &mut self,
        post_id: i64,
        from: ProposalState,
        to: ProposalState,
        actor_uid: Option<u32>,
        actor: Option<&str>,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let timestamp = now.to_rfc3339();
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE proposals
             SET state = ?1, closed_at = CASE
                 WHEN ?1 IN ('accepted', 'rejected', 'withdrawn') THEN ?2
                 ELSE closed_at
             END
             WHERE post_id = ?3 AND state = ?4",
            params![to.label(), timestamp, post_id, from.label()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        transaction.execute(
            "UPDATE posts SET updated_at = ?1 WHERE id = ?2",
            params![timestamp, post_id],
        )?;
        transaction.execute(
            "INSERT INTO proposal_events
                 (post_id, from_state, to_state, actor_uid, actor, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                post_id,
                from.label(),
                to.label(),
                actor_uid,
                actor,
                reason,
                timestamp
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    fn replies(&self, post_id: i64) -> Result<Vec<Reply>> {
        let mut statement = self.connection.prepare(
            "SELECT id, author, body, created_at, updated_at
             FROM replies
             WHERE post_id = ?1
             ORDER BY created_at, id",
        )?;
        statement
            .query_map([post_id], |row| {
                Ok(Reply {
                    id: row.get(0)?,
                    author: row.get(1)?,
                    body: row.get(2)?,
                    created_at: parse_timestamp(row.get::<_, String>(3)?)?,
                    updated_at: parse_timestamp(row.get::<_, String>(4)?)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read replies")
    }

    pub fn mail_subscription(
        &self,
        uid: u32,
        board_slug: &str,
        eligible: bool,
    ) -> Result<Option<bool>> {
        let Some(board) = self.board(board_slug)? else {
            return Ok(None);
        };
        if !eligible {
            return Ok(Some(false));
        }
        let subscribed = self
            .connection
            .query_row(
                "SELECT subscribed FROM mail_preferences
                 WHERE board_id = ?1 AND user_uid = ?2",
                params![board.id, uid],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(true);
        Ok(Some(subscribed))
    }

    pub fn set_mail_subscription(
        &mut self,
        uid: u32,
        username: &str,
        board_slug: &str,
        subscribed: bool,
    ) -> Result<Option<bool>> {
        let Some(board) = self.board(board_slug)? else {
            return Ok(None);
        };
        let now = Utc::now().to_rfc3339();
        let token = random_token()?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO mail_preferences
                 (board_id, user_uid, username, subscribed, unsubscribe_token, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(board_id, user_uid) DO UPDATE SET
                 username = excluded.username,
                 subscribed = excluded.subscribed,
                 updated_at = excluded.updated_at",
            params![board.id, uid, username, subscribed, token, now],
        )?;
        if !subscribed {
            cancel_mail_for_subscription(&transaction, board.id, uid)?;
        }
        transaction.commit()?;
        Ok(Some(subscribed))
    }

    pub fn mail_reply_target(&self, token: &str) -> Result<Option<(i64, u32)>> {
        validate_token(token)?;
        self.connection
            .query_row(
                "SELECT mt.post_id, mt.user_uid
                 FROM mail_thread_tokens mt
                 JOIN posts p ON p.id = mt.post_id
                 JOIN mail_preferences mp
                   ON mp.board_id = p.board_id AND mp.user_uid = mt.user_uid
                 WHERE mt.token = ?1 AND mp.subscribed = 1",
                [token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .context("resolve mail reply token")
    }

    pub fn imported_mail_message(&self, message_id: &str) -> Result<Option<i64>> {
        validate_message_id(message_id)?;
        self.connection
            .query_row(
                "SELECT post_id FROM mail_inbound WHERE message_id = ?1
                 UNION ALL
                 SELECT post_id FROM mail_post_inbound WHERE message_id = ?1
                 LIMIT 1",
                [message_id],
                |row| row.get(0),
            )
            .optional()
            .context("check imported mail message")
    }

    pub fn import_mail_post(&mut self, import: MailPostImport<'_>) -> Result<ImportedMailPost> {
        let MailPostImport {
            board,
            uid,
            author,
            message_id,
            title,
            body,
            recipients,
        } = import;
        validate_message_id(message_id)?;
        if let Some(post_id) = self.imported_mail_message(message_id)? {
            return Ok(ImportedMailPost {
                post_id,
                duplicate: true,
            });
        }
        validate_post(title, body)?;
        let timestamp = Utc::now().to_rfc3339();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO posts
                 (board_id, author_uid, author, title, body, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![board.id, uid, author, title.trim(), body, timestamp],
        )?;
        let post_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO mail_post_inbound (message_id, post_id, received_at)
             VALUES (?1, ?2, ?3)",
            params![message_id, post_id, timestamp],
        )?;
        enqueue_mail_event(
            &transaction,
            board,
            post_id,
            None,
            author,
            title.trim(),
            body,
            &format!("<bbs-post-{post_id}@salyut.one>"),
            None,
            &timestamp,
            recipients,
        )?;
        transaction.commit()?;
        Ok(ImportedMailPost {
            post_id,
            duplicate: false,
        })
    }

    pub fn claim_mail_delivery(&mut self, now: DateTime<Utc>) -> Result<Option<MailDelivery>> {
        let timestamp = now.to_rfc3339();
        let lease_until = (now + Duration::minutes(MAIL_LEASE_MINUTES)).to_rfc3339();
        let transaction = self.connection.transaction()?;
        let delivery = transaction
            .query_row(
                "SELECT d.id, d.recipient, b.slug, e.post_id, e.author, e.subject,
                        e.body, e.message_id, e.in_reply_to, mt.token,
                        mp.unsubscribe_token
                 FROM mail_deliveries d
                 JOIN mail_events e ON e.id = d.event_id
                 JOIN boards b ON b.id = e.board_id
                 JOIN mail_preferences mp
                   ON mp.board_id = e.board_id AND mp.user_uid = d.user_uid
                 JOIN mail_thread_tokens mt
                   ON mt.post_id = e.post_id AND mt.user_uid = d.user_uid
                 WHERE mp.subscribed = 1
                   AND (
                       (d.state IN ('pending', 'retry') AND d.available_at <= ?1)
                       OR (d.state = 'leased' AND d.lease_until <= ?1)
                   )
                 ORDER BY d.id
                 LIMIT 1",
                [&timestamp],
                |row| {
                    Ok(MailDelivery {
                        id: row.get(0)?,
                        recipient: row.get(1)?,
                        board_slug: row.get(2)?,
                        post_id: row.get(3)?,
                        author: row.get(4)?,
                        subject: row.get(5)?,
                        body: row.get(6)?,
                        message_id: row.get(7)?,
                        in_reply_to: row.get(8)?,
                        reply_token: row.get(9)?,
                        unsubscribe_token: row.get(10)?,
                    })
                },
            )
            .optional()?;
        if let Some(delivery) = &delivery {
            transaction.execute(
                "UPDATE mail_deliveries
                 SET state = 'leased', attempts = attempts + 1,
                     lease_until = ?1, last_error = NULL
                 WHERE id = ?2",
                params![lease_until, delivery.id],
            )?;
        }
        transaction.commit()?;
        Ok(delivery)
    }

    pub fn complete_mail_delivery(&mut self, id: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE mail_deliveries
             SET state = 'delivered', delivered_at = ?1, lease_until = NULL
             WHERE id = ?2 AND state = 'leased'",
            params![Utc::now().to_rfc3339(), id],
        )? != 0)
    }

    pub fn fail_mail_delivery(&mut self, id: i64, message: &str) -> Result<bool> {
        if message.len() > MAX_MAIL_ERROR_BYTES {
            bail!("mail delivery error is too long");
        }
        let attempts = self
            .connection
            .query_row(
                "SELECT attempts FROM mail_deliveries WHERE id = ?1 AND state = 'leased'",
                [id],
                |row| row.get::<_, u32>(0),
            )
            .optional()?;
        let Some(attempts) = attempts else {
            return Ok(false);
        };
        if attempts >= MAIL_MAX_ATTEMPTS {
            return Ok(self.connection.execute(
                "UPDATE mail_deliveries
                 SET state = 'failed', lease_until = NULL, last_error = ?1
                 WHERE id = ?2 AND state = 'leased'",
                params![message, id],
            )? != 0);
        }
        let delay_seconds = 30_i64 * (1_i64 << attempts);
        let available_at = (Utc::now() + Duration::seconds(delay_seconds)).to_rfc3339();
        Ok(self.connection.execute(
            "UPDATE mail_deliveries
             SET state = 'retry', available_at = ?1, lease_until = NULL, last_error = ?2
             WHERE id = ?3 AND state = 'leased'",
            params![available_at, message, id],
        )? != 0)
    }

    pub fn unsubscribe_mail_token(&mut self, token: &str) -> Result<Option<String>> {
        validate_token(token)?;
        let preference = self
            .connection
            .query_row(
                "SELECT mp.board_id, mp.user_uid, b.slug
                 FROM mail_preferences mp
                 JOIN boards b ON b.id = mp.board_id
                 WHERE mp.unsubscribe_token = ?1",
                [token],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((board_id, uid, board_slug)) = preference else {
            return Ok(None);
        };
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE mail_preferences
             SET subscribed = 0, updated_at = ?1
             WHERE board_id = ?2 AND user_uid = ?3",
            params![Utc::now().to_rfc3339(), board_id, uid],
        )?;
        cancel_mail_for_subscription(&transaction, board_id, uid)?;
        transaction.commit()?;
        Ok(Some(board_slug))
    }

    pub fn import_mail_reply(
        &mut self,
        uid: u32,
        author: &str,
        post_id: i64,
        message_id: &str,
        body: &str,
        recipients: &[MailRecipient],
    ) -> Result<Option<ImportedMailReply>> {
        validate_message_id(message_id)?;
        if let Some(existing_post_id) = self
            .connection
            .query_row(
                "SELECT post_id FROM mail_inbound WHERE message_id = ?1
             UNION ALL
             SELECT post_id FROM mail_post_inbound WHERE message_id = ?1
             LIMIT 1",
                [message_id],
                |row| row.get(0),
            )
            .optional()?
        {
            return Ok(Some(ImportedMailReply {
                post_id: existing_post_id,
                duplicate: true,
            }));
        }
        validate_body(body)?;
        let timestamp = Utc::now().to_rfc3339();
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT INTO replies
                 (post_id, author_uid, author, body, created_at, updated_at)
             SELECT id, ?1, ?2, ?3, ?4, ?4
             FROM posts WHERE id = ?5 AND locked = 0",
            params![uid, author, body, timestamp, post_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let reply_id = transaction.last_insert_rowid();
        let (board, title) = mail_post_context(&transaction, post_id)?
            .context("post disappeared while importing mail reply")?;
        transaction.execute(
            "INSERT INTO mail_inbound (message_id, post_id, reply_id, received_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![message_id, post_id, reply_id, timestamp],
        )?;
        transaction.execute(
            "UPDATE posts SET updated_at = ?1 WHERE id = ?2",
            params![timestamp, post_id],
        )?;
        enqueue_mail_event(
            &transaction,
            &board,
            post_id,
            Some(reply_id),
            author,
            &format!("Re: {title}"),
            body,
            &format!("<bbs-reply-{reply_id}@salyut.one>"),
            Some(&format!("<bbs-post-{post_id}@salyut.one>")),
            &timestamp,
            recipients,
        )?;
        transaction.commit()?;
        Ok(Some(ImportedMailReply {
            post_id,
            duplicate: false,
        }))
    }

    fn reply_post_id(&self, id: i64) -> Result<Option<i64>> {
        self.connection
            .query_row("SELECT post_id FROM replies WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .context("read reply post")
    }
}

#[allow(clippy::too_many_arguments)]
fn enqueue_mail_event(
    transaction: &rusqlite::Transaction<'_>,
    board: &Board,
    post_id: i64,
    reply_id: Option<i64>,
    author: &str,
    subject: &str,
    body: &str,
    message_id: &str,
    in_reply_to: Option<&str>,
    timestamp: &str,
    recipients: &[MailRecipient],
) -> Result<()> {
    transaction.execute(
        "INSERT INTO mail_events
             (board_id, post_id, reply_id, author, subject, body,
              message_id, in_reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            board.id,
            post_id,
            reply_id,
            author,
            subject,
            body,
            message_id,
            in_reply_to,
            timestamp
        ],
    )?;
    let event_id = transaction.last_insert_rowid();
    for recipient in recipients {
        let unsubscribe_token = random_token()?;
        transaction.execute(
            "INSERT INTO mail_preferences
                 (board_id, user_uid, username, subscribed, unsubscribe_token, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(board_id, user_uid) DO UPDATE SET
                 username = excluded.username,
                 updated_at = excluded.updated_at",
            params![
                board.id,
                recipient.uid,
                recipient.username,
                unsubscribe_token,
                timestamp
            ],
        )?;
        let subscribed: bool = transaction.query_row(
            "SELECT subscribed FROM mail_preferences
             WHERE board_id = ?1 AND user_uid = ?2",
            params![board.id, recipient.uid],
            |row| row.get(0),
        )?;
        if !subscribed {
            continue;
        }
        let reply_token = random_token()?;
        transaction.execute(
            "INSERT INTO mail_thread_tokens (post_id, user_uid, token, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(post_id, user_uid) DO NOTHING",
            params![post_id, recipient.uid, reply_token, timestamp],
        )?;
        transaction.execute(
            "INSERT INTO mail_deliveries
                 (event_id, user_uid, recipient, state, available_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            params![event_id, recipient.uid, recipient.username, timestamp],
        )?;
    }
    Ok(())
}

fn mail_post_context(
    transaction: &rusqlite::Transaction<'_>,
    post_id: i64,
) -> Result<Option<(Board, String)>> {
    transaction
        .query_row(
            "SELECT b.id, b.slug, b.name, b.description, b.kind, b.write_group, p.title
             FROM posts p
             JOIN boards b ON b.id = p.board_id
             WHERE p.id = ?1",
            [post_id],
            |row| Ok((board_from_row(row)?, row.get(6)?)),
        )
        .optional()
        .context("read post mail context")
}

fn cancel_mail_for_subscription(
    transaction: &rusqlite::Transaction<'_>,
    board_id: i64,
    uid: u32,
) -> Result<()> {
    transaction.execute(
        "UPDATE mail_deliveries
         SET state = 'cancelled', lease_until = NULL
         WHERE user_uid = ?1
           AND state IN ('pending', 'retry')
           AND event_id IN (SELECT id FROM mail_events WHERE board_id = ?2)",
        params![uid, board_id],
    )?;
    transaction.execute(
        "DELETE FROM mail_thread_tokens
         WHERE user_uid = ?1
           AND post_id IN (SELECT id FROM posts WHERE board_id = ?2)",
        params![uid, board_id],
    )?;
    Ok(())
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open operating-system random source")?
        .read_exact(&mut bytes)
        .context("read operating-system random source")?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}")?;
    }
    Ok(token)
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid mail token");
    }
    Ok(())
}

fn validate_message_id(message_id: &str) -> Result<()> {
    if message_id.trim().is_empty()
        || message_id.len() > MAX_MESSAGE_ID_BYTES
        || message_id.contains(['\r', '\n'])
    {
        bail!("invalid Message-ID");
    }
    Ok(())
}

fn migrate_mail_delivery_states(connection: &mut Connection) -> Result<()> {
    let schema = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'mail_deliveries'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(schema) = schema else {
        return Ok(());
    };
    if schema.contains("'failed'") {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .context("start mail delivery state migration")?;
    transaction
        .execute_batch(
            "ALTER TABLE mail_deliveries RENAME TO mail_deliveries_legacy;

             CREATE TABLE mail_deliveries (
                 id               INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id         INTEGER NOT NULL REFERENCES mail_events(id) ON DELETE CASCADE,
                 user_uid         INTEGER NOT NULL,
                 recipient        TEXT NOT NULL,
                 state            TEXT NOT NULL CHECK (
                     state IN ('pending', 'leased', 'retry', 'delivered', 'cancelled',
                               'failed')
                 ),
                 attempts         INTEGER NOT NULL DEFAULT 0,
                 available_at     TEXT NOT NULL,
                 lease_until      TEXT,
                 last_error       TEXT,
                 delivered_at     TEXT,
                 UNIQUE(event_id, user_uid)
             );

             INSERT INTO mail_deliveries SELECT * FROM mail_deliveries_legacy;

             DROP TABLE mail_deliveries_legacy;

             CREATE INDEX IF NOT EXISTS mail_deliveries_ready
                 ON mail_deliveries(state, available_at, lease_until, id);",
        )
        .context("add the failed state to mail deliveries")?;
    transaction
        .commit()
        .context("commit mail delivery state migration")?;
    Ok(())
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn validate_post(title: &str, body: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("title cannot be empty");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        bail!("title cannot exceed {MAX_TITLE_CHARS} characters");
    }
    validate_body(body)
}

fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        bail!("body cannot be empty");
    }
    if body.len() > MAX_BODY_BYTES {
        bail!("body cannot exceed {MAX_BODY_BYTES} bytes");
    }
    Ok(())
}

fn validate_transition_note(name: &str, note: &str) -> Result<()> {
    if note.trim().is_empty() {
        bail!("{name} cannot be empty");
    }
    if note.len() > MAX_TRANSITION_NOTE_BYTES {
        bail!("{name} cannot exceed {MAX_TRANSITION_NOTE_BYTES} bytes");
    }
    Ok(())
}

fn parse_timestamp(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                value.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn parse_board_kind(value: String) -> rusqlite::Result<BoardKind> {
    match value.as_str() {
        "discussion" => Ok(BoardKind::Discussion),
        "polls" => Ok(BoardKind::Polls),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Text,
            format!("unknown board kind {value}").into(),
        )),
    }
}

fn parse_proposal_state(value: String) -> rusqlite::Result<ProposalState> {
    let state = match value.as_str() {
        "voting" => ProposalState::Voting,
        "accepted" => ProposalState::Accepted,
        "rejected" => ProposalState::Rejected,
        "withdrawn" => ProposalState::Withdrawn,
        "vetoed" => ProposalState::Vetoed,
        "implemented" => ProposalState::Implemented,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                value.len(),
                rusqlite::types::Type::Text,
                format!("unknown proposal state {value}").into(),
            ));
        }
    };
    Ok(state)
}

fn board_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Board> {
    Ok(Board {
        id: row.get(0)?,
        slug: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        kind: parse_board_kind(row.get(4)?)?,
        write_group: row.get(5)?,
    })
}

fn post_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Post> {
    Ok(Post {
        id: row.get(0)?,
        board: Board {
            id: row.get(1)?,
            slug: row.get(2)?,
            name: row.get(3)?,
            description: row.get(4)?,
            kind: parse_board_kind(row.get(5)?)?,
            write_group: row.get(6)?,
        },
        author: row.get(7)?,
        title: row.get(8)?,
        body: row.get(9)?,
        locked: row.get(10)?,
        replies: Vec::new(),
        poll: None,
        proposal: None,
        created_at: parse_timestamp(row.get::<_, String>(11)?)?,
        updated_at: parse_timestamp(row.get::<_, String>(12)?)?,
    })
}

fn migrate_legacy_proposals(connection: &mut Connection) -> Result<()> {
    let legacy = {
        let mut statement = connection.prepare(
            "SELECT p.id, p.author_uid, p.author, p.created_at, p.updated_at, p.locked
             FROM posts p
             JOIN boards b ON b.id = p.board_id
             WHERE b.kind = 'polls'
               AND EXISTS (SELECT 1 FROM poll_options po WHERE po.post_id = p.id)
               AND NOT EXISTS (SELECT 1 FROM proposals pr WHERE pr.post_id = p.id)
             ORDER BY p.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (post_id, author_uid, author, created_at, updated_at, locked) in legacy {
        let opened_at = DateTime::parse_from_rfc3339(&created_at)
            .with_context(|| format!("parse legacy proposal #{post_id} creation time"))?
            .with_timezone(&Utc);
        let closes_at = opened_at + Duration::days(PROPOSAL_VOTING_DAYS);
        let (state, closed_at) = if locked {
            let (for_votes, against_votes): (u32, u32) = connection.query_row(
                "SELECT
                     COALESCE(SUM(CASE WHEN po.position = 0 THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN po.position = 1 THEN 1 ELSE 0 END), 0)
                 FROM poll_votes pv
                 JOIN poll_options po ON po.id = pv.option_id
                 WHERE pv.post_id = ?1",
                [post_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            (
                if for_votes > against_votes {
                    ProposalState::Accepted
                } else {
                    ProposalState::Rejected
                },
                Some(updated_at.clone()),
            )
        } else {
            (ProposalState::Voting, None)
        };
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO proposals
                 (post_id, state, opens_at, closes_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                post_id,
                state.label(),
                created_at,
                closes_at.to_rfc3339(),
                closed_at
            ],
        )?;
        transaction.execute(
            "INSERT INTO proposal_events
                 (post_id, from_state, to_state, actor_uid, actor, reason, created_at)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)",
            params![
                post_id,
                state.label(),
                author_uid,
                author,
                "Migrated from the legacy poll lifecycle",
                updated_at
            ],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Database, MailPostImport, MailRecipient, PROPOSAL_VOTING_DAYS};
    use crate::protocol::ProposalState;
    use chrono::{Duration, Utc};
    use rusqlite::Connection;

    #[test]
    fn seeds_expected_boards() {
        let database = Database::open(Path::new(":memory:")).unwrap();
        let boards = database.boards().unwrap();
        assert_eq!(
            boards
                .iter()
                .map(|board| board.slug.as_str())
                .collect::<Vec<_>>(),
            ["general", "updates", "proposals"]
        );
        assert_eq!(boards[1].write_group.as_deref(), Some("wheel"));
        assert!(boards[0].write_group.is_none());
        assert!(boards[2].write_group.is_none());
    }

    fn mail_recipients() -> Vec<MailRecipient> {
        vec![
            MailRecipient {
                uid: 1001,
                username: "alice".to_owned(),
            },
            MailRecipient {
                uid: 1002,
                username: "bob".to_owned(),
            },
        ]
    }

    #[test]
    fn mail_delivery_is_opt_out_and_unsubscribe_cancels_pending_mail() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        assert_eq!(
            database.mail_subscription(1001, "general", true).unwrap(),
            Some(true)
        );
        assert_eq!(
            database.mail_subscription(999, "general", false).unwrap(),
            Some(false)
        );
        database
            .set_mail_subscription(1002, "bob", "general", false)
            .unwrap();
        let board = database.board("general").unwrap().unwrap();
        database
            .create_with_mail(
                &board,
                1001,
                "alice",
                "Mail thread",
                "Opening body.",
                &mail_recipients(),
            )
            .unwrap();

        let delivery = database.claim_mail_delivery(Utc::now()).unwrap().unwrap();
        assert_eq!(delivery.recipient, "alice");
        assert!(database.complete_mail_delivery(delivery.id).unwrap());
        assert!(database.claim_mail_delivery(Utc::now()).unwrap().is_none());

        let board = database
            .unsubscribe_mail_token(&delivery.unsubscribe_token)
            .unwrap()
            .unwrap();
        assert_eq!(board, "general");
        assert_eq!(
            database.mail_subscription(1001, "general", true).unwrap(),
            Some(false)
        );
        assert!(
            database
                .mail_reply_target(&delivery.reply_token)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mail_delivery_fails_permanently_after_three_attempts() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        database
            .set_mail_subscription(1002, "bob", "general", false)
            .unwrap();
        let board = database.board("general").unwrap().unwrap();
        database
            .create_with_mail(
                &board,
                1001,
                "alice",
                "Mail thread",
                "Opening body.",
                &mail_recipients(),
            )
            .unwrap();

        let delivery = database.claim_mail_delivery(Utc::now()).unwrap().unwrap();
        assert!(
            database
                .fail_mail_delivery(delivery.id, "smtp down")
                .unwrap()
        );
        let retry = Utc::now() + Duration::seconds(30);
        assert!(database.claim_mail_delivery(retry).unwrap().is_none());

        let mut later = Utc::now() + Duration::hours(1);
        for _ in 0..2 {
            let delivery = database.claim_mail_delivery(later).unwrap().unwrap();
            assert!(
                database
                    .fail_mail_delivery(delivery.id, "smtp down")
                    .unwrap()
            );
            later += Duration::hours(1);
        }
        assert!(database.claim_mail_delivery(later).unwrap().is_none());
        let state: String = database
            .connection
            .query_row("SELECT state FROM mail_deliveries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[test]
    fn mail_post_is_transactional_and_deduplicated() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("general").unwrap().unwrap();
        let imported = database
            .import_mail_post(MailPostImport {
                board: &board,
                uid: 1001,
                author: "alice",
                message_id: "<new-thread@salyut.one>",
                title: "Posted by mail",
                body: "Opening body.",
                recipients: &mail_recipients(),
            })
            .unwrap();
        assert!(!imported.duplicate);
        let post = database.get(imported.post_id, 1001).unwrap().unwrap();
        assert_eq!(post.author, "alice");
        assert_eq!(post.title, "Posted by mail");
        assert_eq!(post.body, "Opening body.");

        let duplicate = database
            .import_mail_post(MailPostImport {
                board: &board,
                uid: 1001,
                author: "alice",
                message_id: "<new-thread@salyut.one>",
                title: "Changed subject",
                body: "This must not be imported twice.",
                recipients: &mail_recipients(),
            })
            .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.post_id, imported.post_id);
        assert_eq!(database.list("general", 20, 0).unwrap().unwrap().len(), 1);
    }

    #[test]
    fn mail_reply_is_deduplicated_and_uses_the_normal_reply_model() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("general").unwrap().unwrap();
        let post = database
            .create_with_mail(
                &board,
                1001,
                "alice",
                "Mail thread",
                "Opening body.",
                &mail_recipients(),
            )
            .unwrap();
        let delivery = database.claim_mail_delivery(Utc::now()).unwrap().unwrap();
        let (token_post_id, uid) = database
            .mail_reply_target(&delivery.reply_token)
            .unwrap()
            .unwrap();
        assert_eq!((token_post_id, uid), (post.id, 1001));

        let imported = database
            .import_mail_reply(
                uid,
                "alice",
                post.id,
                "<incoming-1@salyut.one>",
                "Reply from mail.",
                &mail_recipients(),
            )
            .unwrap()
            .unwrap();
        assert!(!imported.duplicate);
        assert_eq!(
            database.get(post.id, uid).unwrap().unwrap().replies[0].body,
            "Reply from mail."
        );
        let duplicate = database
            .import_mail_reply(
                uid,
                "alice",
                post.id,
                "<incoming-1@salyut.one>",
                "This must not be imported twice.",
                &mail_recipients(),
            )
            .unwrap()
            .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(
            database.get(post.id, uid).unwrap().unwrap().replies.len(),
            1
        );

        database.set_locked(post.id, true, uid).unwrap();
        assert!(
            database
                .import_mail_reply(
                    uid,
                    "alice",
                    post.id,
                    "<incoming-2@salyut.one>",
                    "Too late.",
                    &mail_recipients(),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn post_lifecycle_is_owned_by_uid() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("general").unwrap().unwrap();
        let post = database
            .create(&board, 1001, "alice", "First post", "Original body.")
            .unwrap();

        assert_eq!(post.author, "alice");
        assert!(
            database
                .update(1002, post.id, "Hijacked", "No")
                .unwrap()
                .is_none()
        );
        assert!(!database.delete(1002, post.id).unwrap());

        let updated = database
            .update(1001, post.id, "Updated", "Edited body.")
            .unwrap()
            .unwrap();
        assert_eq!(updated.title, "Updated");
        assert!(database.delete(1001, post.id).unwrap());
        assert!(database.get(post.id, 1001).unwrap().is_none());
    }

    #[test]
    fn proposal_accepts_one_changeable_vote_per_uid() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        let poll = database
            .create_proposal(&board, 1001, "alice", "Tea?", "Serve tea?")
            .unwrap();
        let options = &poll.poll.as_ref().unwrap().options;
        assert_eq!(
            options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["For", "Against", "Abstain"]
        );
        let first = options[0].id;
        let second = options[1].id;

        let voted = database.vote(1002, poll.id, first).unwrap().unwrap();
        assert_eq!(voted.poll.as_ref().unwrap().total_votes, 1);
        let changed = database.vote(1002, poll.id, second).unwrap().unwrap();
        let poll = changed.poll.unwrap();
        assert_eq!(poll.total_votes, 1);
        assert_eq!(poll.my_vote, Some(second));
        assert_eq!(poll.options[0].votes, 0);
        assert_eq!(poll.options[1].votes, 1);
    }

    #[test]
    fn reply_lifecycle_is_owned_by_uid() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("general").unwrap().unwrap();
        let post = database
            .create(&board, 1001, "alice", "Thread", "Opening post.")
            .unwrap();

        let post = database
            .create_reply(1002, "bob", post.id, "First reply.")
            .unwrap()
            .unwrap();
        let reply = &post.replies[0];
        assert_eq!(reply.author, "bob");
        assert_eq!(database.reply_owner_uid(reply.id).unwrap(), Some(1002));
        assert!(
            database
                .update_reply(1003, reply.id, "Hijacked")
                .unwrap()
                .is_none()
        );
        assert!(database.delete_reply(1003, reply.id).unwrap().is_none());

        let reply_id = reply.id;
        let post_id = database
            .update_reply(1002, reply_id, "Edited reply.")
            .unwrap()
            .unwrap();
        assert_eq!(
            database.get(post_id, 1002).unwrap().unwrap().replies[0].body,
            "Edited reply."
        );
        assert_eq!(
            database.delete_reply(1002, reply_id).unwrap(),
            Some(post.id)
        );
        assert!(
            database
                .get(post.id, 1002)
                .unwrap()
                .unwrap()
                .replies
                .is_empty()
        );
    }

    #[test]
    fn locking_closes_replies_but_does_not_close_proposal_voting() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let general = database.board("general").unwrap().unwrap();
        let post = database
            .create(&general, 1001, "alice", "Thread", "Opening post.")
            .unwrap();
        let locked = database.set_locked(post.id, true, 1001).unwrap().unwrap();
        assert!(locked.locked);
        assert!(
            database
                .create_reply(1002, "bob", post.id, "Too late.")
                .unwrap()
                .is_none()
        );

        let proposals = database.board("proposals").unwrap().unwrap();
        let proposal = database
            .create_proposal(&proposals, 1001, "alice", "Tea?", "Serve tea?")
            .unwrap();
        let option_id = proposal.poll.as_ref().unwrap().options[0].id;
        database
            .set_locked(proposal.id, true, 1001)
            .unwrap()
            .unwrap();
        assert!(
            database
                .vote(1002, proposal.id, option_id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn proposals_close_after_seven_days_and_abstentions_do_not_affect_outcome() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        let proposal = database
            .create_proposal(&board, 1001, "alice", "Tea?", "Serve tea?")
            .unwrap();
        let lifecycle = proposal.proposal.as_ref().unwrap();
        assert_eq!(
            lifecycle.closes_at - lifecycle.opens_at,
            Duration::days(PROPOSAL_VOTING_DAYS)
        );
        let options = &proposal.poll.as_ref().unwrap().options;
        database.vote(1001, proposal.id, options[0].id).unwrap();
        database.vote(1002, proposal.id, options[0].id).unwrap();
        database.vote(1003, proposal.id, options[1].id).unwrap();
        database.vote(1004, proposal.id, options[2].id).unwrap();
        database.vote(1005, proposal.id, options[2].id).unwrap();

        assert_eq!(
            database
                .finalize_due_proposals(lifecycle.closes_at)
                .unwrap(),
            1
        );
        let closed = database.get(proposal.id, 1001).unwrap().unwrap();
        assert_eq!(
            closed.proposal.as_ref().unwrap().state,
            ProposalState::Accepted
        );
        assert!(
            database
                .vote(1006, proposal.id, options[0].id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn tied_proposal_is_rejected() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        let proposal = database
            .create_proposal(&board, 1001, "alice", "Tea?", "Serve tea?")
            .unwrap();
        let options = &proposal.poll.as_ref().unwrap().options;
        database.vote(1001, proposal.id, options[0].id).unwrap();
        database.vote(1002, proposal.id, options[1].id).unwrap();
        let closes_at = proposal.proposal.as_ref().unwrap().closes_at;
        database.finalize_due_proposals(closes_at).unwrap();
        assert_eq!(
            database
                .get(proposal.id, 1001)
                .unwrap()
                .unwrap()
                .proposal
                .unwrap()
                .state,
            ProposalState::Rejected
        );
    }

    #[test]
    fn accepted_proposal_can_be_vetoed_or_implemented_with_an_audit_note() {
        for implemented in [false, true] {
            let mut database = Database::open(Path::new(":memory:")).unwrap();
            let board = database.board("proposals").unwrap().unwrap();
            let proposal = database
                .create_proposal(&board, 1001, "alice", "Tea?", "Serve tea?")
                .unwrap();
            let option = proposal.poll.as_ref().unwrap().options[0].id;
            database.vote(1001, proposal.id, option).unwrap();
            let closes_at = proposal.proposal.as_ref().unwrap().closes_at;
            database.finalize_due_proposals(closes_at).unwrap();
            let changed = if implemented {
                database
                    .mark_proposal_implemented(proposal.id, 0, "root", "Installed the kettle.")
                    .unwrap()
            } else {
                database
                    .veto_proposal(proposal.id, 0, "root", "Exceeds the power budget.")
                    .unwrap()
            }
            .unwrap();
            let lifecycle = changed.proposal.unwrap();
            assert_eq!(
                lifecycle.state,
                if implemented {
                    ProposalState::Implemented
                } else {
                    ProposalState::Vetoed
                }
            );
            assert_eq!(
                lifecycle
                    .events
                    .iter()
                    .find(|event| event.to_state == lifecycle.state)
                    .and_then(|event| event.reason.as_deref()),
                Some(if implemented {
                    "Installed the kettle."
                } else {
                    "Exceeds the power budget."
                })
            );
        }
    }

    #[test]
    fn proposal_author_can_withdraw_while_voting_is_open() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        let proposal = database
            .create_proposal(&board, 1001, "alice", "Tea?", "Serve tea?")
            .unwrap();
        let withdrawn = database
            .withdraw_proposal(proposal.id, 1001, "alice")
            .unwrap()
            .unwrap();
        assert_eq!(withdrawn.proposal.unwrap().state, ProposalState::Withdrawn);
    }

    #[test]
    fn migrates_mail_deliveries_to_allow_the_failed_state() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "salyut-bbs-mail-migration-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE mail_events (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         board_id INTEGER NOT NULL,
                         post_id INTEGER NOT NULL,
                         reply_id INTEGER,
                         author TEXT NOT NULL,
                         subject TEXT NOT NULL,
                         body TEXT NOT NULL,
                         message_id TEXT NOT NULL UNIQUE,
                         in_reply_to TEXT,
                         created_at TEXT NOT NULL
                     );
                     CREATE TABLE mail_deliveries (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         event_id INTEGER NOT NULL REFERENCES mail_events(id) ON DELETE CASCADE,
                         user_uid INTEGER NOT NULL,
                         recipient TEXT NOT NULL,
                         state TEXT NOT NULL CHECK (
                             state IN ('pending', 'leased', 'retry', 'delivered', 'cancelled')
                         ),
                         attempts INTEGER NOT NULL DEFAULT 0,
                         available_at TEXT NOT NULL,
                         lease_until TEXT,
                         last_error TEXT,
                         delivered_at TEXT,
                         UNIQUE(event_id, user_uid)
                     );
                     INSERT INTO mail_events
                         (board_id, post_id, author, subject, body, message_id, created_at)
                     VALUES (1, 1, 'alice', 'Legacy', 'Body',
                             '<legacy@salyut.one>', '2026-01-01T00:00:00Z');
                     INSERT INTO mail_deliveries
                         (event_id, user_uid, recipient, state, available_at)
                     VALUES (1, 1002, 'bob', 'pending', '2026-01-01T00:00:00Z');",
                )
                .unwrap();
        }
        {
            let database = Database::open(&path).unwrap();
            database
                .connection
                .execute("UPDATE mail_deliveries SET state = 'failed'", [])
                .unwrap();
            let (state, recipient): (String, String) = database
                .connection
                .query_row("SELECT state, recipient FROM mail_deliveries", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!((state.as_str(), recipient.as_str()), ("failed", "bob"));
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn migrates_legacy_posts_into_general() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "salyut-bbs-migration-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE posts (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         author_uid INTEGER NOT NULL,
                         author TEXT NOT NULL,
                         title TEXT NOT NULL,
                         body TEXT NOT NULL,
                         created_at TEXT NOT NULL,
                         updated_at TEXT NOT NULL
                     );
                     INSERT INTO posts
                         (author_uid, author, title, body, created_at, updated_at)
                     VALUES
                         (1001, 'alice', 'Legacy', 'Before boards',
                          '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
                )
                .unwrap();
        }
        {
            let database = Database::open(&path).unwrap();
            let posts = database.list("general", 20, 0).unwrap().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].title, "Legacy");
            assert_eq!(
                database.get(posts[0].id, 1001).unwrap().unwrap().board.slug,
                "general"
            );
            assert!(!database.get(posts[0].id, 1001).unwrap().unwrap().locked);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn migrates_locked_legacy_proposal_without_losing_votes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "salyut-bbs-proposal-migration-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE boards (
                         id INTEGER PRIMARY KEY,
                         slug TEXT NOT NULL UNIQUE,
                         name TEXT NOT NULL,
                         description TEXT NOT NULL,
                         kind TEXT NOT NULL,
                         write_group TEXT
                     );
                     INSERT INTO boards VALUES
                         (3, 'proposals', 'Proposals', 'Legacy polls', 'polls', NULL);
                     CREATE TABLE posts (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         board_id INTEGER NOT NULL,
                         author_uid INTEGER NOT NULL,
                         author TEXT NOT NULL,
                         title TEXT NOT NULL,
                         body TEXT NOT NULL,
                         locked INTEGER NOT NULL DEFAULT 0,
                         created_at TEXT NOT NULL,
                         updated_at TEXT NOT NULL
                     );
                     INSERT INTO posts VALUES
                         (1, 3, 1001, 'alice', 'Legacy proposal', 'Keep this',
                          1, '2026-07-01T00:00:00Z', '2026-07-08T00:00:00Z');
                     CREATE TABLE poll_options (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         post_id INTEGER NOT NULL,
                         label TEXT NOT NULL,
                         position INTEGER NOT NULL,
                         UNIQUE(post_id, position)
                     );
                     INSERT INTO poll_options VALUES
                         (1, 1, 'yes', 0),
                         (2, 1, 'no', 1);
                     CREATE TABLE poll_votes (
                         post_id INTEGER NOT NULL,
                         option_id INTEGER NOT NULL,
                         voter_uid INTEGER NOT NULL,
                         voted_at TEXT NOT NULL,
                         PRIMARY KEY(post_id, voter_uid)
                     );
                     INSERT INTO poll_votes VALUES
                         (1, 1, 1001, '2026-07-02T00:00:00Z'),
                         (1, 1, 1002, '2026-07-02T00:00:00Z'),
                         (1, 2, 1003, '2026-07-02T00:00:00Z');",
                )
                .unwrap();
        }
        {
            let database = Database::open(&path).unwrap();
            let proposal = database.get(1, 1001).unwrap().unwrap();
            assert_eq!(
                proposal.proposal.as_ref().unwrap().state,
                ProposalState::Accepted
            );
            assert_eq!(proposal.poll.as_ref().unwrap().total_votes, 3);
            assert_eq!(
                proposal
                    .proposal
                    .unwrap()
                    .events
                    .first()
                    .unwrap()
                    .reason
                    .as_deref(),
                Some("Migrated from the legacy poll lifecycle")
            );
        }
        fs::remove_file(path).unwrap();
    }
}
