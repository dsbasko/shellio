# shellio

Нативный для macOS хаб разработчика на Rust. Начните с [docs/README.md](docs/README.md). Решения записаны в [docs/adr](docs/adr/README.md), словарь — в [ADR-0001](docs/adr/0001-vocabulary.md).

## Границы

- Весь собственный код пишется на Rust. Интерфейс строится на AppKit через `objc2`.
- `unsafe` Objective-C допустим только в `shellio-appkit` и `shellio-ghostty`, а также в пробах `spikes/` вместе с системными C API ([ADR-0034](docs/adr/0034-probe-code.md)). Продукт от `spikes/` не зависит.
- Чистые crate не зависят от AppKit.
- Главный поток никогда не блокируется. Git, сеть и файлы работают вне него.
- Длинные операции отменяемы. Закрытие вида отменяет только работу, привязанную к виду. Сессия и начатая запись во внешний сервис живут по своим правилам.

## Плагины

Всё, что открывается в рабочей области, является плагином в `plugins/<id>` с контрактом `shellio-plugin-api`.

- Плагин состоит из UI-free модели и фабрики вида на главном потоке, собранной из безопасных компонентов. Сырые `objc2` и `unsafe` в плагинах запрещены.
- Плагин не зависит от другого плагина. Взаимодействие только через команды шины.
- Конкретные плагины регистрируются только в `apps/shellio`.
- В манифесте обязательны `placements`, `default_zone`, `cardinality` и `min_size`.
- `open-or-focus` выполняется через слой команд, а не вручную.

## Адаптация к зонам

Встроенные плагины объявляют все четыре зоны: левую, правую, центральную и нижнюю. Панель обязана быть удобной в каждой из них.

- Зона делится на части по одной оси: левая и правая одна над другой, центр и нижняя рядом. Вложенных делений нет, навигатор не делится ([ADR-0031](docs/adr/0031-zone-splits.md)).
- `adapt` разбирает `Zone` исчерпывающим `match` без `_` и получает геометрию части зоны.
- Учитываются зона и размер: `Narrow`, `Regular`, `Wide`, `ShortBar`.
- Одна модель панели и разные представления, не четыре реализации.
- Боковая зона: список и детали вглубь, unified diff. Центр: полное представление. Нижняя зона: компактное размещение в ширину.
- Ниже `min_size` показывается заглушка «Развернуть», а не нечитаемая миниатюра.
- Каждое действие панели доступно с клавиатуры в любой зоне, в компактном виде через раскрытие вглубь или `zoom`.
- Плагин обязан проходить контрактный тест геометрии. Поведение каждой панели указано в `docs/ux.md`, изменения вносятся туда же.

## Действия

Новое действие считается законченным, когда есть: команда, обработчик, CLI в `shellioctl`, чтение результата и тесты. Меню, клавиша, палитра и CLI вызывают один обработчик. У действия есть имя в keymap.

## Интерфейс

- Цвета, радиусы и отступы только из токенов темы.
- Все поверхности непрозрачны. Материалы с прозрачностью и размытие не используются ([ADR-0023](docs/adr/0023-opaque-window-and-shell.md)).
- Левая и правая зоны строятся обычными элементами `NSSplitViewItem`: в режимах сайдбара и инспектора macOS 27 добавляет слои Liquid Glass, которые непрозрачная заливка не убирает ([ADR-0023](docs/adr/0023-opaque-window-and-shell.md)).
- Обязательны светлая и тёмная темы, Increase Contrast и Reduce Motion.
- Заголовок панели — вкладка. Отдельную шапку панель не заводит.
- Панель с фокусом контрастнее остальных ([ADR-0027](docs/adr/0027-focused-panel-contrast.md)) и выделяется тоном цвета режима и линией под активной вкладкой ([ADR-0024](docs/adr/0024-main-screen-design.md)). Идущая работа показана ровной синей точкой и индикатором внутри панели, остальные цвета только для внимания, ошибки и успеха.
- Наложения только пяти типов из ADR-0023. Состояние панели показывается внутри неё.
- Один элемент управления переиспользуется, а не создаётся заново.
- Индикатор исчезает вместе с тем, что он описывает.
- Клавиша действует на видимое. Скрытые слои её не получают.

## Сессии и терминал

