-- Журнал черновиков пробы P0.7 (ADR-0015) в state.db (ADR-0018). Версия схемы — PRAGMA user_version.

-- Сессия ревью принадлежит проекту (ADR-0025): два клона одного remote — разные проекты.
CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL CHECK (project <> ''),
    account TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('github', 'gitlab')),
    host TEXT NOT NULL,
    repo TEXT NOT NULL,
    number INTEGER NOT NULL CHECK (number > 0),
    base_sha TEXT NOT NULL,
    start_sha TEXT NOT NULL,
    head_sha TEXT NOT NULL
);

CREATE INDEX sessions_project ON sessions (project);

-- У черновика своя ревизия якоря: после «перейти» устаревший черновик остаётся на прежней.
CREATE TABLE drafts (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id),
    body TEXT NOT NULL,
    remote_id TEXT,
    base_sha TEXT NOT NULL,
    start_sha TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    anchor_state TEXT NOT NULL CHECK (
        anchor_state IN ('current', 'file_missing', 'path_changed', 'lines_changed', 'not_in_own_diff')
    ),
    placement TEXT NOT NULL CHECK (placement IN ('line', 'reply')),
    reply_thread TEXT,
    old_path TEXT,
    new_path TEXT,
    start_side TEXT CHECK (start_side IN ('old', 'new')),
    start_kind TEXT CHECK (start_kind IN ('added', 'removed', 'context')),
    start_old INTEGER CHECK (start_old >= 0),
    start_new INTEGER CHECK (start_new >= 0),
    end_side TEXT CHECK (end_side IN ('old', 'new')),
    end_kind TEXT CHECK (end_kind IN ('added', 'removed', 'context')),
    end_old INTEGER CHECK (end_old >= 0),
    end_new INTEGER CHECK (end_new >= 0),
    CHECK ((placement = 'reply') = (reply_thread IS NOT NULL)),
    CHECK (
        (placement = 'line') = (
            old_path IS NOT NULL AND new_path IS NOT NULL AND end_side IS NOT NULL
            AND end_kind IS NOT NULL AND end_old IS NOT NULL AND end_new IS NOT NULL
        )
    ),
    CHECK (
        (start_side IS NULL) = (start_kind IS NULL)
        AND (start_side IS NULL) = (start_old IS NULL)
        AND (start_side IS NULL) = (start_new IS NULL)
    ),
    CHECK (placement = 'line' OR start_side IS NULL)
);

CREATE INDEX drafts_session ON drafts (session_id);

CREATE TABLE submissions (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id)
);

-- Элемент отправки — один запрос записи к провайдеру. Отметка входа в in_flight (мс Unix)
-- ставится до запроса и сохраняется в unknown, confirmed и failed.
CREATE TABLE submit_items (
    id INTEGER PRIMARY KEY,
    submission_id INTEGER NOT NULL REFERENCES submissions (id),
    seq INTEGER NOT NULL CHECK (seq >= 0),
    operation TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('planned', 'in_flight', 'confirmed', 'unknown', 'failed')),
    remote_id TEXT,
    candidate TEXT,
    detail TEXT,
    in_flight_at INTEGER,
    CHECK ((state = 'planned') = (in_flight_at IS NULL)),
    UNIQUE (submission_id, seq)
);

CREATE INDEX submit_items_state ON submit_items (state);

CREATE TABLE submit_item_drafts (
    item_id INTEGER NOT NULL REFERENCES submit_items (id),
    draft_id INTEGER NOT NULL REFERENCES drafts (id),
    PRIMARY KEY (item_id, draft_id)
);
