PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BIN := mkdownrs

.PHONY: build install uninstall test clean

build:
	cargo build --release

install: build
	mkdir -p $(BINDIR)
	install -m 755 target/release/$(BIN) $(BINDIR)/$(BIN)

uninstall:
	rm -f $(BINDIR)/$(BIN)

test:
	cargo test

clean:
	cargo clean
