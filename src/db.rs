use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::protocol::{Board, BoardKind, Poll, PollOption, Post, PostSummary, Reply};

const MAX_TITLE_CHARS: usize = 120;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_POLL_OPTIONS: usize = 10;
const MAX_OPTION_CHARS: usize = 80;

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let connection =
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
                      'Polls about changes to salyut.one.', 'polls', NULL)
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
                     ON replies(post_id, created_at, id);",
            )
            .context("initialize poll and reply schema")?;

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
                    p.locked,
                    (SELECT COUNT(*) FROM replies r WHERE r.post_id = p.id),
                    p.created_at, p.updated_at
             FROM posts p
             JOIN boards b ON b.id = p.board_id
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
                locked: row.get(5)?,
                reply_count: row.get(6)?,
                created_at: parse_timestamp(row.get::<_, String>(7)?)?,
                updated_at: parse_timestamp(row.get::<_, String>(8)?)?,
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
        transaction.commit()?;
        self.get(id, uid)?.context("newly created post disappeared")
    }

    pub fn create_poll(
        &mut self,
        board: &Board,
        uid: u32,
        author: &str,
        title: &str,
        body: &str,
        options: &[String],
    ) -> Result<Post> {
        validate_post(title, body)?;
        validate_poll_options(options)?;
        let timestamp = Utc::now().to_rfc3339();
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
            for (position, option) in options.iter().enumerate() {
                statement.execute(params![id, option.trim(), position as i64])?;
            }
        }
        transaction.commit()?;
        self.get(id, uid)?.context("newly created poll disappeared")
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
        Ok(self.connection.execute(
            "DELETE FROM posts WHERE id = ?1 AND author_uid = ?2",
            params![id, uid],
        )? != 0)
    }

    pub fn vote(&mut self, uid: u32, post_id: i64, option_id: i64) -> Result<Option<Post>> {
        let transaction = self.connection.transaction()?;
        let valid: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM poll_options po
                 JOIN posts p ON p.id = po.post_id
                 JOIN boards b ON b.id = p.board_id
                 WHERE po.id = ?1 AND po.post_id = ?2
                   AND b.kind = 'polls' AND p.locked = 0
             )",
            params![option_id, post_id],
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
            params![post_id, option_id, uid, Utc::now().to_rfc3339()],
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
        transaction.execute(
            "UPDATE posts SET updated_at = ?1 WHERE id = ?2",
            params![timestamp, post_id],
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
        let changed = self.connection.execute(
            "DELETE FROM replies WHERE id = ?1 AND author_uid = ?2",
            params![id, uid],
        )?;
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

    fn reply_post_id(&self, id: i64) -> Result<Option<i64>> {
        self.connection
            .query_row("SELECT post_id FROM replies WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .context("read reply post")
    }
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

fn validate_poll_options(options: &[String]) -> Result<()> {
    if !(2..=MAX_POLL_OPTIONS).contains(&options.len()) {
        bail!("a poll must have between 2 and {MAX_POLL_OPTIONS} options");
    }
    let mut unique = HashSet::new();
    for option in options {
        let option = option.trim();
        if option.is_empty() {
            bail!("poll options cannot be empty");
        }
        if option.chars().count() > MAX_OPTION_CHARS {
            bail!("poll options cannot exceed {MAX_OPTION_CHARS} characters");
        }
        if !unique.insert(option.to_lowercase()) {
            bail!("poll options must be unique");
        }
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
        created_at: parse_timestamp(row.get::<_, String>(11)?)?,
        updated_at: parse_timestamp(row.get::<_, String>(12)?)?,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::Database;
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
    fn poll_accepts_one_changeable_vote_per_uid() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        let poll = database
            .create_poll(
                &board,
                1001,
                "alice",
                "Tea?",
                "Choose a tea.",
                &["Earl Grey".to_owned(), "Assam".to_owned()],
            )
            .unwrap();
        let options = &poll.poll.as_ref().unwrap().options;
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
    fn locked_posts_reject_new_replies_and_votes() {
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
            .create_poll(
                &proposals,
                1001,
                "alice",
                "Tea?",
                "Choose a tea.",
                &["Earl Grey".to_owned(), "Assam".to_owned()],
            )
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
                .is_none()
        );
    }

    #[test]
    fn validation_rejects_bad_polls() {
        let mut database = Database::open(Path::new(":memory:")).unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        assert!(
            database
                .create_poll(
                    &board,
                    1001,
                    "alice",
                    "Title",
                    "Body",
                    &["same".to_owned(), "Same".to_owned()],
                )
                .is_err()
        );
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
}
