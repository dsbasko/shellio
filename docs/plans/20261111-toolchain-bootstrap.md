# Сборочная база: Rust, Zig, libghostty и подпись

## Overview

- Шаг P0.1 `toolchain-bootstrap` [дорожной карты](../roadmap.md). Это первый код репозитория, от него зависят все остальные планы.
- Закрепляет набор сборки libghostty из [ADR-0003](../adr/0003-terminal-libghostty.md), заводит Cargo workspace, `make check` из [ADR-0021](../adr/0021-testing.md), CI и постоянную идентичность `Shellio Dev` из [ADR-0019](../adr/0019-distribution-and-updates.md).
- Готово, когда пустой бандл собирается, линкует libghostty и запускается с постоянной подписью: запуск через `open` даёт успешную самопроверку, две чистые сборки подряд дают одинаковый designated requirement, подпись не ad-hoc.

## Context (from discovery)

- В репозитории только `AGENTS.md`, `CLAUDE.md`, `docs/` и `prototype/`. Коммитов, remote и `.gitignore` нет.
- На машине есть Xcode 27.0, Homebrew и `gh`. Metal toolchain не установлен: `xcrun metal --version` сообщает «missing Metal Toolchain». Нет Rust, Zig, `bindgen` и идентичностей подписи. libclang из Xcode разбирает `ghostty.h` для `arm64-apple-macos27` без диагностик. Утилиты `flock` нет, есть `/usr/bin/lockf`.
- Образец сборки — `agterm/scripts/setup.sh` (MIT): ghostty `83fb95624b0439ee660c2679b43b38c8b63769cd` (v1.4.0), патч `0001-drain-mailbox-during-surface-teardown.patch`, native xcframework. Только эта связка ревизии и патча подтверждена на macOS 27.
- Факты закреплённой ревизии:
  - `minimum_zig_version = "0.17.0"`: проверка версии допускает 0.17.x, но собирается и принимается только закреплённый 0.17.1;
  - статическая библиотека для внешней линковки — `macos/GhosttyKit.xcframework/macos-arm64/libghostty-internal.a`, в неё уже объединены зависимости. Флага, который ставит её на macOS без xcframework, нет;
  - собственный минимум macOS у ghostty — 13.0 (`genericMacOSTarget`, `osVersionMin`), цель сборки его не меняет;
  - `-Dsentry` на macOS по умолчанию включён, и `ghostty_init` создаёт базу Sentry в каталогах Ghostty, которых `SHELLIO_STATE_DIR` не касается. Владелец решил собирать без Sentry;
  - `ghostty_info` возвращает статические сведения о сборке и инициализации не требует;
  - для P0.3: `ghostty_init` ставит обработчики сигналов процесса, меняет locale и сохраняет указатели `argv`, не копируя их. При пустом `LANG` он вызывает `NSLocale currentLocale`, который поднимает число потоков с 1 до 3, и только потом делает `setenv("LANG")` и `setenv("LANGUAGE")`. Поэтому проверка одного потока на входе контракт не доказывает;
  - для P0.3: в `ReleaseFast` каталог ресурсов берётся сначала из `GHOSTTY_RESOURCES_DIR`, и только без неё ищется `Contents/Resources/terminfo/78/xterm-ghostty` вверх от исполняемого файла. Родительский терминал Ghostty или agterm эту переменную задаёт;
  - в `ghostty.h` версии нет, она приходит только полями `version` и `version_len` результата `ghostty_info`; без `-Dversion-string` версия выводится из git и у неглубокой копии может получить суффикс;
  - `zig build` сам скачивает зависимости из `build.zig.zon`, часть из `deps.files.ghostty.org`.
- Hosted runner GitHub `xcode-27` (public preview, arm64): macOS 27.0.1, Xcode 27.0 в `/Applications/Xcode_27.app`, по умолчанию выбран 27.1. Образ переходит с OpenSSL 3 на 4.
- Самоподписанный сертификат OpenSSL с назначением Code Signing подписывает бинарь с явным requirement `certificate leaf = H"…"` и проходит `codesign --verify --strict` на macOS 27.0.1 без доверия к сертификату. Проверено во временном разблокированном keychain с переданными паролями; поведение login keychain может отличаться.
- `-force_load` вместе с `-dead_strip` скрывает неразрешённые символы неиспользуемого кода: проверка полноты линковки идёт без `-dead_strip`. Вложенный `lockf -k -t 0` на том же файле возвращает 75.
- Патч и приёмы из agterm переносятся с уведомлением об авторстве ([ADR-0022](../adr/0022-licensing-and-borrowing.md)).
- Ни ghostty, ни Rust здесь ещё не собирались: путь к архиву, флаги и список фреймворков выведены из исходников и подтверждаются задачами 2 и 4.

## Development Approach

- **testing approach**: Regular. Код и его проверки входят в одну задачу.
- Каждая задача завершается до начала следующей. С задачи 1 рубеж — `make check`.
- Сеть нужна установке Rust, `bindgen-cli`, Zig и сборке ghostty, включая её зависимости. Тесты, `--print-stamp` и `--verify` в сеть не ходят.
- P0.1 обходится без внешних crate. Если какой-то понадобится, его лицензия проверяется по ADR-0022 и записывается в план.
- Тесты не используют `sleep` и не касаются настоящего каталога состояния. P0.1 не вызывает `ghostty_init`. Самопроверка запускается отдельным процессом с изолированным окружением; окружение самих `cargo` и `rustup` не меняется.
- План обновляется при изменении объёма работ.

## Testing Strategy

- **unit tests**: поиск ресурсов бандла в самопроверке, включая ошибки раскладки.
- **integration tests**: вызов `ghostty_info` из слинкованного теста, самопроверка во временной неподписанной раскладке бандла, проверки раскладки от `bindgen`, отдельная линковка всего архива, тест публикации набора на поддельном наборе, скрипт проверки бандла.
- **e2e tests**: нет, окна в P0.1 нет. Каркас E2E появится в P1.3.

## Progress Tracking

- Выполненные пункты отмечаются `[x]` сразу.
- Новые задачи добавляются с префиксом ➕, препятствия с префиксом ⚠️.

