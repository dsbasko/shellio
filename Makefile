SHELL := /bin/bash

CARGO_HOME ?= $(HOME)/.cargo
export PATH := $(CARGO_HOME)/bin:$(PATH)

# make 3.81 ищет простую команду по исходному PATH, поэтому инструменты вызываются по полному пути.
RUSTUP := $(CARGO_HOME)/bin/rustup
CARGO := $(CARGO_HOME)/bin/cargo

RUSTUP_INIT_URL := https://sh.rustup.rs

GHOSTTY_TOOLS := scripts/ghostty/tools.sh
GHOSTTY_BUILD := scripts/ghostty/build.sh
GHOSTTY_VENDOR := scripts/ghostty/vendor.sh
GHOSTTY_LOCK := scripts/ghostty/lock.sh

# Цели, читающие vendor/ghostty/, выполняют весь рецепт под одной блокировкой набора: рецепт цели X
# лежит во внутренней цели locked-X и запускается вложенным make. Вторая такая цель сразу получает отказ.
LOCKED_TARGETS := ghostty check

.PHONY: setup test-scripts $(LOCKED_TARGETS) $(addprefix locked-,$(LOCKED_TARGETS))

# Под make -j цели одного вызова иначе столкнулись бы на собственной блокировке.
.NOTPARALLEL:

setup:
	@if [ ! -x "$(RUSTUP)" ]; then \
		echo "rustup не найден, устанавливаю в $(CARGO_HOME)"; \
		curl --proto '=https' --tlsv1.2 -sSf $(RUSTUP_INIT_URL) | \
			sh -s -- -y --no-modify-path --profile minimal \
				--default-toolchain none --default-host aarch64-apple-darwin; \
	fi
	$(RUSTUP) toolchain install
	$(RUSTUP) show active-toolchain
	$(GHOSTTY_TOOLS) zig
	$(GHOSTTY_TOOLS) metal

$(LOCKED_TARGETS):
	@$(GHOSTTY_LOCK) $(MAKE) --no-print-directory locked-$@

locked-ghostty:
	@if $(GHOSTTY_VENDOR) --verify; then \
		echo "сборка ghostty не нужна"; \
	else \
		echo "собираю набор ghostty заново"; \
		build_dir="$$(mktemp -d "$${TMPDIR:-/tmp}/shellio-ghostty-set.XXXXXX")" && \
		trap 'rm -rf "$$build_dir"' EXIT && \
		$(GHOSTTY_BUILD) "$$build_dir/set" && \
		$(GHOSTTY_VENDOR) publish "$$build_dir/set"; \
	fi

locked-check:
	$(GHOSTTY_VENDOR) --verify
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked

test-scripts:
	scripts/ghostty/test-publish.sh
