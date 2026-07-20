CARGO ?= cargo
INSTALL ?= install
LN ?= ln
POSTCONF ?= postconf
POSTFIX ?= postfix
POSTMAP ?= postmap
PREFIX ?= /usr/local
SYSCONFDIR ?= /etc
SYSTEMCTL ?= systemctl
SYSTEMD_SYSUSERS ?= systemd-sysusers
SYSTEMD_UNIT_DIR ?= $(SYSCONFDIR)/systemd/system
TMPFILESDIR ?= $(SYSCONFDIR)/tmpfiles.d
SYSUSERS_DIR ?= $(SYSCONFDIR)/sysusers.d
POSTFIX_DIR ?= $(SYSCONFDIR)/postfix
POSTFIX_TRANSPORT_MAP = $(POSTFIX_DIR)/salyut-bbs-transport

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
	$(INSTALL) -m 0755 target/release/salyut-bbs-mail \
		"$(DESTDIR)$(PREFIX)/bin/salyut-bbs-mail"
	$(INSTALL) -d "$(DESTDIR)$(SYSTEMD_UNIT_DIR)"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbsd.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbsd.service"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbs-mail.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbs-mail.service"
	$(INSTALL) -d "$(DESTDIR)$(TMPFILESDIR)"
	$(INSTALL) -m 0644 etc/tmpfiles.d/salyut-bbs.conf \
		"$(DESTDIR)$(TMPFILESDIR)/salyut-bbs.conf"
	$(INSTALL) -d "$(DESTDIR)$(SYSUSERS_DIR)"
	$(INSTALL) -m 0644 etc/sysusers.d/salyut-bbs-mail.conf \
		"$(DESTDIR)$(SYSUSERS_DIR)/salyut-bbs-mail.conf"
	$(INSTALL) -d "$(DESTDIR)$(POSTFIX_DIR)"
	$(INSTALL) -m 0644 etc/postfix/salyut-bbs-transport \
		"$(DESTDIR)$(POSTFIX_TRANSPORT_MAP)"
	@if [ -z "$(DESTDIR)" ]; then \
		$(SYSTEMD_SYSUSERS) "$(SYSUSERS_DIR)/salyut-bbs-mail.conf"; \
		$(POSTMAP) "$(POSTFIX_TRANSPORT_MAP)"; \
		current="$$($(POSTCONF) -h transport_maps)"; \
		case "$$current" in \
			*"hash:$(POSTFIX_TRANSPORT_MAP)"*) ;; \
			"") $(POSTCONF) -e \
				'transport_maps = hash:$(POSTFIX_TRANSPORT_MAP)' ;; \
			*) $(POSTCONF) -e \
				"transport_maps = $$current, hash:$(POSTFIX_TRANSPORT_MAP)" ;; \
		esac; \
		$(POSTCONF) -M \
			'bbs/unix=bbs unix - n n - - pipe flags=Rqu user=salyut-bbs-mail argv=$(PREFIX)/bin/salyut-bbs-mail receive --recipient=$${recipient} --sasl-username=$${sasl_username}'; \
		$(POSTCONF) -e 'bbs_destination_recipient_limit = 1'; \
		$(POSTFIX) check; \
		$(SYSTEMCTL) enable salyut-bbs-mail.service; \
	fi