## Solution Overview

- Всё, что создаёт план, остаётся в продукте: workspace, `Makefile`, скрипты, CI, `shellio-sys-ghostty` и `shellio-ghostty`. Временная только заглушка `apps/shellio/src/main.rs`: она проверяет раскладку бандла и завершается, окно появится в фазе 1. Правило `spikes/` из дорожной карты относится к пробам P0.2–P0.8.
- По ADR-0002 и ADR-0003 вызовы libghostty и весь `unsafe` живут в `shellio-ghostty`. В P0.1 он даёт одну безопасную функцию `info()` со сведениями о сборке. Ей не нужна инициализация, поэтому P0.1 не вызывает `ghostty_init`. В заглушке `unsafe` нет.
- Запуск `ghostty_init` переносится в P0.3, где появляется поверхность. Его влияние на весь процесс — окружение, locale, потоки, сигналы, время жизни `argv` и приоритет `GHOSTTY_RESOURCES_DIR` — записывается в ADR-0003 как исходные данные для P0.3. Безопасного контракта запуска P0.1 не обещает.
- Набор ghostty разделён на рецепт и проверки. `scripts/ghostty/build.sh` загружает, патчит, собирает и извлекает в указанный временный каталог. `scripts/ghostty/vendor.sh` печатает ожидаемый штамп, сверяет опубликованный набор и публикует новый. Правка проверок не пересобирает ghostty.
- Сверку вызывает каждая цель `Makefile`, читающая набор. `build.rs` сверяет набор при своих перезапусках. Прямой `cargo build` после смены SDK без изменения отслеживаемых файлов сверку не гарантирует, и блокировка его не защищает, поэтому рабочий путь — цели `Makefile`.
- Доступ к `vendor/ghostty/` последовательный. Блокировку берёт только обёртка в `Makefile`. Публикация без маркера обёртки отказывается работать, а сверка и `build.rs` блокировку не берут. Вторая цель в другом терминале сразу завершается с ошибкой.
- Zig ставится не из Homebrew, а архивом точной версии с закреплённым SHA-256. В штампе тогда та версия, которая собирала.
- Набор ghostty собирается без Sentry и с явной версией: так будущий `ghostty_init` не пишет в каталоги Ghostty, а версия в рантайме сравнима со штампом.
- Минимумы macOS разные и записаны оба: приложение и тесты — 27.0 через `.cargo/config.toml`, libghostty — upstream 13.0.
- Привязки генерируются `bindgen` закреплённой версии и лежат в репозитории. В P0.1 в них только `ghostty_info`, P0.3 расширяет список. Обновление ревизии даёт читаемый diff привязок.
- Подпись выполняется по SHA-1 сертификата, а не по имени. Designated requirement задаётся явно и хранится в репозитории. Доверие к сертификату и hardened runtime не нужны для P0.1, сохранение разрешений TCC проверяет P0.5.

## Technical Details

- `scripts/ghostty/pin` — переменные для `source`: `GHOSTTY_REV`, `GHOSTTY_VERSION_STRING=1.4.0`, `GHOSTTY_MIN_MACOS=13.0`, `ZIG_VERSION=0.17.1`, `ZIG_SHA256=75f1bc7a1e9852f17ec9a0c1895f767c4ec56d46bc8a834920eece1d0359dd8d`. Флаги сборки `build.sh` собирает из них: `-Doptimize=ReleaseFast -Demit-xcframework=true -Dxcframework-target=native -Demit-macos-app=false -Dsentry=false -Dversion-string=$GHOSTTY_VERSION_STRING`.
- `scripts/ghostty/link-libs` — единственный список линковки: `static=ghostty`, фреймворки `AppKit`, `Foundation`, `Carbon`, `CoreFoundation`, `CoreGraphics`, `CoreText`, `CoreVideo`, `QuartzCore`, `IOSurface`, `Metal`, `GameController` (его требует бэкенд Dear ImGui), библиотеки `c++` и `objc`. Его читают и `build.rs`, и `link-audit.sh`.
- Минимум приложения задаётся отдельно от ghostty, в `.cargo/config.toml` корня workspace: `[env] MACOSX_DEPLOYMENT_TARGET = { value = "27.0", force = true }`. `force` перекрывает значение из родительского окружения. Так минимум получает и прямой `cargo`, запущенный из корня, а смена минимума приложения не пересобирает ghostty.
- Zig: `https://ziglang.org/download/<версия>/zig-aarch64-macos-<версия>.tar.xz` в `~/.cache/shellio/zig/<версия>`, кэш пакетов Zig — `ZIG_GLOBAL_CACHE_DIR=~/.cache/shellio/zig-global`.
- Корень набора по умолчанию `vendor/ghostty/`, тесты переопределяют его через `SHELLIO_GHOSTTY_VENDOR`. Набор в git не входит:

  ```text
  vendor/ghostty/
    lib/libghostty.a          копия libghostty-internal.a из xcframework
    include/ghostty.h
    share/ghostty/shell-integration/  share/ghostty/themes/
    share/terminfo/78/xterm-ghostty …
    LICENSE
    manifest
    stamp
  ```

- Штамп описывает входы сборки строками `ключ=значение`:
  - ревизия, значения из `pin` и флаги сборки;
  - дайджест имён и содержимого `scripts/ghostty-patches/*.patch` и дайджест `build.sh`;
  - SHA-256 Zig, архитектура;
  - путь Xcode (`DEVELOPER_DIR`, а без неё `xcode-select -p`) и `xcodebuild -version`;
  - версия и сборка SDK, версия Metal toolchain.

  Всё это известно до загрузки исходников, поэтому `--print-stamp` работает без сети и до сборки. Без Metal toolchain он завершается ошибкой с подсказкой `make setup`. Ключ кэша CI — SHA-256 текста штампа.
