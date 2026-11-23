-- Профиль пункта restore-legacy (план P1.4, «Матрица приёмки»): state.db схемы v1 с открытой рабочей областью
-- (window_state.route_workbench_id) и last_workbench_id воркспейса, как его оставляет бинарь со схемой v1.
-- Таблиц панелей и раскладок нет: раскладку по умолчанию запуск ядра пишет сам (ADR-0018, «Уточнено в P1.4
-- (планирование)»). Схема — образец v1 из crates/shellio-store/tests/fixtures/v1.sql, а не текущие миграции.
-- layout.sh загружает файл через /usr/bin/sqlite3 в state.db нового профиля до первого запуска.

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

INSERT INTO meta (key, value) VALUES ('revision', 4);

INSERT INTO workspaces (id, title, position, last_workbench_id, deleted_at) VALUES
    (X'019a8f2e00007000800000000000a101', 'Наследие', 0, X'019a8f2e00007000800000000000c101', NULL);

INSERT INTO projects (id, workspace_id, title, list_position, removed_at, last_opened_at) VALUES
    (X'019a8f2e00007000800000000000b101', X'019a8f2e00007000800000000000a101', 'legacy', 0, NULL, 1795000100000);

INSERT INTO project_roots (id, project_id, path, kind, key) VALUES
    (X'019a8f2e00007000800000000000d101', X'019a8f2e00007000800000000000b101', '/tmp/shellio-e2e-legacy', 'folder', '/tmp/shellio-e2e-legacy');

INSERT INTO workbenches (id, project_id, title, position) VALUES
    (X'019a8f2e00007000800000000000c101', X'019a8f2e00007000800000000000b101', 'main', 0);

INSERT INTO window_state (id, active_workspace_id, route_workbench_id) VALUES
    (1, X'019a8f2e00007000800000000000a101', X'019a8f2e00007000800000000000c101');

PRAGMA user_version = 1;
