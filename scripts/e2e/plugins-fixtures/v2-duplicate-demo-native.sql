-- Профиль пункта legacy-duplicate (план P1.5, «Матрица приёмки»): state.db схемы v2 с двумя панелями singleton
-- demo-native («Демо: поле», PerApp) в рабочих областях A и B одного проекта, как их мог оставить бинарь P1.4:
-- layout split --open тогда принимал любой plugin id и кардинальность не проверял. Старшая панель — в A (меньший
-- created_at), открыта рабочая область A. Запуск на v3 оставляет обе строки и раскладки: панель A становится
-- канонической, панель B получает статус duplicate и заглушку «Уже открыт» (план P1.5, «Унаследованные дубликаты
-- singleton»). Таблиц panel_states и plugin_registry и столбца panels.opened_from в v2 нет: их добавляет миграция.
-- Схема — образец v2 из crates/shellio-store/tests/fixtures/v2.sql, а не текущие миграции.
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
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE workbench_layouts (
    workbench BLOB PRIMARY KEY REFERENCES workbenches(id),
    layout TEXT NOT NULL
) STRICT;

INSERT INTO meta (key, value) VALUES ('revision', 12);

INSERT INTO workspaces (id, title, position, last_workbench_id, deleted_at) VALUES
    (X'019a8f2e00007000800000000000a401', 'Дубликаты', 0, X'019a8f2e00007000800000000000c401', NULL);

INSERT INTO projects (id, workspace_id, title, list_position, removed_at, last_opened_at) VALUES
    (X'019a8f2e00007000800000000000b401', X'019a8f2e00007000800000000000a401', 'duplicate', 0, NULL, 1795000100000);

INSERT INTO project_roots (id, project_id, path, kind, key) VALUES
    (X'019a8f2e00007000800000000000d401', X'019a8f2e00007000800000000000b401', '/tmp/shellio-e2e-plugins-duplicate', 'folder', '/tmp/shellio-e2e-plugins-duplicate');

INSERT INTO workbenches (id, project_id, title, position) VALUES
    (X'019a8f2e00007000800000000000c401', X'019a8f2e00007000800000000000b401', 'A', 0),
    (X'019a8f2e00007000800000000000c402', X'019a8f2e00007000800000000000b401', 'B', 1);

INSERT INTO window_state (id, active_workspace_id, route_workbench_id) VALUES
    (1, X'019a8f2e00007000800000000000a401', X'019a8f2e00007000800000000000c401');

-- В каждой рабочей области навигатор в колонке, в центре вкладки терминала и «Демо: поле», показана и с фокусом
-- вторая.
INSERT INTO panels (id, workbench, plugin, title, created_at) VALUES
    (X'019a8f2e00007000800000000000e401', X'019a8f2e00007000800000000000c401', 'navigator', 'Навигатор', 1795000300001),
    (X'019a8f2e00007000800000000000e402', X'019a8f2e00007000800000000000c401', 'terminal', 'Терминал', 1795000300001),
    (X'019a8f2e00007000800000000000e403', X'019a8f2e00007000800000000000c401', 'demo-native', 'Демо: поле', 1795000400000),
    (X'019a8f2e00007000800000000000e404', X'019a8f2e00007000800000000000c402', 'navigator', 'Навигатор', 1795000300002),
    (X'019a8f2e00007000800000000000e405', X'019a8f2e00007000800000000000c402', 'terminal', 'Терминал', 1795000300002),
    (X'019a8f2e00007000800000000000e406', X'019a8f2e00007000800000000000c402', 'demo-native', 'Демо: поле', 1795000500000);

INSERT INTO workbench_layouts (workbench, layout) VALUES
    (X'019a8f2e00007000800000000000c401', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e401","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e402","019a8f2e-0000-7000-8000-00000000e403"],"active":1,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e402","019a8f2e-0000-7000-8000-00000000e403"]}}'),
    (X'019a8f2e00007000800000000000c402', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e404","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e405","019a8f2e-0000-7000-8000-00000000e406"],"active":1,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e405","019a8f2e-0000-7000-8000-00000000e406"]}}');

PRAGMA user_version = 2;