- Манифест: относительный путь и SHA-256 каждого файла набора, включая ресурсы и `terminfo`. Сами `manifest` и `stamp` в него не входят и остаются единственными допустимыми файлами вне него.
- Публичный `--verify` проверяет три вещи: штамп совпадает с ожидаемым, каждый файл манифеста на месте и с тем же хэшем, лишних и недостающих файлов нет. Удалённый после публикации `terminfo` тоже отвергается.
- Публикация (`vendor.sh publish <каталог сборки>`) поддерживается только как шаг цели `make ghostty`. Маркер `SHELLIO_GHOSTTY_LOCKED=1` — соглашение, а не доказательство захвата: без маркера публикация отказывает, но вручную выставленный маркер её не останавливает. Шаги публикации:
  1. проверка каталога сборки: файлы и ресурсы на месте, архив читается `ar`, `ghostty.h` совпадает с `include/ghostty.h` собранного дерева; построение манифеста по каталогу сборки;
  2. удаление старого `stamp`, замена содержимого, сверка опубликованного каталога с манифестом тем же кодом, что и в `--verify`;
  3. запись `stamp`.

  Набор без `stamp` публичный `--verify` отвергает, поэтому середина публикации не линкуется. Атомарности замены каталога скрипт не обещает. Версию ghostty в рантайме проверяет тест `info()` в задаче 5.
- Блокировка: файл `vendor/ghostty.lock` вне заменяемого каталога, `lockf -k -t 0`, файл не удаляется и не заменяется. Обёртка в `Makefile` берёт её на всё время рецепта целей `ghostty`, `check`, `bindings`, `bindings-check`, `link-audit`, `bundle`, `check-bundle`, `run` и `test-bundle` и выставляет `SHELLIO_GHOSTTY_LOCKED=1`. Вторая такая цель в другом терминале завершается с ошибкой «набор занят», а не ждёт. Пока идёт `make check`, соседний терминал собрать бандл не сможет, это принятая цена. `.NOTPARALLEL` не даёт `make -j` упасть на собственной блокировке.
- Версия ghostty для Rust: `build.rs` из `shellio-sys-ghostty` читает её из штампа и отдаёт константой через `cargo:rustc-env`.
- Ресурсы в заглушке: путь берётся один раз через `current_exe`, от него ищутся `Contents/Resources/ghostty` и `Contents/Resources/terminfo/78/xterm-ghostty`, то есть раскладка, которую найдёт алгоритм ghostty без `GHOSTTY_RESOURCES_DIR`. Ошибка `current_exe` завершает запуск с кодом 1 и причиной, путь через `argv[0]` или `PATH` не восстанавливается. `GHOSTTY_RESOURCES_DIR` в P0.1 не читается и не задаётся.
- Изоляция самопроверки: её запускают с временными `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME` и `SHELLIO_STATE_DIR`, и после выхода эти каталоги должны остаться пустыми.
- Раскладка бандла: `shellio.app/Contents/MacOS/shellio`, `Contents/Info.plist`, `Contents/Resources/ghostty/`, `Contents/Resources/terminfo/` рядом с ним (ADR-0003), `Contents/Resources/licenses/ghostty.txt`.
- `Info.plist` — шаблон в `apps/shellio/Info.plist`: `CFBundleIdentifier=io.github.dsbasko.shellio`, `CFBundleExecutable=shellio`, `CFBundlePackageType=APPL`, `LSMinimumSystemVersion=27.0`. Версии подставляет `bundle.sh` через `plutil -replace` из `[workspace.package]`.
- Requirement в `scripts/signing/requirement.txt`: `designated => identifier "io.github.dsbasko.shellio" and certificate leaf = H"<SHA-1 сертификата>"`. Сравнивается каноническая форма из `codesign -d -r-`, а не исходный текст. Хэш сертификата публичен, ключ в репозиторий не попадает.
- Цели `Makefile`:

  | Цель | Что делает |
  | --- | --- |
  | `setup` | растёт по задачам: Rust из `rust-toolchain.toml` (задача 1); Zig по SHA-256 и Metal toolchain, если `xcrun metal --version` не исполняется (задача 2); `bindgen-cli` точной версии с `--locked` (задача 4) |
  | `ghostty` | собирает и публикует набор, если `--verify` не проходит по любой причине, иначе сообщает, что набор актуален |
  | `check` | с задачи 3 сначала `--verify`; затем `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` |
  | `test-scripts` | тест публикации набора на поддельном наборе |
  | `bindings`, `bindings-check` | `--verify`; генерирует привязки; генерирует во временный файл и сравнивает с файлом в репозитории, не переписывая его |
  | `link-audit` | `--verify`; линкует весь архив без `-dead_strip` и проверяет `otool -L`; входит в CI и приёмку, но не в `make check` |
  | `bundle`, `check-bundle` | собирает и подписывает бандл; проверяет подпись, requirement, раскладку, зависимости и самопроверку |
  | `run` | запускает бандл через `open -W -n` с временным `SHELLIO_STATE_DIR`, сохраняет вывод и проверяет его |
  | `test-bundle` | `--verify`; проверяет отказы `check-bundle.sh` на копиях бандла, переподписанных настоящей идентичностью; не входит в `test-scripts` |

## What Goes Where

- **Implementation Steps**: файлы и проверки этого репозитория. Задача 7 выполняется вместе с владельцем.
- **Post-Completion**: резервная копия ключа, секреты CI, первый прогон CI, ручная приёмка.

## Implementation Steps

### Task 1: Каркас workspace, закреплённый Rust и уточнения ADR

**Files:**
- Create: `.gitignore`
- Create: `rust-toolchain.toml`
- Create: `Cargo.toml`
- Create: `Cargo.lock`
- Create: `.cargo/config.toml`
- Create: `rustfmt.toml`
- Create: `clippy.toml`
- Create: `Makefile`
- Create: `apps/shellio/Cargo.toml`
- Create: `apps/shellio/src/main.rs`
- Modify: `docs/adr/0003-terminal-libghostty.md`
- Modify: `docs/adr/0019-distribution-and-updates.md`
- Modify: `docs/roadmap.md`

