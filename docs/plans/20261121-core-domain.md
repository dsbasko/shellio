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

- [x] порт `Persist` (`load`, `commit`) и поток хранения с квитанциями; `MemoryPersist` и `FaultyPersist` (отказ записи по счётчику, отказ загрузки, барьер) за функцией `test-support` по разделу «Development Approach»; `cargo test -p shellio-core` без флагов собирает и запускает `tests/actor.rs`
  - `Persist: Send`: `load() -> Result<Loaded, LoadError>`, где `Loaded { state, revision, storage: StorageInfo }`, а `LoadError` — `Box<dyn Error + Send + Sync>`, чтобы `Core::start` вернул ошибку хранилища как есть; `commit(base, &[Change]) -> Result<(), CommitError>`, `CommitError` — `Unavailable` (`storage_unavailable`) или `Conflict` (`storage_conflict`, его даст задача 9).
  - Поток хранения `shellio-storage` (`std::thread::Builder`, отказ создания — `StartError::Spawn`) получает задания по `std::sync::mpsc` и отдаёт квитанцию актору через `tokio::sync::oneshot`, а запуску ядра — через `std::sync::mpsc`: `Core::start` синхронный и работает и внутри runtime, где `blocking_recv` паникует. Потерянная квитанция — `storage_unavailable`.
  - `MemoryPersist`: клоны делят данные, тест держит клон и видит записанное; `commit` с чужой базовой ревизией — `Conflict`; `query.status` сообщает `schema_version = 0` и условный путь `MemoryPersist::STATE_DB` (`:memory:`). `FaultyPersist::new(memory) -> (FaultyPersist, Faults)`: пульт `Faults` — `fail_load`, `fail_commit(n)` и `fail_next_commit`, `barrier(n)` и `barrier_next_commit` с `CommitBarrier::entered().await` и `release()` (брошенный барьер тоже отпускает), `commits()` и `is_dropped()`. Вызовы `commit` нумеруются с 1, запись запуска ядра тоже считается.
- [x] актор по разделу «Порядок обработки команды», ревизия, кольцевой буфер, `broadcast`
  - Актор — задача на переданном runtime, владеет `Arc<State>` (`Arc::make_mut`), ревизией, `EventLog` на `EVENT_BUFFER = 1024` событий и `broadcast` на `SUBSCRIBER_CAPACITY = 256`; очередь — `QUEUE_CAPACITY = 256`. Константы экспортированы для тестов. Запросы, включая чтения, подписку и `events_since`, идут через очередь строго по одному: так снимок, ревизия и приёмник событий согласованы. Чтения в поток хранения не ходят.
  - ⚠️ Решение исполнителя: внутренние команды запуска выполняет `Core::start` до запуска цикла актора, их запись ждёт квитанции, блокируя вызывающий поток. Отказ записи запуска или его обработчика — `StartError::Startup(CoreError)`, поток хранения закрывается и присоединяется. План этот отказ не задавал; без записи запуска инварианты (активный воркспейс есть) не держатся.
  - `Shutdown` в той же очереди закрывает её (`close`): стоящие в очереди запросы доводятся до ответа, затем канал событий закрывается, поток хранения закрывается и присоединяется через `spawn_blocking`, и только после этого `shutdown` завершается.