- Панель, сессия и агент — разные сущности. Перенос панели не меняет ни сессию, ни агента.
- Каждый агент открывается своей панелью, отдельного списка агентов нет: они перечислены в «Сессиях» навигатора ([ADR-0030](docs/adr/0030-agent-panel-per-agent.md)). Переписка живёт в отдельной панели ([ADR-0028](docs/adr/0028-agents-tab-and-threads.md)). Управляемый nvim один на рабочую область и запускается по необходимости ([ADR-0029](docs/adr/0029-single-nvim-per-workbench.md)).
- Закрыть вид и остановить процесс — разные действия.
- Поверхность libghostty освобождается в одном месте. Колбэки C не трогают главное состояние напрямую.
- Ревизия Ghostty закреплена вместе с патчами, версией Zig и ресурсами. Её обновление — отдельный план по процедуре ADR-0003.
- На запрос терминала отвечает ровно одна сторона (ADR-0005).
- Сообщения агентов не печатаются в их composer.

## Ресурсы

- Для процессов показываются измеренные значения, для данных внутри приложения оценка с пометкой.
- Общая часть не делится между панелями. Оценка не складывается с физической памятью и не вычитается из неё.
- Панель не запускает собственный таймер замеров.

## Качество

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, edition 2024.
- `unwrap` и `expect` только в тестах и настоящих инвариантах. Каждый crate объявляет `[lints] workspace = true`, иначе запрет на него не действует.
- Комментарии только там, где код не может объяснить себя.
- Рубеж: `make check`. Для продуктового UI также `make e2e` и приёмочный проход исполнителя. GUI-проба проверяется своей целью `make probe-<имя>` и приёмочным проходом исполнителя ([ADR-0034](docs/adr/0034-probe-code.md), [ADR-0036](docs/adr/0036-autonomous-execution.md)).
- Тесты не касаются настоящего каталога состояния, не используют `sleep` для синхронизации и не ходят в настоящую сеть.
- Запуск приложения агентом только с изолированным `SHELLIO_STATE_DIR`.
- Код из GPL-проектов не копируется ([ADR-0022](docs/adr/0022-licensing-and-borrowing.md)).

## Сборка