- [x] дополнить ADR-0003 разделом «Уточнено в P0.1»: сборка без Sentry, Zig архивом с SHA-256, минимумы 27.0 и 13.0, состав штампа и манифеста, а также влияние `ghostty_init` на процесс из раздела Context как исходные данные для P0.3; дополнить ADR-0019 таким же разделом: bundle ID, явный requirement, подпись по SHA-1, доверие к сертификату не требуется. Сами решения не меняются
- [x] уточнить в `docs/roadmap.md`, что правило `spikes/` относится к пробам P0.2–P0.8, а P0.1 создаёт продуктовую сборочную базу
- [x] поставить `rustup`, если его нет, и закрепить в `rust-toolchain.toml` точную стабильную версию с `rustfmt`, `clippy` и целью `aarch64-apple-darwin`
- [x] описать workspace: `resolver = "3"`, edition 2024, общая версия, `[workspace.lints]` с запретом `clippy::unwrap_used` и `clippy::expect_used`; каждый crate наследует `lints.workspace = true`, `clippy.toml` разрешает их только в тестах
- [x] добавить в `.gitignore` `target/`, `vendor/`, `.DS_Store`, `*.p12`, `*.keychain-db`; `Cargo.lock` хранить в репозитории
- [x] задать `MACOSX_DEPLOYMENT_TARGET` в `.cargo/config.toml` по разделу Technical Details
- [x] завести пустую заглушку `apps/shellio`
- [x] добавить цели `setup` и `check` в `Makefile`; в этой задаче `setup` ставит только Rust, а `check` — только команды Cargo
- [x] `make check` проходит
- [x] после `cargo clean -p shellio` и прямого `cargo build --locked -p shellio` с `MACOSX_DEPLOYMENT_TARGET=14.0` в родительском окружении `xcrun vtool -show-build target/debug/shellio` показывает minos 27.0

### Task 2: Zig и сборка ghostty во временный каталог

**Files:**
- Create: `scripts/ghostty/pin`
- Create: `scripts/ghostty/build.sh`
- Create: `scripts/ghostty-patches/0001-drain-mailbox-during-surface-teardown.patch`
- Create: `scripts/ghostty-patches/README.md`
- Modify: `Makefile`

- [x] перенести патч из agterm и описать в README его назначение, условие удаления, источник и MIT-уведомление
- [x] загружать Zig архивом по `ZIG_VERSION` и `ZIG_SHA256`, отказываться при несовпадении суммы; добавить Zig и Metal toolchain в `make setup`
- [x] ➕ установка Zig и Metal toolchain вынесена в `scripts/ghostty/tools.sh`: её правка не меняет дайджест `build.sh` в штампе, а сумма архива закреплена в `pin`. `ZIG_MIRROR` задаёт зеркало Zig с плоской раскладкой, сумма проверяется так же. Без первого запуска Xcode `-downloadComponent` не загружает свои модули, поэтому скрипт сначала выполняет `xcodebuild -runFirstLaunch`
- [x] в `build.sh` загружать ревизию, проверять патчи через `git apply --check`, собирать с флагами из `pin` и `ZIG_GLOBAL_CACHE_DIR`
- [x] извлекать в указанный каталог `libghostty-internal.a` как `lib/libghostty.a`, `include/ghostty.h`, ресурсы, `terminfo` и `LICENSE`
- [x] проверить, что все зависимости `zig build` скачиваются, а каталог сборки содержит всё из раскладки набора; при отказе `deps.files.ghostty.org` записать препятствие ⚠️ и остановиться
- [x] ➕ итог первой сборки: путь к архиву и флаги из Context подтверждены. Зависимости, включая `deps.files.ghostty.org`, загружаются отдельным шагом `zig build --fetch`, сборка с ним занимает около 3,5 минут. Все 225 объектов архива имеют minos 13.0 и при `MACOSX_DEPLOYMENT_TARGET=14.0` в окружении; `build.sh` проверяет это, архитектуру arm64 и чтение архива `ar`. В `terminfo` кроме `78/xterm-ghostty` есть `67/ghostty`, в `shell-integration` есть скрытый `zsh/.zshenv`, ссылок в наборе нет: манифест задачи 3 учитывает скрытые файлы. Объектов и символов sentry-native в архиве нет, единственный член без minos — `__.SYMDEF`
- [x] ➕ исходники ревизии кэшируются в `~/.cache/shellio/ghostty-src.git` под ref `refs/shellio/pin/<ревизия>`, каждая сборка идёт в чистой копии из кэша. Загрузка в пустой кэш проверена с локальным источником вместо GitHub. `build.sh --print-flags` печатает флаги без сети для штампа задачи 3. `build.sh` отказывается собирать в непустой каталог
- [x] `make check` проходит

### Task 3: Публикация набора, штамп и блокировка

**Files:**
- Create: `scripts/ghostty/vendor.sh`
- Create: `scripts/ghostty/test-publish.sh`
- Create: `scripts/ghostty/lock.sh`
- Modify: `Makefile`