- [x] `CoreHandle` по разделу «Актор и `CoreHandle`», включая `try_request`, `subscribe`, `events_since`, `shutdown`; `Core::start` с запуском на пустом состоянии
  - `CoreHandle` хранит `runtime::Handle` и запускает задачи только через него: `try_request` вызывается с главного потока, где `tokio::spawn` паникует. Квитанция — `ReplyReceipt` (`oneshot::Receiver<Result<Reply, CoreError>>`).
  - Подготовка `project.add` реализована здесь, потому что её требует раздел «Актор и `CoreHandle`»: `request` выполняет `Command::prepare` в `spawn_blocking`, `try_request` — задачей на runtime, которая сразу отдаёт квитанцию, а любой отказ, включая `busy` и `shutting_down`, присылает через неё. Паника разрешения пробрасывается вызывающему. Файловое разрешение и тест `busy` через квитанцию для `project.add` остаются задаче 6.
  - ⚠️ Решение исполнителя: `subscribe` возвращает `Result<(Snapshot, broadcast::Receiver<Event>), CoreError>`: после `shutdown` ему нужен отказ `shutting_down`, а сигнатура плана его не выражала.
  - ⚠️ Решение исполнителя: `shutdown` — `fn shutdown(&self) -> impl Future<Output = ()> + Send + 'static`, признак завершения ставится в момент вызова, а не при первом опросе future: запросы после вызова получают `shutting_down` детерминированно. Future `request`, созданный до вызова, но не опрошенный, в очередь не попал и тоже получает отказ.
  - ⚠️ Решение исполнителя: курсор `events_since` больше текущей ревизии — `invalid_argument` с полем `cursor` и новой стабильной причиной `cursor_ahead`.
  - `Snapshot { revision, state }` и `EventsSince` (`{"events": […]}` или `{"reset": снимок}`) сериализуемы: состояние в снимке разделяется с актором через `Arc`, а в JSON идёт значением. `Snapshot` отвергает лишние поля. Добавлены эталоны `events_since.events` и `events_since.reset` (новые типы транспортной границы, по эталону на вариант), всего эталонов 35; тест отказов разбора дополнен лишним полем снимка и `events_since` и неизвестным вариантом.
- [x] тесты: ответ после квитанции записи; отказ записи не меняет состояние и ревизию, следующая команда проходит; пустое изменение не даёт ревизии и события
  - `reply_comes_after_commit_receipt`, `failed_commit_changes_nothing_and_next_command_passes`, `empty_change_gives_no_revision_and_no_event`, а также `handler_refusal_does_not_reach_storage`, `reads_do_not_reach_storage` и тесты запуска: `start_on_empty_state_bootstraps_one_workspace`, `start_on_saved_state_writes_nothing_and_resets_old_cursors`, `start_focuses_first_workspace_when_active_is_missing`, `load_failure_is_returned_as_is`, `startup_commit_failure_stops_start`.
- [x] тесты подписки: снимок и события без разрыва при командах, идущих параллельно с `subscribe`; `events_since` внутри буфера, на границе, старше буфера (`Reset`), больше текущей ревизии (`invalid_argument`); `Lagged` у отставшего подписчика
  - `subscribe_concurrent_with_commands_has_no_gap` (4 писателя по 50 команд и 20 подписок на многопоточном runtime: первое событие — ревизия снимка + 1, ревизии подряд, снимок с событиями равен итоговому состоянию), `events_since_covers_buffer_and_resets_beyond_it` (1034 записи: курсор `текущая − 1024` получает все 1024 события, на единицу меньше и 0 — `Reset`), `lagging_subscriber_gets_lagged` (257 событий — `Lagged(1)`). Пустой буфер после перезапуска — в `start_on_saved_state_writes_nothing_and_resets_old_cursors`, граница буфера ещё и модульными тестами `EventLog`.
- [x] тесты: `try_request` при полной очереди даёт `busy` (очередь заполняется, пока поток хранения держит `FaultyPersist` на барьере); брошенная квитанция не отменяет команду; после `shutdown` — `shutting_down`, принятые до него команды записаны
  - `try_request_on_full_queue_is_busy` (одна команда на барьере и 256 в очереди, 258-я — `busy`, после отпускания все 257 записаны по порядку), `dropped_receipt_does_not_cancel_command` (брошены квитанции команды на барьере записи и команды в очереди за ней), `shutdown_refuses_new_requests_and_finishes_accepted` (`try_request`, `request`, `query`, `subscribe`, `events_since` и `project.add` через квитанцию получают `shutting_down`; принятые записаны, хранилище освобождено, повторный `shutdown` завершается).
  - Мутации проверены вручную: сдвиг границы буфера роняет три теста, публикация при отказе записи — тест отказа записи.
