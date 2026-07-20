CARGO ?= cargo
INSTALL ?= install
LN ?= ln
POSTCONF ?= postconf
POSTFIX ?= postfix
POSTMAP ?= postmap
PREFIX ?= /usr/local
RESTORECON ?= restorecon
SYSCONFDIR ?= /etc
SYSTEMCTL ?= systemctl
SYSTEMD_SYSUSERS ?= systemd-sysusers
SYSTEMD_UNIT_DIR ?= $(SYSCONFDIR)/systemd/system
TMPFILESDIR ?= $(SYSCONFDIR)/tmpfiles.d
SYSUSERS_DIR ?= $(SYSCONFDIR)/sysusers.d
POSTFIX_DIR ?= $(SYSCONFDIR)/postfix
POSTFIX_TRANSPORT_MAP = $(POSTFIX_DIR)/salyut-bbs-transport
POSTFIX_EXTERNAL_RECIPIENT_MAP = $(POSTFIX_DIR)/salyut-bbs-external-recipients

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
	$(INSTALL) -m 0755 target/release/salyut-bbs-forwardd \
		"$(DESTDIR)$(PREFIX)/bin/salyut-bbs-forwardd"
	$(INSTALL) -d "$(DESTDIR)$(SYSTEMD_UNIT_DIR)"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbsd.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbsd.service"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbs-mail.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbs-mail.service"
	$(INSTALL) -m 0644 etc/systemd/system/salyut-bbs-forward-map.service \
		"$(DESTDIR)$(SYSTEMD_UNIT_DIR)/salyut-bbs-forward-map.service"
	$(INSTALL) -d "$(DESTDIR)$(TMPFILESDIR)"
	$(INSTALL) -m 0644 etc/tmpfiles.d/salyut-bbs.conf \
		"$(DESTDIR)$(TMPFILESDIR)/salyut-bbs.conf"
	$(INSTALL) -d "$(DESTDIR)$(SYSUSERS_DIR)"
	$(INSTALL) -m 0644 etc/sysusers.d/salyut-bbs-mail.conf \
		"$(DESTDIR)$(SYSUSERS_DIR)/salyut-bbs-mail.conf"
	$(INSTALL) -m 0644 etc/sysusers.d/salyut-bbs-forward-map.conf \
		"$(DESTDIR)$(SYSUSERS_DIR)/salyut-bbs-forward-map.conf"
	$(INSTALL) -d "$(DESTDIR)$(POSTFIX_DIR)"
	$(INSTALL) -m 0644 etc/postfix/salyut-bbs-transport \
		"$(DESTDIR)$(POSTFIX_TRANSPORT_MAP)"
	$(INSTALL) -m 0644 etc/postfix/salyut-bbs-external-recipients \
		"$(DESTDIR)$(POSTFIX_EXTERNAL_RECIPIENT_MAP)"
	@if [ -z "$(DESTDIR)" ]; then \
		$(SYSTEMD_SYSUSERS) "$(SYSUSERS_DIR)/salyut-bbs-mail.conf"; \
		$(SYSTEMD_SYSUSERS) "$(SYSUSERS_DIR)/salyut-bbs-forward-map.conf"; \
		$(RESTORECON) "$(PREFIX)/bin/salyut-bbs-forwardd"; \
		$(POSTMAP) "$(POSTFIX_TRANSPORT_MAP)"; \
		$(POSTMAP) "$(POSTFIX_EXTERNAL_RECIPIENT_MAP)"; \
		current="$$($(POSTCONF) -h transport_maps)"; \
		case "$$current" in \
			*"hash:$(POSTFIX_TRANSPORT_MAP)"*) ;; \
			"") $(POSTCONF) -e \
				'transport_maps = hash:$(POSTFIX_TRANSPORT_MAP)' ;; \
			*) $(POSTCONF) -e \
				"transport_maps = $$current, hash:$(POSTFIX_TRANSPORT_MAP)" ;; \
		esac; \
		$(POSTCONF) -M \
			'bbs/unix=bbs unix - n n - - pipe flags=Rqu user=salyut-bbs-mail:salyut-bbs argv=$(PREFIX)/bin/salyut-bbs-mail receive --recipient=$${recipient} --sasl-username=$${sasl_username}'; \
		$(POSTCONF) -e 'bbs_destination_recipient_limit = 1'; \
		$(POSTCONF) -e \
			'smtpd_milters = unix:/run/opendkim/opendkim.sock'; \
		$(POSTCONF) -e \
			'smtpd_relay_restrictions = permit_mynetworks, permit_sasl_authenticated, check_recipient_access hash:$(POSTFIX_EXTERNAL_RECIPIENT_MAP), reject_unauth_destination'; \
		$(POSTFIX) check; \
		$(SYSTEMCTL) enable salyut-bbs-mail.service; \
		$(SYSTEMCTL) enable salyut-bbs-forward-map.service; \
	fi