- Рабочий путь — цели `Makefile`. Прямой `cargo build` сверяет набор ghostty только при перезапуске `build.rs` и блокировку не берёт.
- `make setup` ставит Rust из `rust-toolchain.toml`, Zig по SHA-256, Metal toolchain и `bindgen-cli`. `make ghostty` собирает и публикует набор в `vendor/ghostty/` ([ADR-0003](docs/adr/0003-terminal-libghostty.md)), если `--verify` не проходит, иначе только сверяет его.
- `make check` — `--verify`, `cargo fmt --check`, `clippy` и тесты. `make test-scripts` — тесты публикации набора и идентичности подписи, тесту идентичности нужен OpenSSL 3 или новее в `PATH` или в `OPENSSL`: `/usr/bin/openssl` — это LibreSSL. `make bindings` генерирует привязки, `make bindings-check` сверяет их с репозиторием. `make link-audit` линкует весь архив и в `check` не входит.
- `make bundle` собирает и подписывает `target/bundle/shellio.app`, `make check-bundle` проверяет его, `make run` запускает через `open` с временным `SHELLIO_STATE_DIR`. `make test-bundle` проверяет отказы `check-bundle.sh`, ему нужна идентичность `Shellio Dev`.
- `make probe-appkit [SCENARIOS="…"]` собирает пробу P0.2 `spikes/probe-appkit-shell` и запускает её GUI-сценарии отдельными процессами с временным `SHELLIO_STATE_DIR`, выход — `target/probes/appkit-shell/<время>-<pid>/`. `make probe-appkit-demo [ARGS="…"]` открывает её демонстрационную сцену. Обе цели набор ghostty не читают, блокировку не берут и в `check` не входят, условия прогона — в [отчёте пробы](docs/probes/p0.2-appkit-shell.md) ([ADR-0034](docs/adr/0034-probe-code.md)).
- `make probe-ghostty [SCENARIOS="…"]` собирает пробу P0.3 `spikes/probe-ghostty-surface` в собственный бандл `target/probes/ghostty-surface/bundle/probe-ghostty-surface.app` и запускает её сценарии отдельными процессами с временным `SHELLIO_STATE_DIR`, выход — `target/probes/ghostty-surface/<время>-<pid>/`. `make probe-ghostty-demo [ARGS="…"]` открывает её демонстрационную сцену с живой поверхностью. Бандл пробы подписывается идентичностью `Shellio Dev` по тому же SHA-1, но под своим идентификатором `io.github.dsbasko.shellio.probe.ghostty-surface` и со своим designated requirement; `make bundle` его не собирает. Обе цели читают набор ghostty и держат блокировку `vendor/ghostty.lock` весь рецепт, демонстрация — пока открыто её окно, отказ «набор ghostty занят» у другой цели в это время ожидаем. В `check` они не входят, условия прогона — в [отчёте пробы](docs/probes/p0.3-ghostty-surface.md) ([ADR-0034](docs/adr/0034-probe-code.md), «Уточнено в P0.3»).
- `make probe-session [SCENARIOS="…"]` собирает пробу P0.4 `spikes/probe-session-survival` и запускает её headless-сценарии, замеры и автоматическую часть прохода F4 `apps` отдельными процессами с временным `SHELLIO_STATE_DIR`, выход — `target/probes/session-survival/<время>-<pid>/`. `make probe-session-surface [SCENARIOS="…"]` запускает её сценарии в поверхности libghostty, включая `apps-surface`, из собственного бандла `target/probes/session-survival/bundle/probe-session-surface.app`: он подписывается идентичностью `Shellio Dev` по тому же SHA-1 под идентификатором `io.github.dsbasko.shellio.probe.session-survival` и со своим designated requirement, `make bundle` его не собирает. Наборы пробы — zmx (`zmx`, `zmx-prev`) и `libghostty-vt` (`vt`) в `target/probes/session-survival/<набор>/` — перед сценариями обеих целей собирают или сверяют `scripts/zmx.sh` и `scripts/vt.sh` пробы, каждый под своей блокировкой `<набор>.lock`, занятая сразу даёт отказ. Zig и исходники zmx кэшируются в `SHELLIO_CACHE_DIR/zmx/`. Набор `vt` берёт ревизию, Zig и патчи из `scripts/ghostty/pin` и `scripts/ghostty-patches/`, а исходники — из git-копии кэша, которую заводит `make ghostty`. Код, которому нужен набор пробы, стоит за cargo-функциями `vt` и `surface`: их clippy и тесты идут в этих целях, а `check` собирает и тестирует пробу без функций ([ADR-0035](docs/adr/0035-probe-ffi-tests.md)). Функции `vt` и `surface` одним вызовом cargo не включаются: `libghostty-vt.a` и `libghostty.a` не попадают в одну линковку. `make probe-session` набор ghostty не читает и `vendor/ghostty.lock` не берёт, `make probe-session-surface` читает его и держит `vendor/ghostty.lock` весь рецепт. В `check` они не входят, условия прогона — в [отчёте пробы](docs/probes/p0.4-session-survival.md) ([ADR-0034](docs/adr/0034-probe-code.md), «Уточнено в P0.4»).
- `make probe-update [SCENARIOS="…"]` собирает пробу P0.5 `spikes/probe-update-lifecycle` и запускает её сценарии отдельными процессами под `caffeinate -dimsu` с временным `SHELLIO_STATE_DIR`, выход — `target/probes/update-lifecycle/<время>-<pid>/`. Для всех сценариев, кроме сценариев стенда `smoke` и `host`, `run.sh` сначала собирает поколения 1, 2 и кандидат `incompatible` отдельными вызовами cargo в `target/probe-update-gen/<N>/` и подписывает бандлы стенда в `target/probes/update-lifecycle/bundle/`: поколения и кандидаты идентичностью `Shellio Dev` под `io.github.dsbasko.shellio.probe.update-lifecycle`, вспомогательные бинари под `<id>.helper`, `<id>.host`, `<id>.attach`, `<id>.fixture`, бандл-контроль под `<id>.denied`, кандидат `foreign` временной идентичностью во временной keychain; кандидату `foreign` нужен OpenSSL 3 или новее, как `make test-scripts`. `make probe-update-tcc-grant` — шаг выдачи TCC владельца между запусками P0.5: сброс `SystemPolicyDocumentsFolder` только идентификаторов пробы с журналом `target/probes/update-lifecycle/tccutil.log`, затем бандл пробы («Разрешить») и контроль `.denied` («Не разрешать»), запись `tcc-grant.json`; `tccutil` вызывает только эта цель. Обе цели набор ghostty не читают и `vendor/ghostty.lock` не берут. Терминалу, из которого идёт `make probe-update`, нужны Accessibility и Screen Recording (их проверяет сценарий `smoke`), сеанс GUI открыт, окна пробы ничто не накрывает. Сценарии `keychain` и `brew` заводят временную keychain и изолированный Homebrew и убирают их сами, login keychain и `/opt/homebrew` только читаются. В `check` цели не входят, условия прогона — в [отчёте пробы](docs/probes/p0.5-update-lifecycle.md) ([ADR-0034](docs/adr/0034-probe-code.md)).
- `make probe-diff [SCENARIOS="…"] [VARIANTS="a b"] [MODES="unified side"] [FIXTURES="…"] [FRAMES_PATH=none]` собирает пробу P0.6 `spikes/probe-diff-view` и запускает её сценарии отдельными процессами под `caffeinate -dimsu` с временным `SHELLIO_STATE_DIR`, выход — `target/probes/diff-view/<время>-<pid>/`. Сценарий стенда (`smoke`, `highlight`, `calibrate`) идёт одним процессом, сценарий вида — процессом на каждую пару варианта отрисовки (`a` — собственный `NSView`, `b` — `NSTableView`) и режима (`unified`, `side`); `VARIANTS` и `MODES` сужают эти оси. Сценарий памяти `memory` идёт процессом на вариант: режим меняется внутри процесса по фазам, `MODES` его не сужает. Сценарии контракта корректности `anchor` и `pairs` идут также по фикстурам (`anchor` — `threads`, `wrap`, `pairs` — `modified`, `wrap`), `selection`, `threads` и `bounded` — процессом на вариант и режим со своей фикстурой. Сценарии зон и тем `zones` и `theme` идут процессом на вариант и режим с фикстурой `modified`, `theme` пишет снимок окна каждой темы в каталог процесса. Приёмочный ход `accept` идёт процессом на вариант и режим с фикстурой `real`, `FIXTURES` его не сужает. Сценарии открытия `open` и `open-warm` идут также по фикстурам (`added`, `modified`, `sparse`, `wrap`, `real`), `open` — 11 свежими процессами на ячейку. Сценарий кадров `frames` идёт по фикстурам `added`, `modified`, `threads` в обоих режимах и `wrap` только в `side`, путь замера кадров задаёт `FRAMES_PATH`: работает только `none` (по умолчанию), `xctrace` и `screencapturekit` дают ошибку исполнения до окна, потому что ни один путь не прошёл калибровку. Сценарий `calibrate` пишет трассу `xctrace` и поток ScreenCaptureKit в процессе сценария, ему нужен встроенный дисплей 120 Гц: при закрытой крышке он даёт ошибку исполнения, и прогон идёт с явным `SCENARIOS` без него. `FIXTURES` сужает ось фикстур каждого сценария пересечением с его списком. Пустое пересечение у любого выбранного сценария или сценарий без единого процесса после сужения — отказ до сборки, поэтому вместе с `FIXTURES` задаётся и `SCENARIOS`. Фикстуру `real` `run.sh` готовит через `git` без сети в `target/probes/diff-view/fixtures/real/`, без закреплённых коммитов её сценарии получают `not_run`. Терминалу, из которого идёт `make probe-diff`, нужны Accessibility и Screen Recording, сеанс GUI открыт, экран разблокирован, AeroSpace не запущен, клавиатуру и мышь во время прогона не трогают: иначе сценарий с окном получает ошибку исполнения «предусловие: …» или «внешний ввод». `make probe-diff-demo [ARGS="--variant a|b --mode unified|side --fixture <имя>"]` открывает её демонстрацию без стенда и `result.json`, фикстуру `real` она готовит так же и без закреплённых коммитов отказывает. Обе цели набор ghostty не читают, `vendor/ghostty.lock` не берут и в `check` не входят, условия прогона — в [отчёте пробы](docs/probes/p0.6-diff-view.md) ([ADR-0034](docs/adr/0034-probe-code.md)).
- `make probe-resources [SCENARIOS="…"] [POINTS="…"] [RUN=<run-id>]` собирает пробу P0.8 `spikes/probe-resource-accounting` в `release` и запускает её headless-сценарии отдельными процессами под `caffeinate -i` с временным `SHELLIO_STATE_DIR`, выход — `target/probes/resource-accounting/<run-id>/`, затем `summary.json` и `summary.md`. Сценарий `cost` идёт процессом на точку оси (`pids-8`, `pids-32`, `pids-128`, `panels-16`, `background`, `idle`), `POINTS` сужает точки, пустое пересечение с точками выбранного сценария — отказ до запуска. Замерные сценарии (`smoke`, `units`, `formula-cpu`, шесть `span-*` и `cost`) `run.sh` запускает под машинной замерной блокировкой `${SHELLIO_CACHE_DIR:-~/.cache/shellio}/measure.lock` (`/usr/bin/lockf -k -t 300`), под ней процесс ждёт тишины фона окнами по 10 с. Процесс, который за 5 минут не взял блокировку или не дождался тишины, получает «не начат»: `run.sh` выходит с кодом 3 (у `make` — `Error 3` и строка `повтор: RUN=<run-id>`), это не ошибка исполнения, процесс повторяется вызовом с тем же `RUN`, третий подряд «не начат» записывает `not_run`. `RUN=<run-id>` продолжает прогон по его `status`: готовые процессы пропускаются, другая сборка или другие параметры протокола заводят новый run-id. Точка `cost` держит блокировку до 25 минут. Замер искажает любая фоновая нагрузка, поэтому во время замерных сценариев сборки и другие пробы на машине не идут, машина на питании от сети. Сценарий `login` запускает настоящий `/usr/bin/login -f` с setuid от текущего пользователя, отказ даёт `not_run` «login не запустился». Тесты логики маркера блокировки и сброса каталога процесса в `run.sh` идут в `check`. Цель набор ghostty не читает, `vendor/ghostty.lock` не берёт и в `check` не входит, условия прогона — в [отчёте пробы](docs/probes/p0.8-resource-accounting.md) ([ADR-0034](docs/adr/0034-probe-code.md), «Уточнено в P0.8»).
- Набор с другим штампом не линкуется. Расхождение `scripts/ghostty/vendor.sh --verify` исправляет `make ghostty`. Штамп, манифест и файлы набора вручную не правятся.
- Закрепление набора — `scripts/ghostty/pin`, список линковки — только `scripts/ghostty/link-libs`. `crates/shellio-sys-ghostty/src/bindings.rs` вручную не правится: новые функции добавляются в allowlist `scripts/ghostty/bindings.sh`, затем `make bindings`.
- Инструменты и исходники ghostty кэшируются в `~/.cache/shellio`, каталог задаёт `SHELLIO_CACHE_DIR`. CI (`.github/workflows/ci.yml`) выполняет те же цели, кроме `bindings`, `test-bundle`, `run`, `probe-appkit`, `probe-appkit-demo`, `probe-ghostty`, `probe-ghostty-demo`, `probe-session`, `probe-session-surface`, `probe-update`, `probe-update-tcc-grant`, `probe-diff`, `probe-diff-demo` и `probe-resources`; его пути кэша повторяют пути `tools.sh` и `build.sh`.
- Доступ к `vendor/ghostty/` последовательный. Цели, читающие набор, держат блокировку `vendor/ghostty.lock` весь рецепт, вторая такая цель сразу получает отказ «набор ghostty занят». Публикует набор только `make ghostty`.
- Бандл подписывается только идентичностью `Shellio Dev` по SHA-1 из `scripts/signing/requirement.txt`, ad-hoc подпись не подставляется ([ADR-0019](docs/adr/0019-distribution-and-updates.md)).

## Документы

- Значимое решение оформляется как ADR до реализации. Принятое решение меняется новым ADR.
- Исполнение автономное ([ADR-0036](docs/adr/0036-autonomous-execution.md)). Владелец участвует только в планировании. Исполнитель не останавливается ради решения или приёмки владельца: предвидимый выбор делается по правилу, записанному в плане, непредвиденный — самим исполнителем с записью в плане. Ручную приёмку заменяет приёмочный проход исполнителя, а то, чего автоматика не наблюдает, получает `not_run` с причиной, а не `pass`.
- Планы лежат в `docs/plans/YYYYMMDD-<slug>.md`, завершённые переносятся в `docs/plans/completed/`. Планы хранятся в git: исполнитель коммитит план вместе с изменениями каждой задачи, перенос в `completed/` тоже коммитится.
- Последний пункт плана вместе с переносом в `completed/` отмечает строку его шага в [дорожной карте](docs/roadmap.md): ⬜ → ✅ в колонке «Готов». Шаг из нескольких планов отмечает последний из них.
- Отчёт пробы фазы 0 лежит в `docs/probes/<id>-<slug>.md`, выбранные сырые данные — в `docs/probes/data/<id>-<slug>/`, полные прогоны остаются в `target/`. ADR получает вывод пробы и ссылку разделом «Уточнено в P0.x». Отрицательный вывод цель пробы не роняет, роняет только ошибка исполнения ([ADR-0034](docs/adr/0034-probe-code.md)).
- Новое поведение панелей и потоков отражается в `docs/ux.md`.