- [x] `make check` проходит
  - 129 тестов `shellio-core` в выводе `make check`: 98 модульных, 17 из `tests/actor.rs` и 14 из `tests/wire.rs`.

### Task 6: Файловое разрешение корней

**Files:**
- Modify: `crates/shellio-core/src/root.rs`, `crates/shellio-core/src/handle.rs`
- Create: `crates/shellio-core/tests/roots.rs`

- [x] файловая реализация `RootResolver` по разделу «Корни проекта»
  - `FsRootResolver` — продуктовый тип без `test-support`, экспортирован из `shellio_core`. `git` не запускается, `.git` вверх не ищется. Относительный `gitdir: <x>` отсчитывается от каталога с файлом `.git`, относительный `commondir` — от `<x>`, как у git: у `.git` требуется префикс `gitdir: ` и обрезаются пробельные символы в конце, у `commondir` — только перевод строки в конце.
  - ⚠️ Решение исполнителя (задача 2 оставила прочие ошибки ввода-вывода и пути не в UTF-8 этой задаче): три новые стабильные причины `invalid_argument`. `path_unreadable` — нет прав, петля ссылок и прочие ошибки чтения пути или файлов `.git` и `commondir`. `path_not_utf8` — канонический путь, ключ или текст файла ссылки не в UTF-8: `path` и `key` в базе `TEXT`, `to_string_lossy` дал бы чужой ключ. `git_link_invalid` — файл `.git` без префикса `gitdir: ` или с пустой целью, цель `gitdir` или `commondir` не существует или не каталог, пустой `commondir`, `commondir` не обычный файл; деталь `path` называет сломанный файл ссылки.
  - ⚠️ Решение исполнителя: файл посреди пути (`<файл>/inner`) и висячая символическая ссылка — `path_not_found`, указанного пути нет. `.git`, который не каталог и не обычный файл (FIFO, сокет), и висячая ссылка `.git` — папка: ни каталога git, ни файла ссылки на него в корне нет. Файл ссылки читается только как обычный файл, поэтому FIFO вместо `commondir` разрешение не блокирует.
- [x] подготовка `project.add` в `CoreHandle::request` через `spawn_blocking` и в `try_request` задачей на runtime; ошибки разрешения возвращаются кодами `invalid_argument` с причиной; тест `try_request` для `project.add` при полной очереди — `busy` через квитанцию
  - Подготовка сделана в задаче 5 (там же записано основание), `handle.rs` в этой задаче не менялся. Тесты на файловом разрешении: `resolution_errors_come_back_as_invalid_argument` (`path_not_absolute`, `path_not_found`, `not_a_directory` и через `request`, и через квитанцию `try_request`, хранилище не вызывается) и `try_request_project_add_on_full_queue_is_busy_through_receipt` (одна команда на барьере и 256 в очереди, `try_request(project.add)` отдаёт квитанцию, через неё — `busy` с повтором; после отпускания проект не добавлен, повтор проходит).
- [x] тесты на временных каталогах: относительный путь, отсутствующий путь, файл вместо каталога, папка, символическая ссылка на папку даёт тот же ключ, подкаталог репозитория — `Folder`
  - `relative_path_is_not_absolute`, `missing_path_is_not_found` (также файл посреди пути и висячая ссылка), `file_is_not_a_directory` (также ссылка на файл), `folder_resolves_to_its_canonical_path` (также путь с `..` и `.`), `symlink_to_folder_gives_same_root` (также ссылка на ссылку), `repository_subdirectory_is_folder`. Сверх списка: `git_file_follows_relative_gitdir_and_commondir`, `broken_git_link_is_git_link_invalid` (семь видов сломанной ссылки), `unreadable_directory_is_path_unreadable`; модульный `path_outside_utf8_is_refused` — APFS не создаёт имя не в UTF-8, поэтому проверка идёт на `OsStr` из байтов. Сравнение — с каноническим путём временного каталога (`/var` → `/private/var`).
