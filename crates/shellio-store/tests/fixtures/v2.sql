-- Образец state.db версии 2 с заполненными данными, как его оставляет бинарь со схемой v2.
-- Самодостаточен: схема записана здесь, а не берётся из миграций, поэтому остаётся образцом v2
-- и после появления следующих версий. Данные v1 те же, что в `v1.sql`, ревизия — 20.
-- Строк данных панелей и отпечатка реестра плагинов в v2 нет: их дописывает запуск ядра на v3.

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

INSERT INTO meta (key, value) VALUES ('revision', 20);

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

-- Панели и раскладки, как их пишет бинарь со схемой v2: навигатор и терминал в каждой рабочей
-- области, в «main» проекта shellio ещё вкладка плагина «notes», которого в реестре нет.
INSERT INTO panels (id, workbench, plugin, title, created_at) VALUES
    (X'019a8f2e00007000800000000000e001', X'019a8f2e00007000800000000000c001', 'navigator', 'Навигатор', 1795000300001),
    (X'019a8f2e00007000800000000000e002', X'019a8f2e00007000800000000000c001', 'terminal', 'Терминал', 1795000300001),
    (X'019a8f2e00007000800000000000e007', X'019a8f2e00007000800000000000c001', 'notes', 'notes', 1795000400000),
    (X'019a8f2e00007000800000000000e003', X'019a8f2e00007000800000000000c002', 'navigator', 'Навигатор', 1795000300002),
    (X'019a8f2e00007000800000000000e004', X'019a8f2e00007000800000000000c002', 'terminal', 'Терминал', 1795000300002),
    (X'019a8f2e00007000800000000000e005', X'019a8f2e00007000800000000000c003', 'navigator', 'Навигатор', 1795000300003),
    (X'019a8f2e00007000800000000000e006', X'019a8f2e00007000800000000000c003', 'terminal', 'Терминал', 1795000300003);

INSERT INTO workbench_layouts (workbench, layout) VALUES
    (X'019a8f2e00007000800000000000c001', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e001","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e002","019a8f2e-0000-7000-8000-00000000e007"],"active":0,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e002"]}}'),
    (X'019a8f2e00007000800000000000c002', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e003","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e004"],"active":0,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e004"]}}'),
    (X'019a8f2e00007000800000000000c003', '{"version":1,"layout":{"navigator":"019a8f2e-0000-7000-8000-00000000e005","zones":{"left":{"parts":[],"last_active":0},"right":{"parts":[],"last_active":0},"center":{"parts":[{"tabs":["019a8f2e-0000-7000-8000-00000000e006"],"active":0,"share":1000}],"last_active":0},"bottom":{"parts":[],"last_active":0}},"sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"only":null,"zoomed":null,"focus":["019a8f2e-0000-7000-8000-00000000e006"]}}');

PRAGMA user_version = 2;
