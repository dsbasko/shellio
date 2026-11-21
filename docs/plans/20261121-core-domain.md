# Ядро: домен, команды, актор и хранилище

## Overview

- Шаг P1.1 `core-domain` [дорожной карты](../roadmap.md), первый план фазы 1. От него зависят P1.2 (блокировка, сокет, `shellioctl`, журнал событий) и через него все остальные шаги фазы.
- Заводит два чистых crate из [docs/architecture.md](../architecture.md): `shellio-core` (идентификаторы, модель, команды, события, актор, транспортная граница) и `shellio-store` (изолированный профиль, SQLite, миграции, реализация хранения для актора).
- Решения: модель процессов и ядро за транспортной границей — [ADR-0004](../adr/0004-process-model.md), словарь — [ADR-0001](../adr/0001-vocabulary.md), команды и события — [ADR-0007](../adr/0007-control-api.md), хранение — [ADR-0018](../adr/0018-storage-and-config.md), актор — [ADR-0020](../adr/0020-concurrency.md), воркспейсы и таблицы — [ADR-0025](../adr/0025-workspaces-and-project-catalog.md), тесты — [ADR-0021](../adr/0021-testing.md).
- Готово, когда модель и восстановление покрыты тестами:
  - каждая команда имеет обработчик, чтение результата через `query.tree` и тесты успеха и отказа;
  - состояние после повторного открытия базы равно состоянию до закрытия, в том числе на случайных последовательностях команд;
  - миграция идёт с резервной копией, схема новее бинаря отвергается без записи, сбой миграции оставляет базу на последней исправной версии;
  - повреждённая запись пропускается с диагностикой и не мешает загрузке остальных;
  - отказ записи не меняет ни состояние, ни ревизию;
  - сценарий через JSON-петлю транспортной границы даёт те же ответы и события, что и прямой вызов;
  - тесты не касаются настоящего каталога состояния.
- UI-результата нет, поэтому приёмочного прохода исполнителя и `make e2e` в плане нет.

## Context (from discovery)

