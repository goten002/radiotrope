# Build and install Radiotrope on Linux.
#
#   make                          build the two programs, man pages and completions
#   sudo make install             install into /usr/local
#   make install PREFIX=~/.local  install for one user, no root needed
#   make install DESTDIR=pkg PREFIX=/usr   stage for a package (AUR, AppImage, ...)
#   make uninstall                remove what `install` put in place (same PREFIX)
#
# The station checker, radiotrope-probe, is not part of any package: it is a
# separate download from the release page. To build and install it from here:
#
#   make probe generated && make install-probe PREFIX=~/.local
#   make uninstall-probe PREFIX=~/.local
#
# `install` doesn't build: run `make` first, as yourself, so cargo never runs
# as root.

APP_ID   := app.radiotrope.Radiotrope
PROGRAMS := radiotrope radiotrope-cli
PROBE    := radiotrope-probe
ICON_SIZES := 16 32 48 64 128 256 512

PREFIX  ?= /usr/local
DESTDIR ?=
BINDIR  ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
MANDIR  ?= $(DATADIR)/man
BASH_COMPLETIONS ?= $(DATADIR)/bash-completion/completions
ZSH_COMPLETIONS  ?= $(DATADIR)/zsh/site-functions
FISH_COMPLETIONS ?= $(DATADIR)/fish/vendor_completions.d

CARGO       ?= cargo
CARGO_FLAGS ?= --locked
# A cargo profile; CI uses PROFILE=dev for a quicker check of the install
PROFILE     ?= release
TARGET_DIR  ?= $(or $(CARGO_TARGET_DIR),target)
BUILD_DIR   := $(TARGET_DIR)/$(if $(filter dev,$(PROFILE)),debug,$(PROFILE))
GENERATED   := $(TARGET_DIR)/generated

INSTALL      ?= install
INSTALL_PROG := $(INSTALL) -Dm755
INSTALL_DATA := $(INSTALL) -Dm644

# A direct install (no DESTDIR) refreshes the desktop's icon and launcher
# caches; packages leave that to their own hooks
ifeq ($(DESTDIR),)
define refresh_caches
-gtk-update-icon-cache -qtf "$(DATADIR)/icons/hicolor" 2>/dev/null
-update-desktop-database -q "$(DATADIR)/applications" 2>/dev/null
endef
endif

# check_built PROGRAMS HOW: each program and its man page are built
define check_built
for p in $(1); do \
	for f in $(BUILD_DIR)/$$p $(GENERATED)/man/$$p.1; do \
		test -f "$$f" || { echo "$$f is missing: run '$(2)' first" >&2; exit 1; }; \
	done; \
done
endef

# install_programs PROGRAMS: each with its man page and completions
define install_programs
for p in $(1); do \
	$(INSTALL_PROG) $(BUILD_DIR)/$$p "$(DESTDIR)$(BINDIR)/$$p"; \
	$(INSTALL_DATA) $(GENERATED)/man/$$p.1 "$(DESTDIR)$(MANDIR)/man1/$$p.1"; \
	$(INSTALL_DATA) $(GENERATED)/completions/bash/$$p "$(DESTDIR)$(BASH_COMPLETIONS)/$$p"; \
	$(INSTALL_DATA) $(GENERATED)/completions/zsh/_$$p "$(DESTDIR)$(ZSH_COMPLETIONS)/_$$p"; \
	$(INSTALL_DATA) $(GENERATED)/completions/fish/$$p.fish "$(DESTDIR)$(FISH_COMPLETIONS)/$$p.fish"; \
done
endef

define uninstall_programs
for p in $(1); do \
	rm -f "$(DESTDIR)$(BINDIR)/$$p" \
		"$(DESTDIR)$(MANDIR)/man1/$$p.1" \
		"$(DESTDIR)$(BASH_COMPLETIONS)/$$p" \
		"$(DESTDIR)$(ZSH_COMPLETIONS)/_$$p" \
		"$(DESTDIR)$(FISH_COMPLETIONS)/$$p.fish"; \
done
endef

.PHONY: all build probe generated install uninstall install-probe uninstall-probe clean

all: build generated

build:
	$(CARGO) build $(CARGO_FLAGS) --profile $(PROFILE) -p radiotrope-app -p radiotrope-cli

# The station checker alone (not part of `all`)
probe:
	$(CARGO) build $(CARGO_FLAGS) --profile $(PROFILE) -p radiotrope-probe

# Man pages and bash, zsh and fish completions of all three programs, the
# probe's too (see crates/xtask)
generated:
	$(CARGO) run $(CARGO_FLAGS) --quiet -p xtask -- man-and-completions $(GENERATED)

install:
	@$(call check_built,$(PROGRAMS),make)
	$(call install_programs,$(PROGRAMS))
	$(INSTALL_DATA) packaging/linux/$(APP_ID).desktop "$(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop"
	$(INSTALL_DATA) packaging/linux/$(APP_ID).metainfo.xml "$(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml"
	for s in $(ICON_SIZES); do \
		$(INSTALL_DATA) assets/icons/icon-$$s.png "$(DESTDIR)$(DATADIR)/icons/hicolor/$${s}x$${s}/apps/$(APP_ID).png"; \
	done
	$(refresh_caches)

uninstall:
	$(call uninstall_programs,$(PROGRAMS))
	rm -f "$(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop" \
		"$(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml"
	for s in $(ICON_SIZES); do \
		rm -f "$(DESTDIR)$(DATADIR)/icons/hicolor/$${s}x$${s}/apps/$(APP_ID).png"; \
	done
	$(refresh_caches)

# The station checker on its own, never by `install` (packages leave it out).
# Also lays out its standalone download (packaging/linux/dist.sh).
install-probe:
	@$(call check_built,$(PROBE),make probe generated)
	$(call install_programs,$(PROBE))

uninstall-probe:
	$(call uninstall_programs,$(PROBE))

clean:
	rm -rf $(GENERATED)
