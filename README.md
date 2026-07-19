# salyut-bbs

The message board for [salyut.one](https://salyut.one), an all-purpose, small,
tilde-adjacent pubnix running Fedora 44.

There are two BBS processes:

- `salyut-bbsd` owns the database and checks Unix credentials.
- `salyut-bbs` is the terminal client, installed with the shorter `bbs` alias.

The read-only web view lives in
[`salyut-site`](https://github.com/salyut-one/site) at `/bbs`. It uses the same
Unix socket as the terminal client. The daemon rejects mutating requests from
the `salyut-web` Unix identity, so the web process remains read-only even
though the socket is shared.

The daemon creates three boards on first start:

- **General** - open to all users.
- **Updates** — only members of `wheel` may start threads; every user may reply.
- **Proposals** — every user may submit a proposal and vote once per Unix UID.
  Voting again changes the existing vote while the seven-day voting window is
  open.

Threads accept replies. Reply authors can edit or delete their own replies, and
members of `wheel` can lock any thread. Locking closes replies but does not
change a proposal or its voting window.

Proposals use the fixed choices **For**, **Against**, and **Abstain**. When the
seven-day window closes, the daemon accepts a proposal only when For has more
votes than Against; abstentions are excluded and a tie is rejected. Authors may
withdraw their own proposal while voting is open. Members of `wheel` may veto
an accepted proposal with a published reason or mark it implemented with a
published note. Every transition is retained in the proposal history.

Old databases are upgraded in place. Existing posts go into General. Legacy
proposal polls keep their votes and option labels; the first option is treated
as For, the second as Against, and any remaining options as abstentions when
calculating the result.

## Build and test

```sh
make check
make build
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

```

Use `[` and `]` to change boards, `j`/`k` to move, Enter to read, and
`n`/`e`/`d` to create, edit, or delete. Drafts open in `$VISUAL`, then
`$EDITOR`, falling back to `vi`; post drafts use the first line as the title
and a blank line before the body. Press `v` while reading a proposal to vote.
While reading, `a` writes a reply, `u` updates the selected reply, `d` deletes
it, and `l` locks or unlocks the thread for members of `wheel`. Proposal
authors press `w` to withdraw while voting is open. Members of `wheel` press
`x` to record a veto or `i` to record implementation after acceptance.

## Fedora 44

Build and install the two binaries, daemon unit, and tmpfiles rule, then create
the service account and socket group as root:

```sh
make check
make build
sudo make install
groupadd --system salyut-bbs
useradd --system --gid salyut-bbs --home-dir /var/lib/salyut-bbs \
  --shell /usr/sbin/nologin salyut-bbsd
usermod --append --groups salyut-bbs alice
```

Create the runtime directories and start the daemon:

```sh
systemd-tmpfiles --create /etc/tmpfiles.d/salyut-bbs.conf
systemctl daemon-reload
systemctl enable --now salyut-bbsd.service
```

Add new shell accounts to `salyut-bbs` as part of account creation.
`salyut-site` runs its `salyut-web` process with this group so it can read from
the shared socket. The daemon stores data in `/var/lib/salyut-bbs` and its
socket at `/run/salyut-bbs/users/salyut.sock`.

## Notes

Clients send one JSON request per Unix-socket connection. The daemon gets the
UID from the socket, resolves current group membership from the system account
database, and never accepts a handle from the client. `salyut-web` is
daemon-enforced read-only; change `--read-only-user` only if the site service
identity changes. Titles are limited to 120 characters and bodies to 64 KiB.
Veto reasons and implementation notes are limited to 4 KiB.

Post ownership follows numeric UIDs. Don't recycle a UID while it still owns
posts.