- [x] реализовать в `vendor.sh` команды `--print-stamp`, `--verify` и `publish` по разделу Technical Details; `--print-stamp` и `--verify` только читают и работают без сети
- [x] добавить обёртку блокировки, `.NOTPARALLEL`, цель `make ghostty` и `--verify` первым шагом `make check`
- [x] в `test-publish.sh` проверять на поддельном наборе в `SHELLIO_GHOSTTY_VENDOR`: изменённый или добавленный патч и изменённый `build.sh` дают другой штамп; удалённый, изменённый или лишний файл делает `--verify` неуспешным; прерванная после шага 2 публикация оставляет набор без штампа; `publish` без маркера отказывает; вторая обёртка при занятой блокировке завершается ошибкой
- [x] добавить цель `make test-scripts`
- [x] проверить на настоящем наборе: повторный `make ghostty` ничего не собирает, а после удаления файла набора восстанавливает его
- [x] `make check` и `make test-scripts` проходят
- [x] ➕ ключи штампа: значения `pin` под своими именами в нижнем регистре, `build_flags`, `patches`, `patches_sha256`, `build_sh_sha256`, `arch`, `developer_dir`, `xcodebuild_version`, `sdk_version`, `sdk_build`, `metal_version`; версию для Rust в задаче 4 даёт `ghostty_version_string`. Манифест — строки `SHA-256  путь` в порядке `LC_ALL=C`, скрытые файлы входят, ссылки и прочие не обычные файлы отвергаются
- [x] ➕ `build.sh` удаляет собранное дерево, поэтому шаг 1 публикации восстанавливает его `include/ghostty.h` из кэша `ghostty-src.git` с теми же патчами через `git apply --include`. Публикация отказывает, если корень набора накрывает репозиторий, `HOME`, `vendor/` или каталог сборки. Прерывание после шага 2 тест вызывает переменной `SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP`
- [x] ➕ блокировку берёт `scripts/ghostty/lock.sh`, его вызывают и `Makefile`, и тест. Рецепт цели X лежит во внутренней цели `locked-X` и выполняется вложенным `make` под одной блокировкой, маркер выставляется только в окружении команды. Код 75 занятой блокировки совпадает с `EX_TEMPFAIL` команды, поэтому обёртка различает их по отметке о старте команды. Файл блокировки — `<корень набора>.lock`, поэтому тест не трогает `vendor/ghostty.lock`
- [x] ➕ итог на настоящем наборе: 831 файл, 144 МБ. Первая публикация собрала набор за 287 с, повторный `make ghostty` только сверил набор, после удаления `share/terminfo/78/xterm-ghostty` набор пересобран за 237 с. `--print-stamp` занимает около 1 с, `--verify` около 6 с. Пока идёт `make ghostty`, `make check` в другом терминале сразу получает отказ «набор занят»; `make -j2 ghostty check` выполняет цели по очереди. `test-publish.sh` выполняет 54 проверки примерно за 45 с

### Task 4: Привязки и линковка `shellio-sys-ghostty`

**Files:**
- Create: `crates/shellio-sys-ghostty/Cargo.toml`
- Create: `crates/shellio-sys-ghostty/build.rs`
- Create: `crates/shellio-sys-ghostty/src/lib.rs`
- Create: `crates/shellio-sys-ghostty/src/bindings.rs`
- Create: `scripts/ghostty/link-libs`
- Create: `scripts/ghostty/link-audit.sh`
- Create: `scripts/ghostty/bindings.sh`
- Modify: `scripts/ghostty/tools.sh`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `Makefile`

- [x] в `build.rs` вызывать `vendor.sh --verify` и при расхождении завершать сборку с подсказкой `make ghostty`; линковку брать из `link-libs`; отдавать версию ghostty из штампа через `cargo:rustc-env`
- [x] перезапускать `build.rs` при изменении `build.sh`, `vendor.sh`, `pin`, `link-libs`, каталога патчей, `stamp`, `manifest`, `libghostty.a`, `ghostty.h` и переменных `DEVELOPER_DIR` и `SDKROOT`
- [x] генерировать `bindings.rs` закреплённым `bindgen-cli` с allowlist `ghostty_info` и его типов; явно задавать `LIBCLANG_PATH` из Xcode, target, sysroot и Rust edition; шапка без временных путей, с ревизией; добавить `bindgen-cli` в `make setup`
- [x] объявить модуль привязок с `#[rustfmt::skip]` и `#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]`; проверить, что `cargo fmt --check` и `clippy` его не отвергают
- [x] добавить цели `make bindings` и `make bindings-check`, обе с блокировкой и `--verify` первым шагом
- [x] в `link-audit.sh` линковать весь архив с `-force_load` без `-dead_strip` по `link-libs`; затем `otool -L` не показывает библиотек вне `/System` и `/usr/lib`; добавить цель `make link-audit` с блокировкой и `--verify`
- [x] проверить, что удаление `GameController` из `link-libs` роняет `make link-audit`
- [x] проверить отказ `build.rs` при устаревшем штампе
- [x] проверки раскладки от `bindgen` включены и проходят; `make check`, `make bindings-check` и `make link-audit` проходят
- [x] ➕ `bindgen-cli` 0.73.2 (BSD-3-Clause) — инструмент генерации, а не зависимость workspace. Его ставит `tools.sh bindgen` через `cargo install --locked` в `~/.cache/shellio/bindgen-cli/<версия>`, версия закреплена в `tools.sh`, а не в `pin`, чтобы не менять штамп набора. Генерацию и сверку ведёт `scripts/ghostty/bindings.sh` (без аргумента и `--check`): `LIBCLANG_PATH` от `xcrun --find clang`, SDK из `xcrun`, `rustfmt` закреплённого toolchain через `rustup which`, `--rust-target` из `rust-toolchain.toml`, edition из `Cargo.toml`, target `arm64-apple-macos` с минимумом из `.cargo/config.toml`. Перечисление остаётся константами, а не Rust enum
- [x] ➕ `build.rs` передаёт `vendor.sh` тот же корень набора, что линкует; относительный `SHELLIO_GHOSTTY_VENDOR` отвергается. Архив подключается как `static:-bundle`, поэтому 140 МБ не копируются в rlib каждого профиля: rlib весит 14 КБ, путь поиска доходит до итоговой линковки. Crate объявляет `links = "ghostty"` и отдаёт версию константой `GHOSTTY_VERSION`. Тест берёт `ghostty_info` указателем на функцию через `black_box` без вызова и `unsafe`, тестовый бинарь содержит `_ghostty_info` и зависит только от системных библиотек
- [x] ➕ итоги проверок: список `link-libs` из Context подтверждён без изменений, `link-audit` линкует весь архив около 3 с, автолинковки `LC_LINKER_OPTION` в архиве нет. Без `GameController` линковка падает на `_OBJC_CLASS_$_GCController` из `imgui_impl_osx.o`. Лишняя строка в патче даёт отказ прямого `cargo build -p shellio-sys-ghostty` с diff штампа и подсказкой `make ghostty`. Лишние пробелы в `bindings.rs` пропускают `cargo fmt --check` и не исправляются `cargo fmt`, а `make bindings-check` на них падает

### Task 5: Сведения о сборке в `shellio-ghostty`

