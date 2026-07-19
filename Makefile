CARGO ?= cargo
INSTALL ?= install
LN ?= ln
PREFIX ?= /usr/local
SYSCONFDIR ?= /etc
SYSTEMD_UNIT_DIR ?= $(SYSCONFDIR)/systemd/system
TMPFILESDIR ?= $(SYSCONFDIR)/tmpfiles.d

.PHONY: all build test check install

all: build

build:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked

install:
	$(INSTALL) -d "$(DESTDIR)$(PREFIX)/bin"
	$(INSTALL) -m 0755 target/release/salyut-bbs \
		"$(DESTDIR)$(PREFIX)/bin/salyut-bbs"
	$(LN) -sfn salyut-bbs "$(DESTDIR)$(PREFIX)/bin/bbs"
	$(INSTALL) -m 0755 target/release/salyut-bbsd \
		"$(DESTDIR)$(PREFIX)/bin/salyut-bbsd"
	$(INSTALL) -d "$(DESTDIR)$(SYSTEMD_UNIT_DIR)"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbsd.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbsd.service"
	$(INSTALL) -d "$(DESTDIR)$(TMPFILESDIR)"
	$(INSTALL) -m 0644 etc/tmpfiles.d/salyut-bbs.conf \
		"$(DESTDIR)$(TMPFILESDIR)/salyut-bbs.conf"