- [x] тесты на настоящем `git`: обычный репозиторий, связанная worktree даёт ключ основного репозитория, `.git`-файл без `commondir`; второе добавление той же worktree или основного репозитория — `root_taken`
  - `real_repository_is_keyed_by_its_git_dir`, `linked_worktree_is_keyed_by_main_repository` (`git worktree add --detach`), `git_file_without_commondir_is_keyed_by_its_gitdir` (`git init --separate-git-dir`), `second_add_of_worktree_or_main_repository_is_root_taken` (повтор worktree, основной репозиторий и он же в другом воркспейсе — `root_taken` с проектом и воркспейсом владельца, ревизия не меняется). `git` получает `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, имя и почту автора через `-c` и без `GIT_DIR`, `GIT_INDEX_FILE` и других переменных перенаправления, которые выставил бы хук git; без `git` в `PATH` тест падает, а не пропускается.
- [x] тест гонки: два параллельных `project.add` одного корня дают один успех и один `root_taken`
  - `parallel_adds_of_one_root_give_one_success_and_one_root_taken`: многопоточный runtime, 20 раундов, в каждом две задачи добавляют одну папку по прямому пути и по символической ссылке. Ровно один успех и один `root_taken`, детали которого называют победителя; в состоянии один корень раунда у проекта победителя.
- [x] `make check` проходит
  - 146 тестов `shellio-core` в выводе `make check`: 99 модульных, 17 из `tests/actor.rs`, 16 из `tests/roots.rs` и 14 из `tests/wire.rs`.

### Task 7: Профиль

**Files:**
- Create: `crates/shellio-store/src/profile.rs`

- [x] `Profile::resolve`, `from_process_env`, `isolated`, `state_db`, `backups_dir` по разделу «Профиль»
  - `Profile`, `ProfileError` и `STATE_DIR_VAR` экспортированы из `shellio_store`, геттеры `state_dir` и `config_dir`. `SHELLIO_STATE_DIR` проверяется первым: при верном значении `HOME` не читается, пустое или относительное — ошибка и при верном `HOME`. `from_process_env` — единственное чтение окружения в crate (`std::env::var_os`), тесты его не вызывают. `isolated(dir)` без `test-support`, раскладка как у `SHELLIO_STATE_DIR=<dir>`.
  - `ProfileError` — ручной `enum`: `HomeMissing`, `HomeEmpty`, `HomeNotAbsolute`, `StateDirEmpty`, `StateDirNotAbsolute`, `NotADirectory { path }`, `Io { path, source }`.
  - ⚠️ Решение исполнителя (план не задаёт относительный `HOME`): относительный `HOME` — `ProfileError::HomeNotAbsolute`. Иначе каталог состояния зависел бы от текущего каталога процесса, это тот же тихий переход в чужое место, который ADR-0018 запрещает для `SHELLIO_STATE_DIR`.
- [x] создание каталога состояния с `0700` при открытии хранилища (функция профиля, вызываемая из задачи 8)
  - `Profile::ensure_state_dir` публичная: `DirBuilder` с `mode(0o700)`, существующий каталог (в том числе по символической ссылке) принимается без смены прав. Путь занят файлом или файл посреди пути — `ProfileError::NotADirectory` с путём этого файла, иначе `Io`.
  - ⚠️ Решение исполнителя (план не задаёт недостающих родителей): недостающие родители создаются вместе с каталогом и тоже с `0700` (`recursive`), права существующих родителей не меняются. `SHELLIO_STATE_DIR` во вложенный несуществующий каталог работает без ручного `mkdir -p`, а новый родитель не открывает каталог состояния другим пользователям.
- [x] тесты таблицей окружений: без переменной, с абсолютной, с пустой, с относительной `SHELLIO_STATE_DIR`, без `HOME`, пустой `HOME`; `XDG_CONFIG_HOME` игнорируется
  - `environment_table` (девять строк, точные варианты ошибок и пути; сверх списка — абсолютная `SHELLIO_STATE_DIR` без `HOME` и относительный `HOME`), `only_state_dir_and_home_are_read` (окружение запрашивается только по `SHELLIO_STATE_DIR` и `HOME`, запрос `XDG_CONFIG_HOME` роняет тест), `isolated_matches_state_dir_variable`, `database_and_backups_live_in_state_dir`. Окружение — подставное замыкание, настоящий `HOME` не читается.
- [x] тест: новый каталог получает `0700`, права уже существующего каталога не меняются; путь, занятый файлом, — `ProfileError::NotADirectory`
  - `new_state_dir_gets_owner_only_mode` (подкаталог `TempDir`, повторный вызов, каталог конфигурации не создан), `missing_parents_are_created_owner_only`, `existing_state_dir_keeps_its_mode` (каталог с `0755` и файлом внутри), `state_dir_taken_by_file_is_not_a_directory`, `file_in_the_middle_of_state_path_is_not_a_directory`, `symlink_to_directory_is_accepted` (права цели `0750` не меняются). Файл на пути не изменяется.
- [x] `make check` проходит
  - 11 тестов `shellio-store` в выводе `make check`: 10 профиля и проверка `test-support`.
  - ⚠️ Нестабильный тест пробы: первый прогон упал на тесте пробы P0.3 `fixture_reports_ready_reads_replies_and_winch_on_a_real_pty` (`ready.tty` не `/dev/tty…`): отдельно он трижды прошёл, повторный `make check` прошёл целиком; профиль этот тест не затрагивает.

### Task 8: Схема и миграции

**Files:**
- Create: `crates/shellio-store/migrations/0001_init.sql`, `crates/shellio-store/src/{open.rs,migrate.rs}`, `crates/shellio-store/tests/{migrate.rs,schema_v1.sql,fixtures/v1.sql}`

- [x] открытие с прагмами и `Migrator` по разделу «`state.db`»; список миграций подменяется в тестах
  - `Database::open(&Profile, &dyn Clock)`: время имени копии берётся из `Clock` ядра, поэтому тест сверяет точное имя. Подмена списка — `Migrator::new(Vec<Migration>)` и `Database::open_with(…, &Migrator)` за `test-support`, `Migrator::new` требует версии подряд с 1. Соединение для проверок — `Database::connection()` за `test-support`, `SqlitePersist` задачи 9 получит его внутри crate.
  - `StoreError` — ручной `enum`: `Profile`, `Corrupt { path, detail }`, `NewerSchema { found, supported }`, `Backup { path, source: BackupError }`, `Migration { to, source }`, `JournalMode`, `Sqlite`. Он `Send + Sync + 'static` и проходит через `LoadError` как есть.
  - Порядок: каталог состояния `0700`, соединение и `busy_timeout`, чтение `user_version` и числа объектов схемы, затем `NewerSchema`, копия, `journal_mode=WAL` с проверкой ответа, `synchronous=FULL`, `foreign_keys=ON` и миграции. Отказ `Corrupt` даёт первое чтение файла (`NotADatabase` или `DatabaseCorrupt`). Каждая миграция идёт `IMMEDIATE`-транзакцией вместе с `user_version`. Переход в WAL переписывает байты 18–19 заголовка, поэтому он идёт только после проверок файла и версии.
  - ⚠️ Решение исполнителя: копия переводится в режим журнала `DELETE` на собственном соединении в монопольном режиме блокировки. Backup API переносит в копию флаг WAL источника: без перевода она не открывается только для чтения, а рядом с ней остаётся `-shm`. Копия получается самодостаточным файлом с правами `0600`, каталог `backups` создаётся по необходимости с `0700`.
  - ⚠️ Решение исполнителя: занятое имя копии не перезаписывается, хотя backup API перезаписал бы цель. Отказ — `StoreError::Backup` с `BackupError::Exists`, миграция не начинается. Недоделанная копия удаляется.
  - ⚠️ Решение исполнителя: отрицательная `user_version` — `Corrupt`, файл не трогается. Ни один бинарь shellio её не пишет.
  - Решение исполнителя по правилу плана: заголовок файла вручную не проверяется, `Corrupt` сообщает SQLite. Исключение — однобайтовый файл: его SQLite принимает за пустую базу.