**Files:**
- Create: `crates/shellio-ghostty/Cargo.toml`
- Create: `crates/shellio-ghostty/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [x] дать безопасную `info()` над `ghostty_info`: режим сборки и версия; версию читать по длине, а не до NUL, и проверять как UTF-8
- [x] записать в документации crate, что `ghostty_init` в P0.1 не вызывается и его контракт запуска определяет P0.3
- [x] написать тест: режим `ReleaseFast`, версия равна версии из штампа
- [x] `make check` проходит
- [x] ➕ `info()` возвращает `Result<Info, InfoError>`: свой `BuildMode`, версия `&'static str`, ошибки — неизвестный режим, нулевой указатель версии и не UTF-8. Типы Ghostty наружу не выходят. Время жизни версии опирается на comptime-строку `build_config.version_string` закреплённой ревизии (`src/main_c.zig`). Преобразование сырой структуры вынесено в отдельную функцию и проверено на собранных вручную структурах: все четыре режима, чтение `b"1.4.0junk"` с длиной 5 даёт `1.4.0`, отказы по каждой ошибке. Тест на слинкованном архиве вызывает `ghostty_info` и сравнивает версию с `GHOSTTY_VERSION` из штампа

### Task 6: Заглушка приложения и самопроверка

**Files:**
- Modify: `apps/shellio/Cargo.toml`
- Modify: `apps/shellio/src/main.rs`
- Create: `apps/shellio/src/self_check.rs`
- Create: `apps/shellio/tests/layout.rs`
- Create: `apps/shellio/Info.plist`
- Modify: `Cargo.lock`

- [x] подключить `shellio-ghostty`; в заглушке нет `unsafe`
- [x] находить ресурсы от пути исполняемого файла по разделу Technical Details
- [x] вызывать `info`, печатать одной строкой JSON: версию приложения, версию и режим ghostty, найденные пути; при ошибке завершаться с кодом 1 и понятной причиной
- [x] не читать и не писать каталог состояния
- [x] написать шаблон `Info.plist` по разделу Technical Details
- [x] написать юнит-тесты поиска ресурсов: правильная раскладка, нет `ghostty`, `terminfo` не рядом, нет `terminfo/78/xterm-ghostty`
- [x] в `tests/layout.rs` раскладывать бинарь и ресурсы набора во временную неподписанную раскладку `*.app` и запускать её с изоляцией из раздела Technical Details: код 0, версия из штампа, пути внутри раскладки, временные каталоги пусты
- [x] `make check` проходит
- [x] ➕ вывод — одна строка JSON с плоскими ключами `app_version`, `ghostty_version`, `ghostty_build_mode`, `ghostty_resources`, `terminfo`; тест проверяет, что её читает `plutil -extract <ключ> raw`, этим путём её будут разбирать скрипты задачи 8. Путь `current_exe` разрешается через `canonicalize`, как это делает ghostty (`std.process.executablePath` в Zig 0.17.1 зовёт realpath): ссылка на бинарь ведёт в бандл настоящего файла. Бинарь обязан лежать в `Contents/MacOS`, иначе код 1 с причиной; каждая ошибка раскладки — свой вариант. Тест раскладки запускает копию бинаря с `env_clear()` и шестью временными каталогами, все остаются пустыми; отдельный запуск без `terminfo` даёт код 1, пустой stdout и причину в stderr. В шаблоне `Info.plist` кроме обязательных ключей `CFBundleName`, `CFBundleInfoDictionaryVersion` и `CFBundleDevelopmentRegion`, версии добавит `bundle.sh`; тест сверяет `LSMinimumSystemVersion` с `.cargo/config.toml`. В `tests/layout.rs` разрешён `clippy::unwrap_used`: `allow-unwrap-in-tests` не покрывает вспомогательные функции интеграционного теста

### Task 7: Идентичность `Shellio Dev` (вместе с владельцем)

**Files:**
- Create: `scripts/signing/create-identity.sh`
- Create: `scripts/signing/README.md`
- Create: `scripts/signing/requirement.txt`

- [x] принимать путь к keychain параметром, по умолчанию login keychain; сначала прогнать скрипт на временном keychain и удалить его
- [x] до любых действий проверять, нет ли уже сертификата `Shellio Dev`, и при наличии отказываться создавать новый
- [x] создавать OpenSSL ключ RSA и самоподписанный сертификат: `keyUsage=digitalSignature`, `extendedKeyUsage=codeSigning`, `basicConstraints=CA:FALSE`, долгий срок
- [x] импортировать `.p12` с доступом для `/usr/bin/codesign`; partition list менять только у этого ключа
- [x] печатать SHA-1, записывать `requirement.txt`, экспортировать `.p12` только по пути вне репозитория, указанному владельцем
- [x] описать в README:
  - bundle ID и вид requirement;
  - два пароля: пароль `.p12` для импорта и экспорта и пароль keychain для разблокировки и partition list;
  - завершение настройки после частичного сбоя по уже импортированному ключу, без нового сертификата;
  - восстановление ключа из резервной копии;
  - доверие к сертификату для P0.1 не нужно, а `add-trusted-cert` требует подтверждения и в `.p12` не переносится
