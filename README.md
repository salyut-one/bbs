# salyut-bbs

The message board for [salyut.one](https://salyut.one).

There are three processes:

- `salyut-bbsd` owns the database and checks Unix credentials.
- `salyut-bbs` is the terminal client.
- `salyut-bbs-web` serves a read-only HTML view on `127.0.0.1:8080`.

The daemon creates three boards on first start:

- **General** - open to all users.
- **Updates** - only members of `wheel` may post.
- **Proposals** - polls with one vote per Unix UID. Voting again changes the
  existing vote.

Threads accept replies. Reply authors can edit or delete their own replies, and
members of `wheel` can lock any thread. A locked thread rejects new replies,
and a locked proposal rejects votes as well. Old databases are upgraded in
place and existing posts go into General.

## Build

```sh
cargo build --release --locked
cargo test --locked
```

CI builds and tests in a Fedora 44 container. macOS remains covered for local
development. SQLite is built from the bundled source.

## Try it on macOS

The macOS defaults live under `$TMPDIR/salyut-bbs`, so don't use `sudo`.

```sh
# terminal 1
cargo run --bin salyut-bbsd

# terminal 2
cargo run --bin salyut-bbs

# optional, terminal 3
cargo run --bin salyut-bbs-web
```

Use `[` and `]` to change boards, `j`/`k` to move, Enter to read, and
`n`/`e`/`d` to create, edit, or delete. Ctrl-S saves. Press `v` while reading a
proposal to vote. While reading, `a` writes a reply, uppercase `E`/`D` edits or
deletes the selected reply, and `l` locks or unlocks the thread for members of
`wheel`.

## Fedora 44

Install the three release binaries in `/usr/local/bin`, then create the service
accounts and socket groups as root:

```sh
groupadd --system salyut-bbs
groupadd --system salyut-bbs-web
useradd --system --gid salyut-bbs --home-dir /var/lib/salyut-bbs \
  --shell /usr/sbin/nologin salyut-bbsd
useradd --system --gid salyut-bbs-web --home-dir /nonexistent \
  --shell /usr/sbin/nologin salyut-web
usermod --append --groups salyut-bbs alice
```

Install the systemd units and tmpfiles rule, create the runtime directories, and
start both services:

```sh
install -m 0644 etc/systemd/system/*.service /etc/systemd/system/
install -m 0644 etc/tmpfiles.d/salyut-bbs.conf /etc/tmpfiles.d/
systemd-tmpfiles --create /etc/tmpfiles.d/salyut-bbs.conf
systemctl daemon-reload
systemctl enable --now salyut-bbsd.service salyut-bbs-web.service
```

Add new shell accounts to `salyut-bbs` as part of account creation. The web
account belongs only to `salyut-bbs-web`; it cannot reach the write socket.
The daemon stores data in `/var/lib/salyut-bbs` and sockets in
`/run/salyut-bbs`. Put a normal TLS reverse proxy in front of port 8080.

## Notes

Clients send one JSON request per Unix-socket connection. The daemon gets the
UID from the socket, resolves current group membership from the system account
database, and never accepts a handle from the client. Titles are limited to 120
characters, bodies to 64 KiB, and polls to 2-10 options.

Post ownership follows numeric UIDs. Don't recycle a UID while it still owns
posts.
