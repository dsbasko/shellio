CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) STRICT;

CREATE TABLE panel_states (
    panel BLOB PRIMARY KEY REFERENCES panels(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    data TEXT NOT NULL
);

CREATE TABLE panels (
    id BLOB PRIMARY KEY,
    workbench BLOB NOT NULL REFERENCES workbenches(id),
    plugin TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL
, opened_from BLOB) STRICT;

CREATE TABLE plugin_registry (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    fingerprint TEXT NOT NULL
) STRICT;

CREATE TABLE project_roots (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE projects (
    id BLOB PRIMARY KEY,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    title TEXT NOT NULL,
    list_position INTEGER,
    removed_at INTEGER,
    last_opened_at INTEGER
) STRICT;

CREATE TABLE window_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    active_workspace_id BLOB,
    route_workbench_id BLOB
) STRICT;

CREATE TABLE workbench_layouts (
    workbench BLOB PRIMARY KEY REFERENCES workbenches(id),
    layout TEXT NOT NULL
) STRICT;

CREATE TABLE workbenches (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL REFERENCES projects(id),
    title TEXT NOT NULL,
    position INTEGER NOT NULL
) STRICT;

CREATE TABLE workspaces (
    id BLOB PRIMARY KEY,
    title TEXT NOT NULL,
    position INTEGER NOT NULL,
    last_workbench_id BLOB,
    deleted_at INTEGER
) STRICT;
