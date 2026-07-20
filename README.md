# salyut-bbs

Message board for https://salyut.one. It provides the `salyut-bbsd` daemon and
the `salyut-bbs` terminal client, installed with the shorter `bbs` alias. The
daemon owns the SQLite database and authorises clients from their Unix socket
credentials; `salyut-site` uses the same socket through a daemon-enforced
read-only account.

## Build and test

```sh
make check
make build
```

For local development, run the daemon and client in separate terminals:

```sh
cargo run --bin salyut-bbsd
cargo run --bin salyut-bbs
```

The socket, database, socket mode, read-only user, and mail bridge user are
configurable:

```text
salyut-bbsd --socket /run/salyut-bbs/users/salyut.sock \
  --database /var/lib/salyut-bbs/posts.sqlite3 --socket-mode 0660 \
  --read-only-user salyut-web --mail-user salyut-bbs-mail
salyut-bbs --socket /run/salyut-bbs/users/salyut.sock
```

## Local mailing list

Every local account with a UID from 1000 through 59999 is subscribed to every
board by default. Press `m` on a board in the terminal client to opt out or back
in. Each notification also has a `List-Unsubscribe` header and an unsubscribe
address in its footer.

`salyut-bbs-mail` is a dedicated Postfix bridge. The delivery worker claims the
transactional outbox through `salyut-bbsd`, submits mail to the local username,
and acknowledges successful deliveries. Postfix pipes replies and unsubscribe
requests back to the same binary. Recipient-specific capability addresses map a
reply to its Unix UID and BBS thread, so mail headers cannot select an author.

To create a discussion post, send mail to `<board>@bbs.salyut.one`, for example
`updates@bbs.salyut.one`. The message subject becomes the post title and its
`text/plain` part becomes the body. Postfix's authenticated SASL username
selects the Unix author. Mail sent through another provider is accepted for
`general@bbs.salyut.one` and `updates@bbs.salyut.one` only when its single
`From:` address appears as a plain mail address in one eligible Unix user's
`~/.forward` and OpenDKIM reports a passing signature aligned with that
address's domain. Commands, file destinations, include directives, symlinks,
unsafe files, and ambiguous address mappings never authorize a BBS post.

The daemon still applies the board's normal write-group rule, so posting to
`/updates` requires `wheel`. Unauthenticated local `sendmail` submissions,
unaligned or unsigned external mail, and email-created proposals are rejected.

`make install` creates the service account, installs and indexes a dedicated
Postfix transport map, adds it to the existing `transport_maps` value, installs
the `master.cf` pipe, and enables the worker as a dependency of `salyut-bbsd`.
These operations are skipped for staged `DESTDIR` installs.

Do not add `bbs.salyut.one` to `relay_domains`. The installed recipient-access
map narrowly permits unauthenticated SMTP for the two supported board
addresses; reply and unsubscribe capabilities remain local authenticated
routes.

## Deploying

```sh
salyut-admin update salyut-admin salyut-bbs salyut-config
```

The normal update reloads systemd and restarts `salyut-bbsd` and Postfix, which
also starts the enabled mail worker.

## License
[MIT](./LICENSE)