- [x] тест схемы: пустой файл после открытия совпадает с эталоном `schema_v1.sql` (`sqlite_schema` без `sqlite_%`), `user_version = 1`, прагмы установлены, `sqlite_version()` не ниже 3.37
  - `fresh_database_matches_schema_v1` проверяет два случая: файла нет и файл пуст. Эталон сравнивается текстом точно, потому что `sqlite_schema` хранит SQL так, как он записан в миграции. Тест сверяет прагмы и `sqlite_version()` (числом), строки `meta.revision = 0` и `window_state (1, NULL, NULL)` и отсутствие копии.
- [x] тесты на синтетической миграции v1→v2: резервная копия создана и открывается с `user_version = 1`, данные сохранены; сбой второй миграции — откат, база на v1, копия есть; неудача копии (каталог `backups` занят файлом) — миграция не начата
  - `migration_to_v2_backs_up_first`: в `backups` ровно одна копия без `-wal` и `-shm`, она открывается только для чтения в режиме `DELETE`. `failed_second_migration_rolls_back_and_keeps_backup`: первый оператор v2 создаёт таблицу, затем падает, таблица откатана. `backup_failure_prevents_migration` проверяет и то, что файл `backups` не изменён.
  - Сверх списка: `failed_migration_on_fresh_file_stays_on_last_good_version` (пустой файл с исправной v1 и сломанной v2 остаётся на v1 без копии) и `existing_backup_name_is_not_overwritten`.
