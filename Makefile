# make && make install       installs to ~/.local/bin
# make && sudo make install  installs to /usr/local/bin
# make install PREFIX=/opt/far DESTDIR=/tmp/pkg  overrides both

EUID_NOW ?= $(shell id -u)

ifeq ($(EUID_NOW),0)
PREFIX ?= /usr/local
else
PREFIX ?= $(HOME)/.local
endif

CARGO ?= cargo
BINDIR := $(DESTDIR)$(PREFIX)/bin
BINARY := target/release/far

.PHONY: all build install uninstall test clean

all: build

build:
	$(CARGO) build --release

# Root does not build: sudo often has no cargo, and building would leave
# root-owned files in target/.
install:
ifeq ($(EUID_NOW),0)
	@test -x $(BINARY) || { echo "$(BINARY) not found: run 'make' as a normal user first" >&2; exit 1; }
else
	$(CARGO) build --release
endif
	install -Dm755 $(BINARY) $(BINDIR)/far
	@echo "Installed $(BINDIR)/far"
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; *) \
		echo "Note: $(PREFIX)/bin is not in PATH. Add it to your shell profile:"; \
		echo "  export PATH=\"$(PREFIX)/bin:\$$PATH\"";; esac

uninstall:
	rm -f $(BINDIR)/far

test:
	$(CARGO) test

clean:
	$(CARGO) clean
