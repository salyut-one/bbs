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

The socket, database, socket mode, and read-only user are configurable:

```text
salyut-bbsd --socket /run/salyut-bbs/users/salyut.sock \
  --database /var/lib/salyut-bbs/posts.sqlite3 --socket-mode 0660 \
  --read-only-user salyut-web
salyut-bbs --socket /run/salyut-bbs/users/salyut.sock
```

## Deploying

```sh
salyut-admin update
```

## License
[MIT](./LICENSE)
