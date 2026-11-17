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
GHOSTTY_BINDINGS := scripts/ghostty/bindings.sh
GHOSTTY_LINK_AUDIT := scripts/ghostty/link-audit.sh
BUNDLE := scripts/bundle.sh
CHECK_BUNDLE := scripts/check-bundle.sh
TEST_BUNDLE := scripts/test-bundle.sh
PROBE_APPKIT := spikes/probe-appkit-shell/run.sh
PROBE_GHOSTTY := spikes/probe-ghostty-surface/run.sh
PROBE_SESSION := spikes/probe-session-survival/run.sh

# Цели, читающие vendor/ghostty/, выполняют весь рецепт под одной блокировкой набора: рецепт цели X
# лежит во внутренней цели locked-X и запускается вложенным make. Вторая такая цель сразу получает отказ.
LOCKED_TARGETS := ghostty check bindings bindings-check link-audit bundle check-bundle run test-bundle \
	probe-ghostty probe-ghostty-demo probe-session-surface

.PHONY: setup test-scripts probe-appkit probe-appkit-demo probe-session $(LOCKED_TARGETS) $(addprefix locked-,$(LOCKED_TARGETS))

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
	$(GHOSTTY_TOOLS) bindgen

$(LOCKED_TARGETS):
	@$(GHOSTTY_LOCK) $(MAKE) --no-print-directory locked-$@

locked-ghostty:
	@if $(GHOSTTY_VENDOR) --verify; then \
		echo "сборка ghostty не нужна"; \
	else \
		echo "собираю набор ghostty заново"; \
		build_dir="$$(mktemp -d "$${TMPDIR:-/tmp}/shellio-ghostty-set.XXXXXX")" && \
		trap 'rm -rf "$$build_dir"' EXIT && \
		$(GHOSTTY_VENDOR) --print-stamp >"$$build_dir/stamp" && \
		$(GHOSTTY_BUILD) "$$build_dir/set" && \
		$(GHOSTTY_VENDOR) publish "$$build_dir/set" "$$build_dir/stamp"; \
	fi

locked-check:
	$(GHOSTTY_VENDOR) --verify
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked

locked-bindings:
	$(GHOSTTY_VENDOR) --verify
	$(GHOSTTY_BINDINGS)

locked-bindings-check:
	$(GHOSTTY_VENDOR) --verify
	$(GHOSTTY_BINDINGS) --check

# Весь архив без -dead_strip; дольше make check, поэтому входит в CI и приёмку, но не в check.
locked-link-audit:
	$(GHOSTTY_VENDOR) --verify
	$(GHOSTTY_LINK_AUDIT)

# bundle.sh сверяет набор сам, перед раскладкой бандла.
locked-bundle:
	CARGO=$(CARGO) $(BUNDLE)

locked-check-bundle:
	$(GHOSTTY_VENDOR) --verify
	$(CHECK_BUNDLE)

locked-run:
	$(GHOSTTY_VENDOR) --verify
	$(CHECK_BUNDLE) --open

# Отказы check-bundle.sh на копиях бандла; нужна настоящая идентичность, поэтому не в test-scripts.
locked-test-bundle:
	$(GHOSTTY_VENDOR) --verify
	$(TEST_BUNDLE)

test-scripts:
	scripts/ghostty/test-publish.sh
	scripts/signing/test-identity.sh

# Пробы фазы 0 (ADR-0034) идут вне check. SCENARIOS — имена сценариев через пробел, пусто — все;
# ARGS — флаги демонстрации.
# Проба P0.2 probe-appkit набор ghostty не читает и идёт без блокировки.
probe-appkit:
	CARGO=$(CARGO) $(PROBE_APPKIT) scenarios $(SCENARIOS)

probe-appkit-demo:
	CARGO=$(CARGO) $(PROBE_APPKIT) demo $(ARGS)

# Проба P0.3 probe-ghostty читает набор ghostty и держит блокировку весь рецепт, демонстрация — пока
# открыто её окно; отказ «набор ghostty занят» у другой цели в это время ожидаем (ADR-0034).
locked-probe-ghostty:
	CARGO=$(CARGO) $(PROBE_GHOSTTY) scenarios $(SCENARIOS)

locked-probe-ghostty-demo:
	CARGO=$(CARGO) $(PROBE_GHOSTTY) demo $(ARGS)

# Проба P0.4 probe-session headless: набор ghostty не читает и идёт без блокировки. clippy и тесты
# с функцией vt идут здесь, а не в check (ADR-0034, «Уточнено в P0.4»; ADR-0035).
probe-session:
	CARGO=$(CARGO) $(PROBE_SESSION) scenarios $(SCENARIOS)

# Проба P0.4 в поверхности libghostty: читает набор ghostty и держит блокировку весь рецепт;
# clippy и тесты с функцией surface идут здесь (ADR-0034, ADR-0035).
locked-probe-session-surface:
	CARGO=$(CARGO) $(PROBE_SESSION) surface $(SCENARIOS)