- В workspace продуктовые crate только `apps/shellio` (заглушка самопроверки P0.1), `crates/shellio-ghostty` и `crates/shellio-sys-ghostty`. Каталогов `plugins/`, `crates/shellio-core`, `crates/shellio-store` нет.
- `Cargo.lock` уже содержит `rusqlite 0.43.0` и `libsqlite3-sys 0.40.0` (проба P0.7, без `bundled`, функция `fallible_uint`), `tokio 1.54.1` (проба P0.8: `rt-multi-thread`, `sync`, `time`), `serde 1.0.229`, `serde_json 1.0.154`, `tempfile 3.27.0`, `getrandom 0.4.3`. Крейта `uuid` нет.
- `clippy.toml`: `allow-unwrap-in-tests` и `allow-expect-in-tests`. В workspace `unwrap_used` и `expect_used` — `deny`, edition 2024, `rust-version = "1.99.1"`.
- В edition 2024 `std::env::set_var` — `unsafe`, а `unsafe` вне `shellio-appkit` и `shellio-ghostty` запрещён. Поэтому профиль получает окружение параметром, тесты окружение процесса не меняют.
- `make check` = `vendor.sh --verify`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` под блокировкой `vendor/ghostty.lock`. Новые члены workspace попадают в рубеж сами.
- ADR-0018 и ADR-0020 имеют статус `Proposed` и в реестре ADR подтверждаются в P1.1. ADR-0025 задаёт таблицы `workspaces`, `projects`, `project_roots` и ключ корня: общий git-каталог у репозитория, канонический путь у папки.
- Допустимые лицензии зависимостей — MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, их сочетания через `OR` или `AND` (как в планах P0.6–P0.8, [ADR-0022](../adr/0022-licensing-and-borrowing.md)).
- Передачи проб фазы 0 (отчёты P0.2–P0.8 просмотрены при планировании), адресованные ядру и хранилищу, отсутствуют. Ближайшие: пороги геометрии зон из P0.6 — P1.4, PID и время старта нагрузки — P2.1, разбор незавершённого обновления до публикации сокета — P1.2, журнал черновиков — P4.4.

## Решения планирования

Ответы владельца при планировании 2026-11-21:

- **Идентификаторы** — UUIDv7, newtype на каждую сущность. В CLI, JSON и переменных окружения — каноническая строка в нижнем регистре с дефисами, в SQLite — `BLOB` из 16 байт.
- **Ревизия и события** — ревизия хранится в `state.db` и растёт в той же транзакции, что и изменение, поэтому монотонна через перезапуски. События живут в кольцевом буфере в памяти. Курсор старше буфера или из прошлого запуска получает снимок и ревизию.
- **Сущности с таблицами** — `workspaces`, `projects`, `project_roots` по ADR-0025, `workbenches` и состояние окна (активный воркспейс и маршрут). `PanelId`, `SessionId`, `AgentId` заводятся только типами: таблицы панелей — P1.4, сессий — P2.1, агентов — P2.5.
- **Команды** — минимальный набор: `workspace.create`, `workspace.rename`, `project.add`, `workbench.create`, `workbench.rename`, `workbench.open`, `window.focus_workspace` и чтения `query.tree`, `query.status`. Показ и скрытие, порядок, перенос, «убрать», «вернуть», «забыть», удаление воркспейса с отменой и блокерами — P1.7.

## Предусловия

Физических предусловий нет: план не собирает бандл, не подписывает и не ходит в сеть, кроме загрузки новых crate в `Cargo.lock`. Для рубежа нужен опубликованный набор ghostty: при планировании 2026-11-21 `scripts/ghostty/vendor.sh --verify` в этой копии прошёл, иначе перед исполнением — `make ghostty`. Для интеграционных тестов корней нужен `git` в `PATH`.

## Development Approach

- **testing approach**: Regular. Код и его тесты входят в одну задачу.
- Каждая задача завершается до начала следующей и не опирается на код более поздней. Рубеж каждой задачи — `make check`. Для итераций внутри задачи допустим `cargo test -p <crate>`.
- Граница плана:
  - нет блокировки единственного экземпляра, сокета, `hello`, `shellioctl`, журнала событий с курсором по сокету — P1.2;
  - нет раскладки, зон и записей панелей — P1.4, плагинов и кардинальности — P1.5;
  - нет `settings.toml`, `keymap.toml` и горячей перезагрузки — P1.6 и P1.8;
  - нет UI и интеграции в `apps/shellio`. Заглушка P0.1 не меняется. P1.2 соединяет порядок запуска: блокировка, затем хранилище, затем ядро.
- Правило AGENTS.md о законченном действии (команда, обработчик, CLI, чтение результата, тесты) в P1.1 выполняется частично: команда, обработчик, чтение и тесты. CLI каждой команды этого плана — обязательная часть P1.2, имя в keymap и UI — P1.7. Передача записывается в ADR-0007 задачей 1.
- `unsafe` в обоих crate запрещён. Ни один из них не зависит от AppKit, objc2 или `shellio-ghostty`.
- Обработчики команд чистые: без файлового ввода-вывода, времени и случайности. Время и идентификаторы приходят через внедряемые `Clock` и `IdGen`, ввод-вывод корня — через подготовку вне актора (задача 6).
- Тесты:
  - каталог состояния — только `tempfile::TempDir` через `Profile::isolated`. Окружение процесса не читается и не меняется;
  - синхронизация — квитанции и каналы, `sleep` и ожидание по времени не используются;
  - в сеть не ходят. Настоящий `git` — только локальные `git init` и `git worktree add` во временном каталоге с `GIT_CONFIG_GLOBAL=/dev/null` и `GIT_CONFIG_NOSYSTEM=1`, переданными самой команде.
- Тестовая поддержка (`MemoryPersist`, `FaultyPersist`, JSON-петля, крючок сбоя `SqlitePersist`) стоит за функцией `test-support` своего crate. `make check` функций не передаёт, поэтому каждый crate подключает её себе сам: `shellio-core` — dev-зависимостью на себя (`shellio-core = { path = ".", features = ["test-support"] }`), `shellio-store` — так же на себя и на `shellio-core` с `test-support`. `required-features` у тестов запрещён: тест с ним в `make check` молча не собирается.
- Исполнитель не останавливается ради решения или приёмки владельца ([ADR-0036](../adr/0036-autonomous-execution.md)). Предвидимый выбор делается по разделу «Правила выбора». Непредвиденный — самим исполнителем: вариант, ближайший к принятым ADR и объёму плана, обратимый и самый дешёвый, с записью ⚠️ в плане. Решение ADR исполнитель не меняет: противоречие записывается отрицательным выводом с передачей.
- План коммитится вместе с изменениями каждой задачи (AGENTS.md).

## Testing Strategy

- **unit tests**: в каждой задаче с кодом — модель, валидация, обработчики, `State::apply`, формат JSON, профиль, миграции, загрузка и запись.
- **integration tests**: актор с поддельным и настоящим хранилищем, восстановление после закрытия, случайные последовательности, корни на настоящем `git`, JSON-петля транспортной границы.
- **e2e tests**: нет, окна нет. Каркас E2E появится в P1.3.

## Progress Tracking

- Выполненные пункты отмечаются `[x]` сразу.
- Новые задачи — с префиксом ➕, препятствия и решения исполнителя вне правил — с ⚠️.
- Решения по правилу плана записываются как «решение исполнителя по правилу плана» с основаниями.
- План обновляется при изменении объёма работ.

## Solution Overview

- **Направление зависимостей.** `shellio-core` не знает SQLite: он объявляет порт хранения `Persist` и содержит актор. `shellio-store` зависит от `shellio-core` и реализует `Persist` на SQLite. Тесты ядра идут на `MemoryPersist` и `FaultyPersist`, интеграционные тесты хранилища — на настоящем `state.db`. Соединяет их `apps/shellio` в P1.2.
- **Одна дельта на всё.** Обработчик команды по снимку состояния возвращает ответ и список `Change`. Тот же список коммитит хранилище, применяет к памяти `State::apply` и несёт событие. Главный поток в P1.3 держит зеркало состояния и применяет те же `Change`. Поэтому восстановление проверяется одним равенством: состояние актора = перезагрузка из базы = зеркало, собранное из снимка и событий.
- **Порядок обработки команды** в акторе:
  1. проверка и вычисление `Change` чистым обработчиком;
  2. пустой список — ответ без новой ревизии;
  3. иначе `commit(base_revision, changes)` на потоке хранения, актор ждёт квитанцию;
  4. успех — `State::apply`, ревизия +1, событие в буфер и подписчикам, ответ с ревизией;
  5. отказ — состояние и ревизия не меняются, ответ с ошибкой `storage_unavailable` и признаком повтора.

  Ответ приходит только после записи: структурные изменения пишутся немедленно (ADR-0018).
- **Поток хранения.** `Persist` блокирующий. Актор владеет отдельным потоком `std::thread` с экземпляром `Persist` и общается с ним каналом и квитанциями. Так блокирующий SQLite не занимает потоки `tokio`, а запись идёт строго по порядку.
- **Транспортная граница.** Всё, что проходит через `CoreHandle`, сериализуемо в JSON: `Command`, `Query`, `Reply`, `CoreError`, `Event`, `Snapshot`. В сообщениях нет `Arc`, каналов и ссылок. Стабильные имена команд и коды ошибок закреплены эталонными JSON. Тест JSON-петли прогоняет сценарий через обёртку, которая сериализует и разбирает каждый вызов, и сравнивает с прямым. Конверт запроса с номером, `hello` и версия протокола — P1.2.
- **Подписка без разрыва.** `subscribe` выполняется внутри актора и возвращает снимок с ревизией вместе с приёмником событий начиная со следующей ревизии. `events_since(cursor)` возвращает события после курсора или `Reset(Snapshot)`, если курсор вне буфера. Подписчик, отставший больше ёмкости канала, получает `Lagged` и запрашивает снимок.
- **Главный поток.** `CoreHandle::try_request` не ждёт: при полной очереди сразу возвращает `busy` с повтором, ответ приходит квитанцией. Для `project.add` подготовка корня идёт отдельной задачей на runtime, поэтому `try_request` сразу отдаёт квитанцию, а `busy` при полной очереди приходит через неё. `request` — асинхронный вариант для фоновых задач и будущего сервера сокета.
- **Профиль.** `Profile` вычисляет каталоги из переданного окружения. `SHELLIO_STATE_DIR` перенаправляет и состояние, и конфигурацию; неверное значение — ошибка, а не тихий переход на настоящий каталог.

## Technical Details

### Идентификаторы

- `WorkspaceId`, `ProjectId`, `ProjectRootId`, `WorkbenchId`, `PanelId`, `SessionId`, `AgentId` — newtype над `uuid::Uuid` с `Copy`, `Eq`, `Ord`, `Hash`, `Display`, `FromStr`, serde как строка.
- `FromStr` принимает только каноническую форму с дефисами (регистр любой), остальное — `invalid_argument`. Версия UUID при разборе не проверяется: внешние ID не обязаны быть v7.
- Создание — только через `IdGen` (`Uuid::now_v7()` в продукте, последовательный генератор в тестах). Никакая логика не опирается на порядок ID.
- В SQLite — `BLOB` длиной ровно 16. Другая длина при загрузке — повреждённая запись.

### Модель

```rust
pub struct State {
    pub workspaces: BTreeMap<WorkspaceId, Workspace>,
    pub projects: BTreeMap<ProjectId, Project>,
    pub roots: BTreeMap<ProjectRootId, ProjectRoot>,
    pub workbenches: BTreeMap<WorkbenchId, Workbench>,
    pub window: WindowState,
    pub diagnostics: Vec<Diagnostic>, // не хранится, заполняется загрузкой
}
pub struct Workspace { id, title: Title, position: u32, last_workbench: Option<WorkbenchId>,
    deleted_at: Option<UnixMillis> }
pub struct Project { id, workspace: WorkspaceId, title: Title, list_position: Option<u32>,
    removed_at: Option<UnixMillis>, last_opened_at: Option<UnixMillis> }