- [x] тест: `user_version` больше известной — `NewerSchema`, байты файла не изменились; файл с мусором — `Corrupt`, файл не изменён
  - `newer_schema_is_refused_without_writing`: файл заводится в режиме `DELETE`, чтобы преждевременный переход в WAL было видно по байтам; рядом не появилось ни `-wal`, ни `-journal`. `garbage_file_is_corrupt_and_untouched` проверяет текст, два байта и заголовок SQLite с мусором после него. Сверх списка — `negative_user_version_is_corrupt`.
- [x] фикстура `fixtures/v1.sql` с заполненными данными как образец для будущих миграций; тест загружает её и открывает без миграции
  - Фикстура самодостаточна: собственная схема, данные и `PRAGMA user_version = 1`, поэтому она остаётся образцом v1 и после появления v2. Данные согласованы с моделью, задача 9 может их загружать: три воркспейса (один удалён), четыре проекта (участники списка, проект вне списка и убранный), корни `git` и `folder` (среди них worktree с ключом основного репозитория), три рабочие области, активное окно с маршрутом, ревизия 12. `fixture_v1_opens_without_migration` сверяет схему с эталоном, число строк и ревизию; копия не появляется.
- [x] `make check` проходит
  - 25 тестов `shellio-store` в выводе `make check`: 15 модульных и 10 из `tests/migrate.rs`. Мутации проверены вручную: переход в WAL до проверки версии роняет три теста, миграция без транзакции — два.

### Task 9: Хранилище как `Persist`

**Files:**
- Create: `crates/shellio-store/src/{persist.rs,load.rs,write.rs}`, `crates/shellio-store/tests/load.rs`, `crates/shellio-store/tests/write.rs`
- Modify: `crates/shellio-store/src/{lib.rs,open.rs}`, `crates/shellio-core/src/{title.rs,error.rs}`

