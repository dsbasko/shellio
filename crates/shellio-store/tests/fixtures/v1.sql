-- Образец state.db версии 1 с заполненными данными, как его оставляет бинарь со схемой v1.
-- Самодостаточен: схема записана здесь, а не берётся из миграций, поэтому остаётся образцом v1
-- и после появления следующих версий. Тесты будущих миграций начинают с этого файла.
-- Данные согласованы с моделью: ID по 16 байт, ссылки владения и мягкие ссылки существуют,
-- ключи корней уникальны, имена уникальны в своей области.

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

INSERT INTO meta (key, value) VALUES ('revision', 12);

-- «Основной» активен и открыт на области «main» проекта shellio, «Архив» удалён.
INSERT INTO workspaces (id, title, position, last_workbench_id, deleted_at) VALUES
    (X'019a8f2e00007000800000000000a001', 'Основной', 0, X'019a8f2e00007000800000000000c001', NULL),
    (X'019a8f2e00007000800000000000a002', 'Работа', 1, NULL, NULL),
    (X'019a8f2e00007000800000000000a003', 'Архив', 2, NULL, 1795000000000);

-- Два участника списка в «Основном»; в «Работе» один проект вне списка и один убранный.
INSERT INTO projects (id, workspace_id, title, list_position, removed_at, last_opened_at) VALUES
    (X'019a8f2e00007000800000000000b001', X'019a8f2e00007000800000000000a001', 'shellio', 0, NULL, 1795000100000),
    (X'019a8f2e00007000800000000000b002', X'019a8f2e00007000800000000000a001', 'notes', 1, NULL, NULL),
    (X'019a8f2e00007000800000000000b003', X'019a8f2e00007000800000000000a002', 'site', NULL, NULL, 1795000050000),
    (X'019a8f2e00007000800000000000b004', X'019a8f2e00007000800000000000a002', 'old', NULL, 1795000200000, NULL);

-- Репозиторий, папка, связанная worktree с ключом основного репозитория, корень убранного проекта.
INSERT INTO project_roots (id, project_id, path, kind, key) VALUES
    (X'019a8f2e00007000800000000000d001', X'019a8f2e00007000800000000000b001', '/Users/example/src/shellio', 'git', '/Users/example/src/shellio/.git'),
    (X'019a8f2e00007000800000000000d002', X'019a8f2e00007000800000000000b002', '/Users/example/notes', 'folder', '/Users/example/notes'),
    (X'019a8f2e00007000800000000000d003', X'019a8f2e00007000800000000000b003', '/Users/example/src/site-feature', 'git', '/Users/example/src/site/.git'),
    (X'019a8f2e00007000800000000000d004', X'019a8f2e00007000800000000000b004', '/Users/example/old', 'folder', '/Users/example/old');

INSERT INTO workbenches (id, project_id, title, position) VALUES
    (X'019a8f2e00007000800000000000c001', X'019a8f2e00007000800000000000b001', 'main', 0),
    (X'019a8f2e00007000800000000000c002', X'019a8f2e00007000800000000000b001', 'review', 1),
    (X'019a8f2e00007000800000000000c003', X'019a8f2e00007000800000000000b003', 'main', 0);

INSERT INTO window_state (id, active_workspace_id, route_workbench_id) VALUES
    (1, X'019a8f2e00007000800000000000a001', X'019a8f2e00007000800000000000c001');

PRAGMA user_version = 1;