pub struct ProjectRoot { id, project: ProjectId, path: PathBuf, kind: RootKind, key: RootKey }
pub enum RootKind { Git, Folder }
pub struct Workbench { id, project: ProjectId, title: Title, position: u32 }
pub struct WindowState { active_workspace: Option<WorkspaceId>, route: Route } // None только до первого запуска
pub enum Route { WorkspaceHome, Workbench(WorkbenchId) }
```

- `deleted_at` и `removed_at` есть в схеме v1 и в модели по ADR-0025, но команды P1.1 их не ставят: удаление и «убрать» — P1.7. Загруженные записи с непустым значением остаются в `State`: `query.tree` их не показывает, имена среди них не занимают уникальность, а их корни остаются занятыми (`root_taken` называет такой проект).
- Порядок — по паре `(position, id)`. Новая запись получает позицию «наибольшая сохранённая в родителе + 1», поэтому пропуски после повреждённых записей ничего не перезаписывают. Совпадение позиций у загруженных записей — диагностика, а не пропуск строки. Уплотнение позиций — работа команд порядка P1.7.
- Инварианты (проверяются обработчиком и тестом `State::check_invariants` после каждой команды в тестах): ссылки владения (`workspace_id`, `project_id`) указывают на существующие записи; мягкие ссылки (`last_workbench`, маршрут) указывают на существующую область своего воркспейса; маршрут `Workbench` — на область активного воркспейса; `active_workspace` есть, если есть хоть один воркспейс; ключи корней уникальны; имена уникальны в своей области; позиции, выданные ядром, не совпадают с уже существующими в родителе.
- `Title`: обрезка пробелов по краям, не пусто, не длиннее 200 символов (`chars`), без управляющих символов. Уникальность — без учёта регистра по `to_lowercase()` после обрезки: воркспейсы среди всех, проекты внутри воркспейса, рабочие области внутри проекта.
- Ресурсоёмкость: `State` лежит в `Arc`, актор меняет его через `Arc::make_mut`; снимок — клон `Arc`.

### Команды и дельты

| Команда | Поля | Изменения | Ответ | Отказы |
| --- | --- | --- | --- | --- |
| `workspace.create` | `title` | новый воркспейс в конец | `{workspace}` | `invalid_argument`, `name_taken` |
| `workspace.rename` | `workspace`, `title` | имя | — | `not_found`, `invalid_argument`, `name_taken` |
| `project.add` | `workspace`, `path`, `title?` | проект в конец списка, один корень | `{project, root}` | `not_found`, `invalid_argument` (`path_not_absolute`, `path_not_found`, `not_a_directory`), `name_taken`, `root_taken {project, workspace}` |
| `workbench.create` | `project`, `title` | область в конец проекта | `{workbench}` | `not_found`, `invalid_argument`, `name_taken` |
| `workbench.rename` | `workbench`, `title` | имя | — | `not_found`, `invalid_argument`, `name_taken` |
| `workbench.open` | `workbench` | активный воркспейс — воркспейс проекта области, маршрут `Workbench`, `last_workbench` воркспейса, `last_opened_at` проекта | — | `not_found` |
| `window.focus_workspace` | `workspace` | активный воркспейс, маршрут — его `last_workbench` или `WorkspaceHome` | — | `not_found` |

- Имя проекта по умолчанию — последний компонент канонического пути корня.
- Команда, не меняющая состояние (переименование в то же имя, фокус уже активного воркспейса), даёт пустой список `Change` и ответ без новой ревизии.
- `Change` — закрытый перечень: `WorkspaceInserted`, `WorkspaceRenamed`, `WorkspaceLastWorkbenchSet`, `ProjectInserted { project, roots }`, `ProjectOpened`, `WorkbenchInserted`, `WorkbenchRenamed`, `WindowSet`. `State::apply` и перевод в SQL разбирают его исчерпывающим `match` без `_`.
- Запуск ядра выполняет внутренние команды до приёма внешних:
  - нет ни одного воркспейса — `bootstrap`: одна запись с двумя дельтами `WorkspaceInserted { title: "Основной" }` и `WindowSet { active, WorkspaceHome }`, ревизия 0 → 1;
  - воркспейсы есть, а `active_workspace` пуст (в том числе сброшен загрузкой как висячий) — `window.focus_workspace` на первый по `(position, id)` неудалённый, одна запись.

  Внутренние команды в `Command` не входят и в JSON не сериализуются, их дельты идут обычными событиями.

### Чтения

- `query.tree { scope }`, где `scope` — `Workspace(id)` или `All` (ADR-0025: внешний CLI без контекста сессии задаёт область явно). Ответ: ревизия, активный воркспейс, маршрут, воркспейсы по `position`, в каждом проекты (участники списка по `list_position`, затем вне списка по имени) с корнями и рабочими областями.
- `query.status`: ревизия, версия схемы, путь `state.db`, диагностика загрузки.
- Чтение не меняет ревизию и не проходит через поток хранения.

### Формат JSON

- `Command`: `{"cmd":"workspace.create","title":"Работа"}` — внутренний тег `cmd` с именами из таблицы, поля в `snake_case`.
- `Query`: `{"query":"tree","scope":{"workspace":"…"}}` или `{"query":"tree","scope":"all"}`.
- `Reply`: `{"revision":12,"value":{…}}`. `CoreError`: `{"code":"root_taken","message":"…","retryable":false,"details":{…}}`. Коды стабильны: `invalid_argument`, `not_found`, `name_taken`, `root_taken`, `storage_unavailable`, `storage_conflict`, `busy`, `shutting_down`.
- `Event`: `{"revision":12,"changes":[{"change":"workspace_renamed",…}]}`.
- Эталоны — `crates/shellio-core/tests/wire/*.json`, по одному на вариант каждого перечня. Тест сравнивает сериализацию с эталоном и разбор эталона с образцом. Эталон меняется только вместе с записью причины в плане.

### Актор и `CoreHandle`

- Очередь команд — ограниченная `tokio::sync::mpsc` ёмкостью 256. Кольцевой буфер событий — 1024 события. Канал подписчиков — `tokio::sync::broadcast` ёмкостью 256.
- `CoreHandle` (клонируемый, `Send + Sync`):
  - `async request(Command) -> Result<Reply, CoreError>`;
  - `try_request(Command) -> Result<oneshot::Receiver<…>, CoreError>` — без ожидания; для `project.add` подготовка запускается задачей на runtime, и любой отказ, включая `busy`, приходит через квитанцию;
  - `async query(Query) -> Result<Reply, CoreError>`;
  - `async subscribe() -> (Snapshot, broadcast::Receiver<Event>)`;
  - `async events_since(Revision) -> Result<EventsSince, CoreError>`, где `EventsSince` — `Events(Vec<Event>)` или `Reset(Snapshot)`;
  - `async shutdown()` — новые запросы получают `shutting_down`, принятые доводятся до ответа, поток хранения закрывается и присоединяется.
- `Core::start(runtime: &tokio::runtime::Handle, persist: Box<dyn Persist>, clock, ids, resolver: Arc<dyn RootResolver>) -> Result<CoreHandle, StartError>` загружает состояние на потоке хранения до запуска цикла и возвращает ошибку загрузки как есть.
- Отменённый вызывающий (брошенная квитанция) команду не отменяет: принятая команда атомарна и выполняется.
- Курсор больше текущей ревизии — `invalid_argument`.

### Корни проекта

- Порт `RootResolver::resolve(&Path) -> Result<ResolvedRoot, RootError>` блокирующий. `CoreHandle::request` для `project.add` выполняет его в `spawn_blocking` до отправки в актор и кладёт результат в подготовленную команду. Уникальность ключа проверяет актор, поэтому гонка двух добавлений даёт один `root_taken`.
- Реализация на файловой системе без запуска `git`:
  - путь абсолютный, иначе `path_not_absolute`; `canonicalize`; не каталог — `not_a_directory`;
  - `<путь>/.git` — каталог: `Git`, ключ — канонический путь этого каталога;
  - `<путь>/.git` — файл `gitdir: <x>`: каталог worktree; если в `<x>` есть файл `commondir`, ключ — канонический `<x>/<commondir>`, иначе канонический `<x>`;
  - иначе `Folder`, ключ — канонический путь папки.

  Подкаталог репозитория — обычная папка: поиск `.git` вверх не выполняется, так проект совпадает с тем, что указал пользователь.
- Две связанные worktree одного репозитория дают один ключ, поэтому вторая получает `root_taken`. Это прямое следствие ADR-0025 (ключ — общий git-каталог), не дефект.

### Профиль

- `Profile::resolve(env: impl Fn(&str) -> Option<OsString>) -> Result<Profile, ProfileError>`; `Profile::from_process_env()` для приложения; `Profile::isolated(dir)` для тестов.
- Без `SHELLIO_STATE_DIR`: состояние — `$HOME/Library/Application Support/shellio`, конфигурация — `$HOME/.config/shellio` (ADR-0018 буквально, `XDG_CONFIG_HOME` не читается). Нет или пустой `HOME` — ошибка.
- С `SHELLIO_STATE_DIR=<d>`: состояние — `<d>`, конфигурация — `<d>/config`. Пустое или относительное значение — ошибка.
- `state_db()` = `<state>/state.db`, `backups_dir()` = `<state>/backups`. Каталог состояния создаётся с правами `0700` при открытии хранилища, каталог конфигурации в P1.1 не создаётся. Блокировку, сокет и `runtime/<version>/` добавляют P1.2, P2.1 и P5.2.

### `state.db`

- Открытие: `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=5000`. Хранилище открывается одним процессом: в продукте после блокировки P1.2.
- Схема v1 (`crates/shellio-store/migrations/0001_init.sql`):

  ```sql
  CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) STRICT; -- 'revision'
  CREATE TABLE workspaces (id BLOB PRIMARY KEY, title TEXT NOT NULL, position INTEGER NOT NULL,
    last_workbench_id BLOB, deleted_at INTEGER) STRICT;
  CREATE TABLE projects (id BLOB PRIMARY KEY, workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    title TEXT NOT NULL, list_position INTEGER, removed_at INTEGER, last_opened_at INTEGER) STRICT;
  CREATE TABLE project_roots (id BLOB PRIMARY KEY, project_id BLOB NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL UNIQUE) STRICT;
  CREATE TABLE workbenches (id BLOB PRIMARY KEY, project_id BLOB NOT NULL REFERENCES projects(id),
    title TEXT NOT NULL, position INTEGER NOT NULL) STRICT;
  CREATE TABLE window_state (id INTEGER PRIMARY KEY CHECK (id = 1),
    active_workspace_id BLOB, route_workbench_id BLOB) STRICT;
  ```

  `path` и `kind` у `project_roots` дополняют ADR-0025: корень нужно открыть, а ключ у worktree не путь папки. Ограничения `CHECK` на значения перечней не ставятся сознательно: неверное значение должно загрузиться как повреждённая запись, а не уронить вставку при внешней правке.
- Миграция 0001 заполняет `meta` строкой `('revision', 0)` и `window_state` строкой `(1, NULL, NULL)`.
- Миграции — список `(version, sql)` из `include_str!`, версия — `PRAGMA user_version`. Порядок открытия:
  1. открыть файл; ошибка SQLite «не база» или «повреждена» — `StoreError::Corrupt { path }`, файл не трогается (путь восстановления — P5.6);
  2. `user_version` больше последней известной — `StoreError::NewerSchema { found, supported }`, ни одной записи;
  3. версия 0 и пустая схема — миграции без резервной копии;
  4. версия ниже последней при непустой схеме — резервная копия через backup API SQLite в `<state>/backups/state.db.v<версия>.<unix_ms>.bak`, затем открытие копии и проверка её `user_version`; неудача копии — `StoreError::Backup`, миграция не начинается;
  5. каждая миграция — своя транзакция вместе с установкой `user_version`; сбой — откат, `StoreError::Migration { to }`, база остаётся на прежней версии.

  Резервные копии автоматически не удаляются, уборка и откат — P5.2.
- `commit(base, changes)` — одна транзакция: проверка `meta.revision == base` (иначе `storage_conflict`: в базу писал кто-то ещё), SQL для каждого `Change`, `meta.revision = base + 1`. Нарушение `UNIQUE` или внешнего ключа — `storage_conflict`, состояние актора не меняется.
- Загрузка построчная, каждая находка попадает в `diagnostics` как `{table, rowid, reason}`. Строки в базе не удаляются и не исправляются загрузкой.
  - Пропуск строки: неверная длина ID, пустое или слишком длинное имя, неизвестный `kind`, висячая ссылка владения (`workspace_id`, `project_id`). Зависимые записи пропущенной строки тоже пропускаются со своей диагностикой.
  - Сброс с диагностикой, строка остаётся: висячая мягкая ссылка (`last_workbench_id` → `None`, `route_workbench_id` → `WorkspaceHome`, `active_workspace_id` → `None`, после чего запуск ядра выполняет фокус по разделу «Команды и дельты»).
  - Только диагностика: совпадение позиций внутри родителя.

### Правила выбора

- **Новые зависимости.** `uuid` с функциями `v7` и `serde`, без `fast-rng` и `macro-diagnostics`. Если версия `uuid` требует другого `getrandom`, берётся она. Лицензия каждого нового пакета в `cargo metadata` проверяется по списку раздела «Context» и записывается в задачу 1; пакет вне списка — ⚠️ и замена ближайшим допустимым. `thiserror`, `anyhow`, `proptest` не добавляются: ошибки — ручные `enum` с `Display`, случайные последовательности — собственный генератор с фиксированными зёрнами.
- **SQLite.** `rusqlite 0.43` без `bundled`, системная `libsqlite3` из SDK macOS, как в P0.7, плюс функция `backup`. `STRICT` таблицы требуют SQLite 3.37+: версия из SDK macOS 27 это покрывает, тест задачи 8 проверяет `sqlite_version()` и падает с понятным сообщением.
- **`tokio`.** Функции `rt`, `rt-multi-thread`, `sync`, в `dev-dependencies` также `macros`. `time` не нужен.
- **`Cargo.lock`.** Обновляется одним `cargo update -p <новый пакет>` или сборкой без `--locked`, затем коммитится с задачей 1. Остальные версии не поднимаются.
- **Конфликт эталона JSON** с тем, что нужно P1.2, P1.2 решает новым вариантом, а не переименованием: имена и коды P1.1 считаются замороженными после задачи 4.
- **Найденное противоречие ADR-0025** в процессе (например, правило, которое минимальный набор команд не может выполнить) — запись в ADR-0025 «Уточнено в P1.1» как передача P1.7, без изменения решения.

## What Goes Where

- **Implementation Steps** (`[ ]`): код, тесты, ADR и документы в этом репозитории.
- **Post-Completion** (без чекбоксов): то, что делают следующие планы.

## Implementation Steps

### Task 1: ADR, каркас crate и зависимости

**Files:**
- Modify: `docs/adr/0001-vocabulary.md`, `docs/adr/0004-process-model.md`, `docs/adr/0007-control-api.md`, `docs/adr/0018-storage-and-config.md`, `docs/adr/0020-concurrency.md`, `docs/adr/0025-workspaces-and-project-catalog.md`
- Create: `crates/shellio-core/{Cargo.toml,src/lib.rs}`, `crates/shellio-store/{Cargo.toml,src/lib.rs}`
- Modify: `Cargo.toml`, `Cargo.lock`

- [x] раздел «Уточнено в P1.1» в ADR-0001: идентификаторы UUIDv7, newtype на сущность, строковая форма в CLI и окружении, `BLOB` 16 байт в базе; `PanelId`, `SessionId`, `AgentId` заведены типами
- [x] раздел «Уточнено в P1.1» в ADR-0004 и ADR-0020: транспортная граница (сериализуемые сообщения `CoreHandle`), актор с потоком хранения, ответ после записи, очередь 256 и `busy`, атомарная подписка, буфер событий 1024 и `Reset(Snapshot)`, чистые обработчики с `Clock` и `IdGen`
- [x] раздел «Уточнено в P1.1» в ADR-0007: имена команд и чтений, формат JSON, стабильные коды ошибок, монотонная ревизия через перезапуски; CLI этих команд — обязательная часть P1.2
- [x] раздел «Уточнено в P1.1» в ADR-0018 и ADR-0025: прагмы, `user_version`, порядок открытия и резервная копия, отказ на новой схеме, построчная загрузка с диагностикой, схема v1 с `path` и `kind` у корней, `workbenches`, `window_state`, `meta.revision`, раскладка профиля при `SHELLIO_STATE_DIR`, правило ключа корня без поиска вверх
- [x] crate `shellio-core` и `shellio-store` в workspace с `[lints] workspace = true`, зависимости по «Правилам выбора», `Cargo.lock` обновлён только новыми пакетами; лицензии новых пакетов проверены по `cargo metadata` и перечислены здесь
  - Зависимости: `shellio-core` — `uuid` (`v7`, `serde`, без `fast-rng` и `macro-diagnostics`), `serde` с `derive`, `serde_json`, `tokio` (`rt`, `rt-multi-thread`, `sync`), в `dev-dependencies` `tokio` с `macros`, `tempfile` и сам `shellio-core` с `test-support`. `shellio-store` — `shellio-core`, `rusqlite 0.43.0` без функций по умолчанию с `backup` и `fallible_uint` (как в P0.7, без `bundled`), в `dev-dependencies` `shellio-core` и сам `shellio-store` с `test-support`, `serde_json`, `tempfile`, `tokio` с `macros`. `shellio-core` от `rusqlite` не зависит.
  - `Cargo.lock` обновлён одной сборкой без `--locked`: версии прежних пакетов не изменились, `uuid 1.29.3` берёт уже закреплённый `getrandom 0.4.3`.
  - Новые пакеты и лицензии по `cargo metadata`: `uuid 1.29.3` — Apache-2.0 OR MIT; `tokio-macros 2.7.2` — MIT; `slab 0.4.12` — MIT; `futures-core 0.3.34`, `futures-task 0.3.34`, `futures-util 0.3.34`, `js-sys 0.3.107`, `wasm-bindgen 0.2.130`, `wasm-bindgen-macro 0.2.130`, `wasm-bindgen-macro-support 0.2.130`, `wasm-bindgen-shared 0.2.130`, `bumpalo 3.20.4`, `rustversion 1.0.23` — MIT OR Apache-2.0. Все в списке раздела «Context». Пакеты `wasm-bindgen`, `js-sys` и их зависимости — целевые для `wasm32` у `uuid` и на macOS не собираются.
- [x] тест-заглушка в каждом crate, чтобы `make check` собирал и тестировал оба
  - Заглушка проверяет на этапе компиляции (`const { assert!(cfg!(feature = "test-support")) }`), что dev-зависимость crate на себя включила `test-support` без флагов.
- [x] `make check` проходит

### Task 2: Идентификаторы и модель

**Files:**
- Create: `crates/shellio-core/src/{ids.rs,model.rs,title.rs,clock.rs,root.rs}`

- [x] newtype ID по разделу «Идентификаторы», `IdGen` (продуктовый и последовательный тестовый), `Clock` (системный и ручной тестовый), `UnixMillis`, `Revision`
  - Решение исполнителя по правилу плана: serde ID идёт через собственные `Display` и `FromStr`, а не через serde `uuid`: тот принимает формы без дефисов, в скобках и `urn:uuid:`. `FromStr` до `Uuid::try_parse` требует длину 36, это отсекает остальные формы.
  - Новый ID выдаёт `<Id>::generate(&mut dyn IdGen)`, хранилище восстанавливает ID через `from_bytes` и пишет `as_bytes`. `IdGen::next_uuid(&mut self)`, `Clock::now(&self)`: `IdGen: Send`, `Clock: Send + Sync`.
  - `SequentialIds` (1, 2, 3, … в младших битах), `ManualClock` (клоны делят показание) и `FakeRootResolver` стоят за `test-support` и доступны как `shellio_core::test_support::*`. `Revision` лежит в `model.rs`.
- [x] `Title` с нормализацией и сравнением по разделу «Модель»
  - ⚠️ Решение исполнителя: равенство `Title` точное, уникальность сравнивает `Title::folded()` (`to_lowercase()`), иначе переименование только регистром стало бы пустым изменением. Обрезка — `str::trim` (пробельные символы Unicode), затем проверка управляющих символов (`char::is_control`) и длины в `chars`. `Deserialize` проверяет значение.
- [x] `State` и вложенные типы, `State::check_invariants` со списком нарушений
  - ⚠️ Решение исполнителя: совпадение позиций внутри родителя `check_invariants` сообщает отдельным вариантом `Violation::PositionTaken`. Различить позиции, выданные ядром, и загруженные по одному снимку нельзя, поэтому тесты задач 9 и 10 над загруженными данными с совпадением допускают ровно этот вариант. Позиции считаются по всем записям, включая удалённые и убранные: новая позиция — «наибольшая сохранённая + 1».
  - Ссылки (владения и мягкие) сверяются с ключом карты, отдельный вариант `KeyMismatch` ловит запись, чей `id` не совпадает с ключом. Маршрут `Workbench` без активного воркспейса — `RouteWorkbenchForeign`. Имена удалённых воркспейсов и убранных проектов уникальность не занимают, корни убранных проектов в уникальности ключа участвуют.
  - Типы модели выводят serde (`Route` — `"workspace_home"` или `{"workbench": id}`), закрепляет формат задача 4.
- [x] типы корня `RootKind`, `RootKey`, `ResolvedRoot`, `RootError` и порт `RootResolver` (`src/root.rs`) с поддельной реализацией за `test-support`; файловая реализация — задача 6
  - `RootError` — три варианта с `reason()`: `path_not_absolute`, `path_not_found`, `not_a_directory`. Прочие ошибки ввода-вывода и пути не в UTF-8 (`path` и `key` в базе — `TEXT`) решает задача 6. `RootKey` — строка, `RootKind` в базе и JSON — `git` и `folder`. `FakeRootResolver`: относительный путь — `NotAbsolute`, зарегистрированный — свой результат, остальные — папка с ключом, равным пути.
- [x] тесты ID: разбор канонической строки в любом регистре, отказ некорректных строк, serde как строка, уникальность последовательного генератора
- [x] тесты `Title`: обрезка, пустое, 200 и 201 символ, управляющие символы, сравнение без регистра для кириллицы и латиницы
- [x] тесты `check_invariants`: каждое нарушение из раздела «Модель» ловится на собранном вручную состоянии
- [x] `make check` проходит
  - 40 тестов `shellio-core` в выводе `make check`.

### Task 3: Команды, дельты и обработчики

**Files:**
- Create: `crates/shellio-core/src/{command.rs,change.rs,handler.rs,error.rs}`

- [x] `Command`, `Change`, `CoreError` с кодами из раздела «Формат JSON»
  - `CoreError` — структура с закрытыми полями: код, сообщение, `retryable` из кода (повтор только у `storage_unavailable` и `busy`) и детали объектом JSON. Детали: `invalid_argument` — `field` и `reason`, у пути ещё `path`; `not_found` — `entity` и `id`; `name_taken` — `entity`, `name`, `existing`; `root_taken` — `project` и `workspace`.
  - ⚠️ Решение исполнителя: новые стабильные причины `invalid_argument` — `title_empty`, `title_too_long`, `title_control_char`, `title_required` (у канонического пути корня нет последнего компонента в UTF-8, имя по умолчанию не вычисляется) и `position_exhausted` (после наибольшей позиции родителя `u32::MAX` места нет, деталь `scope`).
  - Имена в `Command` — строки как есть: `Title` проверил бы имя уже при разборе, тогда обработчик не мог бы ответить `invalid_argument`, а JSON-петля разошлась бы с прямым вызовом. Значение ответа — `CommandValue`: `{workspace}`, `{project, root}`, `{workbench}` или `null`, формат закрепляет задача 4.
  - `Change` несёт записи целиком, применение ничего не вычисляет: `WindowSet { active, route }` с непустым `active`, `WorkspaceLastWorkbenchSet { workspace, workbench: Option }`, `ProjectOpened { project, at }`.
- [x] чистый `handle(&State, Command, &mut Ctx) -> Result<Outcome, CoreError>` для всех команд таблицы; `project.add` принимает только подготовленный корень (`ResolvedRoot`)
  - ⚠️ Решение исполнителя: `handle` принимает `PreparedCommand`, а не `Command`: у `project.add` в нём `ResolvedRoot` вместо пути. Ввод-вывод выполняет `Command::prepare(&dyn RootResolver)` вне актора (задача 6 вызывает его в `spawn_blocking`), ошибка разрешения — `invalid_argument` с причиной и путём; остальные команды проходят без вызова разрешения. `Ctx::new(&dyn Clock, &mut dyn IdGen)`, `Outcome { value, changes }`.
  - ⚠️ Решение исполнителя: удалённый воркспейс, убранный проект, проект удалённого воркспейса и рабочие области таких проектов для команд — `not_found`: `query.tree` их не показывает, и окно на них не переходит.
  - ⚠️ Решение исполнителя: `workbench.open` уже открытой области (окно и `last_workbench` совпадают) — пустое изменение. `ProjectOpened` идёт только вместе с другими дельтами, иначе повторное открытие давало бы ревизию ради одного времени.
  - `project.add` проверяет занятый корень до имени: повторное добавление пути даёт и то же имя по умолчанию, а причина отказа — корень. Новая позиция — «наибольшая по всем записям родителя, включая удалённые и убранные, + 1», у пустого родителя — 0.
- [x] `State::apply(&Change)` с исчерпывающим `match`; внутренние команды запуска по разделу «Команды и дельты»: `bootstrap` одной записью и фокус при пустом `active_workspace`
  - `State::apply` ошибку не возвращает: вставка заменяет запись с тем же `id`, изменение отсутствующей записи пропускается, расхождение зеркало увидит по ревизии.
  - ⚠️ Решение исполнителя: `startup(&State, &mut Ctx) -> Result<Option<Outcome>, CoreError>`. `bootstrap` выполняется, когда нет ни одного неудалённого воркспейса, а не только когда их нет вовсе; фокус — когда активный воркспейс пуст или удалён. Так инварианты держатся после запуска и при внешней правке, удалившей все воркспейсы. `bootstrap` — это `workspace.create` «Основной»: создание при пустом или удалённом активном воркспейсе само добавляет `WindowSet` на новый воркспейс с маршрутом `WorkspaceHome`.
  - Общие помощники `State::live_workspace`, `live_project`, `live_workbench`, `workspaces_in_order` и `workbenches_in_order` (порядок `(position, id)`) — для фокуса запуска и дерева задачи 4.
- [x] тесты успеха каждой команды: дельты, ответ, состояние после `apply`, инварианты
  - Тестовая обвязка после каждой успешной команды применяет дельты и требует, чтобы команда не добавила нарушений. Допустимы только совпадения позиций, заведённые тестом до команды, — так выглядят загруженные записи.
- [x] тесты отказов каждой команды по колонке «Отказы», включая `root_taken` с проектом и воркспейсом владельца и `name_taken` без учёта регистра
  - Причины пути (`path_not_absolute`, `path_not_found`, `not_a_directory`) проверены через `Command::prepare` с `FakeRootResolver`, на настоящей файловой системе — задача 6.
- [x] тесты позиций: новая запись после пропуска позиции получает «наибольшая + 1», совпадение позиций упорядочивается по `id`
- [x] тесты пустых изменений: переименование в то же имя, фокус активного воркспейса; тест `workbench.open` области другого воркспейса переключает активный воркспейс и маршрут
- [x] `make check` проходит
  - 79 тестов `shellio-core` в выводе `make check`.

### Task 4: Чтения и формат JSON

**Files:**
- Create: `crates/shellio-core/src/{query.rs,tree.rs,wire.rs}`, `crates/shellio-core/tests/wire.rs`, `crates/shellio-core/tests/wire/*.json`

- [x] `Query`, `Reply`, `TreeView`, `StatusView` по разделу «Чтения»; сортировка проектов в дереве
  - `read(&State, Revision, &StorageInfo, &Query) -> Result<Reply, CoreError>` — чистое чтение над снимком. `StorageInfo { schema_version, state_db }` сообщает хранилище: задача 5 получает его из `Persist`. `state_db` — обязательный путь, `MemoryPersist` задачи 5 сообщает условный путь.
  - Ревизия ответа лежит только в `Reply.revision`: `TreeView` и `StatusView` её не повторяют.
  - `TreeView { active_workspace, route, workspaces }`, у воркспейса — `id`, `title`, `last_workbench`, `projects`, у проекта — `id`, `title`, `list_position`, `last_opened_at`, `roots`, `workbenches`. Позиции воркспейсов и рабочих областей заданы порядком массива. Состояние окна одно и то же в обеих областях.
  - Порядок: воркспейсы по `(position, id)`, участники списка по `(list_position, id)`, проекты вне списка по `Title::folded()`, затем по точному имени и `id`, корни по `(path, id)`, рабочие области по `(position, id)`. Удалённые воркспейсы, убранные проекты и рабочие области таких проектов в дерево не входят.
  - ⚠️ Решение исполнителя: область `Workspace(id)` удалённого или несуществующего воркспейса — `not_found`, как у команд задачи 3. Ошибки чтений план не задавал.
- [x] serde-представление всех перечней по разделу «Формат JSON»
  - `Query` — внутренний тег `query` со значениями `tree` и `status`, `TreeScope` — `"all"` или `{"workspace": id}`. `Query::name()` даёт `query.tree` и `query.status`.
  - `Reply { revision, value }`, где `ReplyValue` без тега: `Tree`, `Status` или `Command(CommandValue)`. Варианты различаются набором полей, поэтому разбор однозначен; `CommandValue::None` — `null`. `Event { revision, changes }` заведён в `src/wire.rs` здесь, ради эталона; кольцевой буфер — задача 5.
  - ⚠️ Решение исполнителя: `deny_unknown_fields` стоит на всех типах транспортной границы, и входящих, и исходящих: `Command`, `Query`, `Change`, `CoreError`, `Reply`, `Event`, `CommandValue`, `TreeView` с вложенными типами, `StatusView`, а также записи модели и `State`. Без него разбор `ReplyValue` без тега был бы неоднозначным, а JSON-петля задачи 10 пропускала бы расхождения. Развитие формата — новый вариант (раздел «Правила выбора») и версия протокола `hello` P1.2.
  - ⚠️ Решение исполнителя: `Query::Status {}` — вариант с пустыми полями, а не единичный. Проверено: у единичного варианта внутренне помеченного перечня serde пропускает лишние поля даже с `deny_unknown_fields` (`{"query":"status","x":1}` разбирался). Ручная проверка не понадобилась.
- [x] эталоны JSON на каждый вариант `Command`, `Query`, `Change`, `CoreError`, а также на `Reply` дерева и `Event`
  - 33 эталона `tests/wire/<вид>.<вариант>.json`: 7 `command.*`, 3 `query.*` (`tree.workspace`, `tree.all`, `status`), 8 `change.*`, 8 `error.<код>`, 6 `reply.*` (`tree`, `status`, `workspace`, `project`, `workbench`, `none`) и `event`. Вместе они покрывают оба варианта `Route`, `RootKind` и `TreeScope` и все варианты `CommandValue`. Сообщения ошибок входят в эталоны.
- [x] тест эталонов: сериализация совпадает, разбор эталона даёт образец; тест отказа разбора неизвестной команды и лишнего поля (`deny_unknown_fields`; если serde не поддерживает его вместе с внутренним тегом, лишнее поле отвергает ручная проверка, решение — ⚠️)
  - Решение исполнителя по правилу плана: сериализация сравнивается с эталоном как значение `serde_json::Value`, а не как текст. В `Cargo.lock` у `serde_json` есть `indexmap`, поэтому порядок ключей деталей ошибки зависит от набора функций в сборке и расходится между `cargo test -p shellio-core` и `make check`.
  - Имя эталона задаёт исчерпывающий `match` без `_` по варианту, у ошибок — `ErrorCode::ALL`. Новый вариант не даёт собрать тест, пока ему не задано имя эталона. Файл без образца, образец без файла и повтор имени роняют тест.
  - `deny_unknown_fields` с внутренним тегом serde поддерживает. Отказы проверены по тексту причины: неизвестная команда, нет тега, лишнее поле команды, неизвестное чтение и область, лишнее поле `status`, `tree` и области, а также лишние поля `Reply`, значения ответа, `Event`, `Change` и `CoreError`.
- [x] тесты дерева: область одного воркспейса и `All`, порядок участников списка и проектов вне списка, пустой воркспейс
  - Также удалённый воркспейс и убранный проект скрыты, `not_found` для области, порядок корней и рабочих областей, совпадения позиций упорядочены по `id`. Записи вне списка, убранные и удалённые собраны вручную: команды P1.1 таких не создают.
- [x] `make check` проходит
  - 106 тестов `shellio-core` в выводе `make check`: 93 модульных и 13 из `tests/wire.rs`.

### Task 5: Актор и `CoreHandle`

**Files:**
- Create: `crates/shellio-core/src/{actor.rs,handle.rs,persist.rs,events.rs}`, `crates/shellio-core/tests/actor.rs`

- [ ] порт `Persist` (`load`, `commit`) и поток хранения с квитанциями; `MemoryPersist` и `FaultyPersist` (отказ записи по счётчику, отказ загрузки, барьер) за функцией `test-support` по разделу «Development Approach»; `cargo test -p shellio-core` без флагов собирает и запускает `tests/actor.rs`
- [ ] актор по разделу «Порядок обработки команды», ревизия, кольцевой буфер, `broadcast`
- [ ] `CoreHandle` по разделу «Актор и `CoreHandle`», включая `try_request`, `subscribe`, `events_since`, `shutdown`; `Core::start` с запуском на пустом состоянии
- [ ] тесты: ответ после квитанции записи; отказ записи не меняет состояние и ревизию, следующая команда проходит; пустое изменение не даёт ревизии и события
- [ ] тесты подписки: снимок и события без разрыва при командах, идущих параллельно с `subscribe`; `events_since` внутри буфера, на границе, старше буфера (`Reset`), больше текущей ревизии (`invalid_argument`); `Lagged` у отставшего подписчика
- [ ] тесты: `try_request` при полной очереди даёт `busy` (очередь заполняется, пока поток хранения держит `FaultyPersist` на барьере); брошенная квитанция не отменяет команду; после `shutdown` — `shutting_down`, принятые до него команды записаны
- [ ] `make check` проходит

### Task 6: Файловое разрешение корней

**Files:**
- Modify: `crates/shellio-core/src/root.rs`, `crates/shellio-core/src/handle.rs`
- Create: `crates/shellio-core/tests/roots.rs`

- [ ] файловая реализация `RootResolver` по разделу «Корни проекта»
- [ ] подготовка `project.add` в `CoreHandle::request` через `spawn_blocking` и в `try_request` задачей на runtime; ошибки разрешения возвращаются кодами `invalid_argument` с причиной; тест `try_request` для `project.add` при полной очереди — `busy` через квитанцию
- [ ] тесты на временных каталогах: относительный путь, отсутствующий путь, файл вместо каталога, папка, символическая ссылка на папку даёт тот же ключ, подкаталог репозитория — `Folder`
- [ ] тесты на настоящем `git`: обычный репозиторий, связанная worktree даёт ключ основного репозитория, `.git`-файл без `commondir`; второе добавление той же worktree или основного репозитория — `root_taken`
- [ ] тест гонки: два параллельных `project.add` одного корня дают один успех и один `root_taken`
- [ ] `make check` проходит

### Task 7: Профиль

**Files:**
- Create: `crates/shellio-store/src/profile.rs`

- [ ] `Profile::resolve`, `from_process_env`, `isolated`, `state_db`, `backups_dir` по разделу «Профиль»
- [ ] создание каталога состояния с `0700` при открытии хранилища (функция профиля, вызываемая из задачи 8)
- [ ] тесты таблицей окружений: без переменной, с абсолютной, с пустой, с относительной `SHELLIO_STATE_DIR`, без `HOME`, пустой `HOME`; `XDG_CONFIG_HOME` игнорируется
- [ ] тест: новый каталог получает `0700`, права уже существующего каталога не меняются; путь, занятый файлом, — `ProfileError::NotADirectory`
- [ ] `make check` проходит

### Task 8: Схема и миграции

**Files:**
- Create: `crates/shellio-store/migrations/0001_init.sql`, `crates/shellio-store/src/{open.rs,migrate.rs}`, `crates/shellio-store/tests/{migrate.rs,schema_v1.sql,fixtures/v1.sql}`

- [ ] открытие с прагмами и `Migrator` по разделу «`state.db`»; список миграций подменяется в тестах
- [ ] тест схемы: пустой файл после открытия совпадает с эталоном `schema_v1.sql` (`sqlite_schema` без `sqlite_%`), `user_version = 1`, прагмы установлены, `sqlite_version()` не ниже 3.37
- [ ] тесты на синтетической миграции v1→v2: резервная копия создана и открывается с `user_version = 1`, данные сохранены; сбой второй миграции — откат, база на v1, копия есть; неудача копии (каталог `backups` занят файлом) — миграция не начата
- [ ] тест: `user_version` больше известной — `NewerSchema`, байты файла не изменились; файл с мусором — `Corrupt`, файл не изменён
- [ ] фикстура `fixtures/v1.sql` с заполненными данными как образец для будущих миграций; тест загружает её и открывает без миграции
- [ ] `make check` проходит

### Task 9: Хранилище как `Persist`

**Files:**
- Create: `crates/shellio-store/src/{persist.rs,load.rs,write.rs}`, `crates/shellio-store/tests/load.rs`

- [ ] `SqlitePersist::load` с построчной загрузкой и диагностикой, `commit` с проверкой ревизии по разделу «`state.db`»
- [ ] перевод каждого `Change` в SQL исчерпывающим `match`
- [ ] тесты записи: каждая дельта, ревизия растёт на 1, неверная базовая ревизия — `storage_conflict` без изменений, нарушение `UNIQUE` ключа корня (запись, вставленная в обход ядра) — `storage_conflict`
- [ ] тесты повреждённых записей, вставленных прямым SQL: неверная длина ID, пустое имя, неизвестный `kind`, висячая ссылка владения — строка и зависимые пропущены; висячий `last_workbench_id` — воркспейс со всеми проектами загружен, указатель сброшен; совпадение позиций — обе строки загружены; диагностика перечисляет каждую находку, байты базы не изменились
- [ ] тест: после загрузки с совпадением позиций новая запись получает «наибольшая + 1», повторная загрузка не теряет ни одной строки
- [ ] тест: висячий `active_workspace_id` сбрасывается загрузкой, старт ядра выполняет фокус одной записью, диагностика есть; запись с непустым `removed_at` держит свой корень (`root_taken`) и не видна в `query.tree`
- [ ] `make check` проходит

### Task 10: Восстановление и транспортная граница

**Files:**
- Create: `crates/shellio-store/tests/{recovery.rs,loopback.rs}`, `crates/shellio-core/src/loopback.rs` (за `test-support`)

- [ ] тест повторного открытия: сценарий из всех команд, `shutdown`, новое ядро на той же базе — `State` (без `diagnostics`) и ревизия равны, `query.tree` совпадает побайтно в JSON
- [ ] тест случайных последовательностей: 200 зёрен по 50 команд, включая заведомо ошибочные; после каждой команды инварианты, в конце состояние актора = перезагрузка из базы = зеркало из снимка и событий подписчика
- [ ] тест отказа посреди транзакции: тестовый крючок `SqlitePersist` за функцией `test-support` прерывает `commit` после первого SQL-оператора дельты — ответ `storage_unavailable`, состояние и ревизия не изменились, в базе нет частичной записи, следующая команда проходит, перезагрузка совпадает с состоянием актора
- [ ] тест первого запуска: пустой профиль даёт `meta.revision = 0` после миграции, затем `bootstrap` — воркспейс «Основной», активный, маршрут `WorkspaceHome`, ревизия 1, одно событие с двумя дельтами; второй запуск его не дублирует и ревизию не меняет
- [ ] JSON-петля: обёртка над `CoreHandle`, сериализующая и разбирающая каждый запрос, ответ и событие; один сценарий через петлю и напрямую на двух временных базах с одинаковыми `IdGen` и `Clock` даёт одинаковые ответы, события и итоговое дерево
- [ ] `make check` проходит

### Task 11: Verify acceptance criteria

- [ ] каждый пункт «Готово, когда» раздела Overview сопоставлен с тестом, имена тестов записаны здесь
- [ ] `grep` по `crates/shellio-core` и `crates/shellio-store`: нет `unsafe`, `std::env::set_var`, `std::env::var` вне `Profile::from_process_env`, `sleep`, `objc2`, `shellio-ghostty`
- [ ] `cargo tree -p shellio-core` не содержит `rusqlite`, `cargo tree -p shellio-core -p shellio-store` не содержит AppKit-зависимостей
- [ ] тесты не трогают настоящий каталог: `HOME` в тестах не читается (проверка поиском), до и после `make check` отметка времени изменения `~/Library/Application Support/shellio` не меняется или каталога нет
- [ ] в выводе `make check` есть тесты из `tests/actor.rs`, `tests/recovery.rs`, `tests/loopback.rs` и тесты крючка сбоя: функция `test-support` подключена без флагов
- [ ] `make check` проходит, число тестов записано здесь

### Task 12: [Final] Документы

**Files:**
- Modify: `docs/adr/README.md`, `docs/adr/0018-storage-and-config.md`, `docs/adr/0020-concurrency.md`, `docs/architecture.md`, `AGENTS.md`, `docs/roadmap.md`

- [ ] разделы «Уточнено в P1.1» сверены с кодом: расхождения задач 2–10 перенесены туда
- [ ] ADR-0018 и ADR-0020 остаются `Proposed`: P1.1 подтверждает только часть решения. В «Уточнено в P1.1» — список подтверждённого (SQLite, WAL, миграции с копией, отказ на новой схеме, построчная загрузка, `SHELLIO_STATE_DIR`, актор, снимки, ревизия) и неподтверждённого с шагом, который его подтвердит: TOML и горячая перезагрузка — P1.6 и P1.8, `state_version` плагинов — P1.5, пакетная запись частых событий — P2.2, токены отмены длинных операций — P3.1, проверка времени кадра главного потока — P1.3
- [ ] реестр `docs/adr/README.md`: колонка «Подтверждается в» у 0018, 0020, 0025 — итог P1.1 и оставшиеся шаги
- [ ] `docs/architecture.md`: в «Поток данных» — ответ после записи, ревизия через перезапуски и `Reset(Snapshot)`, если формулировки расходятся с кодом
- [ ] `AGENTS.md`: правило «тесты получают каталог состояния только через `Profile::isolated`, профиль читает окружение только в `Profile::from_process_env`» в раздел «Качество»
- [ ] строка P1.1 в `docs/roadmap.md`: ⬜ → ✅; план перенесён в `docs/plans/completed/`, относительные ссылки поправлены, коммит вместе с отметкой
- [ ] `make check` проходит

## Post-Completion

**Передачи следующим планам:**
- P1.2: CLI для каждой команды и чтения P1.1 (`workspace new|rename`, `project add`, `workbench new|rename|open`, `workspace focus`, `tree`, `status`), конверт запроса с номером, `hello` с версией протокола, `events --since` поверх `events_since`, проверка области по ADR-0025, порядок запуска «блокировка → хранилище → ядро» в `apps/shellio`.
- P1.4: таблицы панелей и раскладки отдельной миграцией v2, `PanelId` в модели.
- P1.7: показ и скрытие, порядок, перенос, «убрать», «вернуть», «забыть», удаление воркспейса, имя первой рабочей области и имя проекта при совпадении по умолчанию.
- P5.2 и P5.6: уборка резервных копий и откат схемы, путь восстановления при `Corrupt` и `NewerSchema`.