- [x] `SqlitePersist::load` с построчной загрузкой и диагностикой, `commit` с проверкой ревизии по разделу «`state.db`»
  - `SqlitePersist::new(Database)` экспортирован из `shellio_store`, `query.status` получает версию схемы и путь из `Database`. Загрузка — одна читающая транзакция по таблицам в порядке `workspaces`, `projects`, `project_roots`, `workbenches` (`ORDER BY rowid`), затем мягкие ссылки воркспейсов, окно, совпадения позиций и совпадения имён. Каждое поле читается отдельно и проверяется в Rust: плохое значение пропускает строку, а не роняет загрузку. Ссылка владения не на загруженную запись (висячая, не из 16 байт или на пропущенную строку) — пропуск. Байты базы загрузка не меняет.
  - Стабильные причины диагностики (уходят в `query.status`). Пропуск строки: `invalid_id`, `title_empty`, `title_too_long`, `title_control_char` (те же, что у `invalid_argument`, через новый `TitleError::reason()` ядра), `position_out_of_range`, `unknown_kind`, `owner_missing`, `invalid_value` (тип значения не тот, при `STRICT` недостижимо). Строка загружена, значение в памяти сброшено: `title_trimmed`, `last_workbench_missing`, `last_workbench_foreign`, `active_workspace_missing`, `route_workbench_missing`, `route_workbench_foreign`, `row_missing` (у `window_state`). Только диагностика: `position_taken`, `name_taken`. Из совпадающих строк диагностику получают все, кроме строки с наименьшим `rowid`.
  - ⚠️ Решение исполнителя: позиция вне `u32` (отрицательная или больше `u32::MAX`) — пропуск строки с `position_out_of_range`, так же у `list_position`. Модель держит позиции в `u32`, а пропуск — ближайшая к плану категория для значения, которое нельзя загрузить.
  - ⚠️ Решение исполнителя: имя с пробелами по краям загружается обрезанным с диагностикой `title_trimmed`, база не меняется. Пропуск воркспейса вместе со всеми проектами из-за пробела был бы несоразмерным.
  - ⚠️ Решение исполнителя: совпадение имён без учёта регистра в области уникальности (неудалённые воркспейсы, неубранные проекты воркспейса, рабочие области проекта) — только диагностика `name_taken`, обе строки загружены, как совпадение позиций. План такой находки не называл, а внешняя правка может её создать.
  - ⚠️ Решение исполнителя: мягкая ссылка сбрасывается не только висячей, но и чужой, потому что инварианты требуют область своего воркспейса. Это `last_workbench_id` на область другого воркспейса и маршрут на область не активного воркспейса, в том числе при сброшенном активном.
  - ⚠️ Решение исполнителя: нет строки `meta.revision` или ревизия отрицательна — `StoreError::Corrupt`, загрузка отказывает целиком, файл не меняется. Без ревизии запись не может проверить базовую.
  - Запись: `IMMEDIATE`-транзакция, сверка `meta.revision` с базовой ревизией, SQL каждой дельты по одному оператору через общий `exec` (через него задача 10 подключит крючок сбоя) и `meta.revision = base + 1`. Нарушение ограничения SQLite (`UNIQUE`, первичный или внешний ключ) — `CommitError::Conflict` (`storage_conflict`), прочие ошибки SQLite — `Unavailable` (`storage_unavailable`). Любой отказ откатывает транзакцию целиком.
  - ⚠️ Решение исполнителя: изменение записи (`*Renamed`, `WorkspaceLastWorkbenchSet`, `ProjectOpened`) должно затронуть ровно одну строку, иначе `storage_conflict`: запись удалили в обход ядра, а «успешная» запись разошлась бы с перезагрузкой. Путь корня не в UTF-8 тоже `storage_conflict`, без `to_string_lossy` (недостижимо: разрешение такие пути отвергает).
  - ⚠️ Решение исполнителя: `WindowSet` пишет строку окна вставкой с `ON CONFLICT (id) DO UPDATE`, поэтому строка, удалённая снаружи, восстанавливается. Загрузка без этой строки даёт окно по умолчанию и `row_missing`, после чего запуск ядра выполняет фокус. Вставки записей — обычный `INSERT`, совпадение ключа даёт конфликт.
- [x] перевод каждого `Change` в SQL исчерпывающим `match`
  - `write_change` в `src/write.rs` — `match` без `_`. `NULL` пишется явно: маршрут `WorkspaceHome` и `last_workbench: None`.
