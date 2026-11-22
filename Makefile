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
RUN := scripts/run.sh
E2E_SINGLE_INSTANCE := scripts/e2e/single-instance.sh
PROBE_APPKIT := spikes/probe-appkit-shell/run.sh
PROBE_GHOSTTY := spikes/probe-ghostty-surface/run.sh
PROBE_SESSION := spikes/probe-session-survival/run.sh
PROBE_UPDATE := spikes/probe-update-lifecycle/run.sh
PROBE_DIFF := spikes/probe-diff-view/run.sh
PROBE_REVIEW := spikes/probe-review-api/run.sh
PROBE_RESOURCES := spikes/probe-resource-accounting/run.sh

# Цели, читающие vendor/ghostty/, выполняют весь рецепт под одной блокировкой набора: рецепт цели X
# лежит во внутренней цели locked-X и запускается вложенным make. Вторая такая цель сразу получает отказ.
LOCKED_TARGETS := ghostty check bindings bindings-check link-audit bundle check-bundle run test-bundle e2e \
	probe-ghostty probe-ghostty-demo probe-session-surface

.PHONY: setup test-scripts probe-appkit probe-appkit-demo probe-session probe-update probe-update-tcc-grant probe-diff probe-diff-demo probe-review probe-review-live probe-resources $(LOCKED_TARGETS) $(addprefix locked-,$(LOCKED_TARGETS))

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

# Бандл собирается и проверяется скриптами внутри той же блокировки: вложенный вызов публичных целей
# bundle и check-bundle получил бы «набор ghostty занят». Владелец работает с временным профилем до выхода.
# ARGS — аргументы владельца, например ARGS="--gallery".
locked-run:
	CARGO=$(CARGO) $(BUNDLE)
	$(CHECK_BUNDLE)
	$(RUN) -- $(ARGS)

# Продуктовый рубеж P1.2 вне check и CI: сценарий второго запуска. Бандл собирается и проверяется скриптами
# внутри той же блокировки, shellioctl сценарий собирает сам. Нужны идентичность Shellio Dev, открытый сеанс GUI,
# разблокированный экран и запущенный Finder; цель запускается из процесса-потомка приложения терминала, а не из
# tmux, screen или ssh. Итог — result.json в target/e2e/single-instance/<время>-<pid>/, код сценария 3 —
# обязательный пункт не pass, 1 — ошибка исполнения.
locked-e2e:
	CARGO=$(CARGO) $(BUNDLE)
	$(CHECK_BUNDLE)
	CARGO=$(CARGO) $(E2E_SINGLE_INSTANCE)

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

# Проба P0.4 probe-session headless: набор ghostty не читает и vendor/ghostty.lock не берёт. Наборы
# пробы zmx, zmx-prev и vt run.sh собирает или сверяет под их собственными блокировками <набор>.lock,
# занятая сразу даёт отказ. clippy и тесты с функцией vt идут здесь, а не в check (ADR-0034,
# «Уточнено в P0.4»; ADR-0035).
probe-session:
	CARGO=$(CARGO) $(PROBE_SESSION) scenarios $(SCENARIOS)

# Проба P0.4 в поверхности libghostty: читает набор ghostty и держит vendor/ghostty.lock весь рецепт,
# наборы пробы собирает или сверяет под их блокировками, как probe-session; clippy и тесты с функцией
# surface идут здесь (ADR-0034, ADR-0035).
locked-probe-session-surface:
	CARGO=$(CARGO) $(PROBE_SESSION) surface $(SCENARIOS)

# Проба P0.5 probe-update набор ghostty не читает и идёт без блокировки: run.sh собирает только её
# пакет, для сценариев не из стенда ещё поколения бандлов, подписанные Shellio Dev. Сценарий smoke
# проверяет разрешения Accessibility и Screen Recording терминала, из которого идёт make (ADR-0034).
probe-update:
	CARGO=$(CARGO) $(PROBE_UPDATE) scenarios $(SCENARIOS)

# Шаг выдачи TCC для владельца между запусками P0.5 (план, «Предусловия»): сброс разрешений
# SystemPolicyDocumentsFolder только идентификаторов пробы с журналом tccutil.log, затем бандл пробы
# («Разрешить») и контроль .denied («Не разрешать»), запись tcc-grant.json. В check и CI не входит.
probe-update-tcc-grant:
	CARGO=$(CARGO) $(PROBE_UPDATE) tcc-grant

# Проба P0.6 probe-diff набор ghostty не читает и идёт без блокировки vendor/ghostty.lock. VARIANTS
# («a b») и MODES («unified side») сужают оси сценариев вида, FIXTURES («added modified sparse wrap
# real threads») — ось фикстур сценариев open, open-warm, frames, anchor и pairs, пусто — все;
# FRAMES_PATH («none», «xctrace», «screencapturekit», пусто — none) — путь замера кадров frames,
# работает только none: остальные дают ошибку исполнения, ни один путь не откалиброван (ADR-0034).
probe-diff:
	CARGO=$(CARGO) VARIANTS="$(VARIANTS)" MODES="$(MODES)" FIXTURES="$(FIXTURES)" FRAMES_PATH="$(FRAMES_PATH)" $(PROBE_DIFF) scenarios $(SCENARIOS)

probe-diff-demo:
	CARGO=$(CARGO) $(PROBE_DIFF) demo $(ARGS)

# Проба P0.7 probe-review набор ghostty не читает и vendor/ghostty.lock не берёт. run.sh держит
# замерную блокировку measure.lock весь рецепт (срок захвата 5 минут, иначе код 4 «не начат»);
# снаружи цели lockf не оборачивается. HOSTS сужает столбцы, SERIES объединяет вызовы в серию.
# probe-review — столбец fake (forge-fake на loopback), probe-review-live — живые песочницы и
# локальный GitLab CE в docker (ADR-0034).
probe-review:
	CARGO=$(CARGO) HOSTS="$(HOSTS)" SERIES="$(SERIES)" $(PROBE_REVIEW) fake $(SCENARIOS)

probe-review-live:
	CARGO=$(CARGO) HOSTS="$(HOSTS)" SERIES="$(SERIES)" $(PROBE_REVIEW) live $(SCENARIOS)

# Проба P0.8 probe-resources headless, набор ghostty не читает и vendor/ghostty.lock не берёт. Замерные
# сценарии run.sh запускает под машинной замерной блокировкой measure.lock в SHELLIO_CACHE_DIR с
# проверкой фона, процесс без блокировки или тишины даёт код 3 «не начат». SCENARIOS и POINTS сужают
# оси, RUN продолжает прогон по его status (ADR-0034).
probe-resources:
	CARGO=$(CARGO) POINTS="$(POINTS)" RUN="$(RUN)" $(PROBE_RESOURCES) scenarios $(SCENARIOS)
