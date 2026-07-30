VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")
BIN := target/release/tpx

.PHONY: build run install clean test lint fmt check

build:
	cargo build --release

run: build
	$(BIN)

install: build
	rm -f ~/.local/bin/tpx
	cp $(BIN) ~/.local/bin/tpx

test:
	cargo test

lint:
	cargo clippy --all-targets

fmt:
	cargo fmt

# What CI would run: everything that can fail without a terminal.
check: fmt lint test

clean:
	cargo clean