- [x] тесты записи: каждая дельта, ревизия растёт на 1, неверная базовая ревизия — `storage_conflict` без изменений, нарушение `UNIQUE` ключа корня (запись, вставленная в обход ядра) — `storage_conflict`
  - Тесты записи вынесены в отдельный `tests/write.rs`. `every_change_is_written_and_reloads`: девять записей покрывают все восемь дельт, оба вида корня, проект с двумя корнями, удалённый воркспейс, убранный проект вне списка, переход маршрута с области на главную и сброс `last_workbench`. После каждой записи перезагрузка равна состоянию с применёнными дельтами, ревизия в базе — `base + 1`, `NULL` проверены прямым SQL. `wrong_base_revision_is_conflict_and_changes_nothing` (базы 0, 2 и 7 при ревизии 1, затем верная проходит), `root_key_taken_outside_core_is_conflict` (вставка проекта до корня откатилась). Сверх списка: `primary_and_foreign_key_violations_are_conflicts`, `change_of_record_missing_in_database_is_conflict` (также откат первой, исправной дельты записи), `window_set_restores_window_row_deleted_outside`. Каждый отказ сверяет перезагрузку до и после и код `storage_conflict` через `CoreError::from`.
- [x] тесты повреждённых записей, вставленных прямым SQL: неверная длина ID, пустое имя, неизвестный `kind`, висячая ссылка владения — строка и зависимые пропущены; висячий `last_workbench_id` — воркспейс со всеми проектами загружен, указатель сброшен; совпадение позиций — обе строки загружены; диагностика перечисляет каждую находку, байты базы не изменились
  - `corrupt_rows_are_skipped_with_dependents_and_database_is_untouched` (`tests/load.rs`): у каждого пропуска есть зависимые, их пропуск сверяется точным списком двенадцати находок, а инварианты загруженного состояния — только `PositionTaken`. Байты `state.db` сравниваются после закрытия всех соединений, файл к этому моменту уже в WAL. Сверх списка: `values_outside_model_are_reset_or_reported` (имя на 200 и 201 символ, управляющий символ, обрезка пробелов, позиции −1 и 2³², чужая последняя область, активный воркспейс не из 16 байт и маршрут при сброшенном активном, совпадения имён, имя удалённого воркспейса уникальность не занимает), `window_findings_fall_back_to_workspace_home`, `missing_or_negative_revision_is_corrupt`, `fixture_v1_loads_without_findings` (образец v1 задачи 8 загружается без диагностики на ревизии 12, инварианты целы).
- [x] тест: после загрузки с совпадением позиций новая запись получает «наибольшая + 1», повторная загрузка не теряет ни одной строки
  - `after_position_collision_new_records_go_after_max_and_reload_keeps_rows`: воркспейсы на позициях 0, 0 и удалённый на 7, рабочие области на 2 и 2. Через ядро новый воркспейс получает 8, новая область — 3. Перезагрузка равна снимку ядра вместе с диагностикой, число строк каждой таблицы равно числу загруженных записей.
- [x] тест: висячий `active_workspace_id` сбрасывается загрузкой, старт ядра выполняет фокус одной записью, диагностика есть; запись с непустым `removed_at` держит свой корень (`root_taken`) и не видна в `query.tree`
  - `dangling_active_workspace_is_reset_and_start_focuses_in_one_record`: ревизия 5 → 6, одно событие с одной дельтой `WindowSet` на первый воркспейс по позиции, `active_workspace_missing` в `query.status`, перезагрузка без диагностики. `removed_project_keeps_its_root_and_stays_out_of_tree`: `project.add` другого пути с тем же ключом даёт `root_taken` с убранным проектом и его воркспейсом, дерево воркспейса пусто, ревизия не меняется.
  - Мутации проверены вручную: запись строки при загрузке роняет тест байтов, отключённая проверка позиций — три теста, принятая чужая последняя область — один, изменение без строки как успех — один, ограничение как `Unavailable` — два, маршрут на главную не `NULL` — два.
- [x] `make check` проходит
  - 40 тестов `shellio-store` в выводе `make check`: 16 модульных, 8 из `tests/load.rs`, 10 из `tests/migrate.rs` и 6 из `tests/write.rs`. `cargo clippy -p shellio-store --lib -- -D warnings` без `test-support` тоже чист.

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
