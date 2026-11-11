SHELL := /bin/bash

CARGO_HOME ?= $(HOME)/.cargo
export PATH := $(CARGO_HOME)/bin:$(PATH)

# make 3.81 ищет простую команду по исходному PATH, поэтому инструменты вызываются по полному пути.
RUSTUP := $(CARGO_HOME)/bin/rustup
CARGO := $(CARGO_HOME)/bin/cargo

RUSTUP_INIT_URL := https://sh.rustup.rs

.PHONY: setup check

setup:
	@if [ ! -x "$(RUSTUP)" ]; then \
		echo "rustup не найден, устанавливаю в $(CARGO_HOME)"; \
		curl --proto '=https' --tlsv1.2 -sSf $(RUSTUP_INIT_URL) | \
			sh -s -- -y --no-modify-path --profile minimal \
				--default-toolchain none --default-host aarch64-apple-darwin; \
	fi
	$(RUSTUP) toolchain install
	$(RUSTUP) show active-toolchain

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked
