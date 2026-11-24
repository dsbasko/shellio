-- Профиль пункта status-stubs (план P1.5, «Матрица приёмки»): state.db схемы v3 с открытой рабочей областью и
-- двумя каноническими панелями демо, данные которых запуск не принимает. У «Демо: список» строка panel_states
-- версии 2 при state_version плагина 1 — статус state_newer и заглушка «Сохранено более новой версией». У «Демо:
-- поле» данные `{` не разбираются — статус state_corrupt и заглушка «Состояние не прочитано». Обе строки остаются в
-- базе побайтно (ADR-0018, «Уточнено в P1.5 (планирование)»). Строки plugin_registry нет: отпечаток пишет запуск.
-- Схема — образец v3 из crates/shellio-store/tests/schema_v3.sql, а не текущие миграции.
-- plugins.sh загружает файл через /usr/bin/sqlite3 в state.db нового профиля до первого запуска.

CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) STRICT;

CREATE TABLE workspaces (
    id BLOB PRIMARY KEY,
    title TEXT NOT NULL,
    position INTEGER NOT NULL,
    last_workbench_id BLOB,
    deleted_at INTEGER
) STRICT;

CREATE TABLE projects (
    id BLOB PRIMARY KEY,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    title TEXT NOT NULL,
    list_position INTEGER,
    removed_at INTEGER,
    last_opened_at INTEGER
) STRICT;

CREATE TABLE project_roots (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE workbenches (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL REFERENCES projects(id),
    title TEXT NOT NULL,
    position INTEGER NOT NULL
) STRICT;

CREATE TABLE window_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    active_workspace_id BLOB,
    route_workbench_id BLOB
) STRICT;

CREATE TABLE panels (
    id BLOB PRIMARY KEY,
    workbench BLOB NOT NULL REFERENCES workbenches(id),
    plugin TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    opened_from BLOB
) STRICT;

CREATE TABLE workbench_layouts (
    workbench BLOB PRIMARY KEY REFERENCES workbenches(id),
    layout TEXT NOT NULL
) STRICT;

CREATE TABLE panel_states (
    panel BLOB PRIMARY KEY REFERENCES panels(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    data TEXT NOT NULL
);

CREATE TABLE plugin_registry (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    fingerprint TEXT NOT NULL
) STRICT;

INSERT INTO meta (key, value) VALUES ('revision', 8);

INSERT INTO workspaces (id, title, position, last_workbench_id, deleted_at) VALUES
    (X'019a8f2e00007000800000000000a301', 'Статусы', 0, X'019a8f2e00007000800000000000c301', NULL);

INSERT INTO projects (id, workspace_id, title, list_position, removed_at, last_opened_at) VALUES
    (X'019a8f2e00007000800000000000b301', X'019a8f2e00007000800000000000a301', 'status', 0, NULL, 1795000100000);

INSERT INTO project_roots (id, project_id, path, kind, key) VALUES
    (X'019a8f2e00007000800000000000d301', X'019a8f2e00007000800000000000b301', '/tmp/shellio-e2e-plugins-status', 'folder', '/tmp/shellio-e2e-plugins-status');

INSERT INTO workbenches (id, project_id, title, position) VALUES
    (X'019a8f2e00007000800000000000c301', X'019a8f2e00007000800000000000b301', 'main', 0);

INSERT INTO window_state (id, active_workspace_id, route_workbench_id) VALUES
    (1, X'019a8f2e00007000800000000000a301', X'019a8f2e00007000800000000000c301');

-- Навигатор в колонке; в центре вкладки терминала и «Демо: поле», показана и с фокусом вторая; «Демо: список»
-- в правой зоне.
INSERT INTO panels (id, workbench, plugin, title, created_at, opened_from) VALUES
    (X'019a8f2e00007000800000000000e301', X'019a8f2e00007000800000000000c301', 'navigator', 'Навигатор', 1795000300001, NULL),
    (X'019a8f2e00007000800000000000e302', X'019a8f2e00007000800000000000c301', 'terminal', 'Терминал', 1795000300001, NULL),
    (X'019a8f2e00007000800000000000e303', X'019a8f2e00007000800000000000c301', 'demo-list', 'Демо: список', 1795000400000, NULL),
    (X'019a8f2e00007000800000000000e304', X'019a8f2e00007000800000000000c301', 'demo-native', 'Демо: поле', 1795000500000, NULL);

INSERT INTO workbench_layouts (workbench, layout) VALUES
    (X'019a8f2e00007000800000000000c301', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e301","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e303"],"active":0,"share":1000}],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e302","019a8f2e-0000-7000-8000-00000000e304"],"active":1,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e302","019a8f2e-0000-7000-8000-00000000e304"]}}');

INSERT INTO panel_states (panel, version, data) VALUES
    (X'019a8f2e00007000800000000000e301', 1, 'null'),
    (X'019a8f2e00007000800000000000e302', 1, 'null'),
    (X'019a8f2e00007000800000000000e303', 2, '{"key":"a","history":[],"details":false}'),
    (X'019a8f2e00007000800000000000e304', 1, '{');

PRAGMA user_version = 3;