- [x] ⚠️ владелец запускает скрипт на login keychain сам (не выполнено, ждёт владельца: запуск на login keychain остаётся за ним, нужны пароль входа и путь резервной копии). Команда: `scripts/signing/create-identity.sh --export <путь вне репозитория>.p12`, затем коммит `scripts/signing/requirement.txt`. До этого `requirement.txt` в репозитории нет: запуск на временном keychain писал его во временный каталог
- [x] проверить: тестовый бинарь подписывается по SHA-1 и проходит `codesign --verify --strict` (на временных keychain; на login keychain это же делает самопроверка скрипта при запуске владельцем)
- [x] проверить, как в CI: экспортированный `.p12` импортируется во временный keychain через `security import`, и из него тестовый бинарь подписывается; временный keychain удаляется. Образ `xcode-27` переходит на OpenSSL 4, поэтому формат `.p12` проверяется именно импортом
- [x] ➕ у скрипта три режима: `--export <файл>` создаёт идентичность, `--from-p12 <файл>` восстанавливает её из резервной копии, `--finish` завершает настройку уже импортированного ключа без нового сертификата. `--requirement` задаёт другой файл requirement для тестов. Резервная копия записывается с правами `600` до импорта, поэтому ключ никогда не живёт только в keychain. Создание отказывает до любых действий, если `requirement.txt` уже есть; восстановление и `--finish` отказывают, если он описывает другой сертификат. Пароли берутся из `SHELLIO_DEV_P12_PASSWORD` и `SHELLIO_KEYCHAIN_PASSWORD`, без них пароль `.p12` запрашивает скрипт, а пароль keychain сам `security`
- [x] ➕ partition list: macOS даёт каждому импортированному из `.p12` ключу метку `Imported Private Key`, а `set-key-partition-list` ограничивает выбор только `-l`: `-a` не находит двоичный application label, `-D` и `-j` игнорируются, и команда меняет все ключи keychain. Скрипт отказывает, если ключ с такой меткой уже есть, после импорта находит ключ по SHA-1 открытого ключа, требует уникальности его метки и сверяет по выводу команды, что изменён только он. В login keychain владельца таких ключей сейчас нет
- [x] ➕ нужен OpenSSL 3 или новее: `/usr/bin/openssl` — LibreSSL 3.3.6, он шифрует `.p12` устаревшими алгоритмами. Алгоритмы заданы явно: ключ RSA 3072, сертификат на 20 лет, `.p12` с AES-256-CBC, PBKDF2 на 200 000 итераций и MAC SHA-256. Такой `.p12` от OpenSSL 4.1.0 импортирует `security` на macOS 27. `codesign --keychain` подписывает из keychain вне списка поиска, канонический requirement записывает SHA-1 в нижнем регистре
- [x] ➕ `scripts/signing/test-identity.sh` входит в `make test-scripts`: 22 проверки примерно за 6 с на временных keychain вне списка поиска, включая путь CI, прерывание после импорта (`SHELLIO_SIGNING_TEST_ABORT_AFTER_IMPORT`) с `--finish`, чужой ключ с той же меткой и ввод обоих паролей в терминале через `expect`. Тест проверяет, что список поиска и `requirement.txt` репозитория не изменились

### Task 8: Сборка, подпись и проверка бандла

**Files:**
- Create: `scripts/bundle.sh`
- Create: `scripts/check-bundle.sh`
- Create: `scripts/test-bundle.sh`
- Create: `scripts/signing/sign.sh`
- Modify: `scripts/signing/README.md`
- Modify: `Makefile`

⚠️ Подпись зависит от задачи 7: нужны идентичность `Shellio Dev`, которую владелец создаёт в login keychain, и закоммиченный `scripts/signing/requirement.txt`. Если их нет, задача останавливается: временную идентичность или ad-hoc подпись вместо них не подставлять, а пункты подписи и проверки не закрываются как пропущенные.

- [x] собирать `cargo build --release --locked -p shellio`, раскладывать `target/bundle/shellio.app` по разделу Technical Details, подставлять версии в `Info.plist` и проверять его через `plutil -lint`
- [x] перед раскладкой выполнять `--verify`
- [x] подписывать `codesign --force --sign <SHA-1> --identifier io.github.dsbasko.shellio --timestamp=none -r scripts/signing/requirement.txt`; без идентичности завершаться ошибкой, ad-hoc не подставлять
- [x] в `check-bundle.sh` проверять: `codesign --verify --strict`, подпись не ad-hoc, сертификат с SHA-1 из requirement, каноническое совпадение requirement, bundle ID, раскладку ресурсов, minos 27.0 у бинаря, `otool -L` бинаря без библиотек вне `/System` и `/usr/lib`
- [x] запускать самопроверку бандла с изоляцией из раздела Technical Details: код 0, версия ghostty из штампа, пути ресурсов внутри бандла, временные каталоги пусты
- [x] в `make run` запускать `open -W -n --env SHELLIO_STATE_DIR=<временный> --stdout <файл> --stderr <файл>` и проверять JSON так же
- [x] добавить цели `bundle`, `check-bundle` и `run` с блокировкой
- [x] проверить отказы `check-bundle.sh`: бандл без `terminfo`, подписанный настоящей идентичностью; бандл, переподписанный ad-hoc; файл, изменённый после подписи
- [x] проверить: две сборки с `cargo clean` между ними дают одинаковый requirement и обе проходят `make check-bundle`
- [x] `make check` проходит
- [x] ➕ подпись вынесена в `scripts/signing/sign.sh <путь>`: SHA-1 из `requirement.txt`, идентичность ищется через `security find-identity -p codesigning` без `-v`, потому что недоверенный сертификат в списке действительных не бывает. `SHELLIO_SIGNING_KEYCHAIN` задаёт keychain для `find-identity` и `codesign --keychain`, им задача 9 подписывает из временного keychain CI. `bundle.sh` собирает бандл в `target/bundle/.staging.*`, подписывает его там и только потом заменяет `target/bundle/shellio.app`; каталог сборки берётся из `cargo metadata`, версия — из `[workspace.package]` в `CFBundleShortVersionString` и `CFBundleVersion`. Ресурсы копируются `ditto` без расширенных атрибутов, скрытый `.zshenv` сохраняется
- [x] ➕ `check-bundle.sh [--open] [<бандл>]`: сертификат сверяется по SHA-1 извлечённого листа и `codesign --verify -R`, ресурсы `ghostty/`, `terminfo/` и `licenses/ghostty.txt` — с манифестом набора по хэшам, без лишних и недостающих файлов; `Contents` и `Contents/MacOS` не содержат ничего сверх раскладки, бинарь только arm64. `open -W` возвращает 0 и при коде 1 приложения, а заглушка выходит раньше, чем он начинает ждать («Unable to block on application»), поэтому `make run` судит по выводу: одна строка JSON, пустой stderr и пустой `SHELLIO_STATE_DIR`
- [x] ➕ отказы проверяет `scripts/test-bundle.sh` в цели `make test-bundle` с блокировкой: 18 проверок примерно за 7 с на копиях бандла, переподписанных настоящей идентичностью. Причина каждого отказа своя: без `terminfo` — раскладка, ad-hoc — проверка подписанта (`codesign --verify --strict` ad-hoc подпись пропускает), изменённые после подписи ресурс и бинарь — `codesign --verify --strict`, лишний и изменённый ресурс — манифест, чужой bundle ID — `Info.plist`, `sign.sh` с пустым keychain отказывает и оставляет прежнюю подпись. Тесту нужна идентичность, поэтому в `make test-scripts` он не входит; исходный бандл и список поиска keychain он не меняет
- [x] ➕ итоги: `make bundle` после сборки занимает около 5 с, `make check-bundle` около 2 с, `make run` около 1 с. После `cargo clean` вторая сборка дала тот же requirement и тот же CDHash, обе прошли `make check-bundle`. Подпись из login keychain идёт без запросов Keychain

### Task 9: CI на GitHub Actions

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `.github/actionlint.yaml`
- ➕ Modify: `scripts/signing/README.md`

- [x] `runs-on: xcode-27`, `DEVELOPER_DIR=/Applications/Xcode_27.app/Contents/Developer`, в логе `xcodebuild -version`
- [x] до `make setup` восстанавливать кэш архива Zig по версии и SHA-256 и кэш `ZIG_GLOBAL_CACHE_DIR` по `GHOSTTY_REV` и `ZIG_VERSION` с `restore-keys`; после `make setup` считать ключ кэша `vendor/ghostty/` по SHA-256 `--print-stamp`; кэш только ускоряет, `--verify` обязателен
- [x] выполнять `make ghostty`, `make check`, `make test-scripts`, `make bindings-check` и `make link-audit`; команды Cargo внутри целей идут с `--locked`
- [x] импортировать идентичность из секретов во временный keychain, выполнять `make bundle` и `make check-bundle`, удалять keychain в шаге `always()`; без секретов пропускать сборку бандла с явной пометкой
- [x] объявить метку `xcode-27` в `.github/actionlint.yaml` и проверить workflow через `actionlint`, поставленный на время проверки
- [x] записать в плане, что задача проверена только статически: первый прогон CI будет после появления репозитория на GitHub
- [x] `make check` проходит
- [x] ➕ один job, `DEVELOPER_DIR` задан на уровне job: путь Xcode входит в штамп. `actions/checkout` v7.0.1 и `actions/cache` v6.1.0 закреплены по SHA коммита. Кэши разнесены на `actions/cache/restore` и `actions/cache/save`, потому что общий `actions/cache` сохраняет только при успехе всего job (`post-if: success()`). Ключи: `zig-archive-<ZIG_VERSION>-<ZIG_SHA256>` для одного файла архива, а не распакованного Zig, поэтому сумма сверяется при каждом прогоне; `zig-global-<ZIG_VERSION>-<GHOSTTY_REV>` с `restore-keys` `zig-global-<ZIG_VERSION>-`; `ghostty-vendor-<SHA-256 штампа>` без `restore-keys`. Набор сохраняется сразу после успешного `make ghostty`, кэш пакетов Zig — только при промахе набора, когда сборка действительно шла. Ключ набора на этой машине совпадает с SHA-256 опубликованного `vendor/ghostty/stamp`. В целях `--locked` есть у `clippy`, `test`, `cargo build` и `cargo metadata` в `bundle.sh` и `cargo install` в `tools.sh`; `cargo fmt` зависимостей не разрешает
- [x] ➕ наличие секретов проверяет отдельный шаг: в `if:` секреты недоступны. Без обоих секретов шаги бандла пропускаются с `::notice` и строкой в итогах прогона, при одном заданном прогон падает. Секреты получают только этот шаг и шаг импорта. `SHELLIO_DEV_P12` хранится в base64, формат описан в `scripts/signing/README.md`. Импорт повторяет путь CI из `test-identity.sh` и проверен локально на одноразовом `.p12`: keychain в `RUNNER_TEMP` со случайным паролем, без тайм-аута, вне списка поиска; подпись из него проходит `codesign --verify --strict`, очистка удаляет keychain и не падает, если его нет. actionlint 1.7.12 и shellcheck 0.11.0 ставились во временный каталог по SHA-256 релизов и удалены; без `.github/actionlint.yaml` actionlint отвергает метку `xcode-27`
- [x] ⚠️ задача проверена только статически, первый прогон CI будет после появления репозитория на GitHub. Не проверены на раннере: метка `xcode-27` и путь `/Applications/Xcode_27.app`, установленный `rustup`, загрузка Metal toolchain через `xcodebuild -downloadComponent` без `sudo`, OpenSSL 3 или новее первым в `PATH` для `test-identity.sh` (шаг Xcode печатает его версию), время сборки набора и размеры кэшей

### Task 10: Проверка приёмочных условий

- [ ] в копии рабочего дерева без `target/` и `vendor/` проходят `make setup ghostty check test-scripts bindings-check link-audit bundle check-bundle run`
- [ ] повторный `make ghostty` ничего не собирает, изменённый патч блокирует линковку
- [ ] две чистые сборки дают одинаковый requirement, подпись не ad-hoc
- [ ] `make run` запускает бандл через `open` с изолированным `SHELLIO_STATE_DIR`, самопроверка успешна

### Task 11: [Final] Документы

- [ ] добавить в `AGENTS.md` раздел о сборке: цели `Makefile`, правило «не линковать набор с другим штампом», последовательный доступ к `vendor/ghostty/`
- [ ] добавить в дерево `docs/architecture.md` каталоги `scripts/ghostty/`, `scripts/signing/` и несохраняемый `vendor/`
- [ ] перенести план в `docs/plans/completed/`

## Post-Completion

Владелец:

- хранит `.p12` и пароль вне репозитория и вне этой машины: потеря ключа означает ручную переустановку (ADR-0019);
- создаёт репозиторий на GitHub и добавляет секреты `SHELLIO_DEV_P12` (`.p12` в base64) и `SHELLIO_DEV_P12_PASSWORD`, команды в `scripts/signing/README.md`;
- проверяет первый прогон CI.

Ручная приёмка:

- `shellio.app`, открытый из Finder, не показывает сообщения о повреждении или блокировке. Окна нет: заглушка выполняет самопроверку и завершается;
- повторная сборка и подпись не вызывают новых запросов Keychain на доступ `codesign` к ключу.
