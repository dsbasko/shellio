-- Схема v3: данные панелей плагинов, вызывающая панель и отпечаток реестра плагинов (ADR-0018,
-- «Уточнено в P1.5 (планирование)»).
-- `panel_states` не STRICT: версию и текст данных проверяет загрузка, а не SQLite. Строка,
-- испорченная внешней правкой, загружается находкой и остаётся в базе побайтно.

CREATE TABLE panel_states (
    panel BLOB PRIMARY KEY REFERENCES panels(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    data TEXT NOT NULL
);

ALTER TABLE panels ADD COLUMN opened_from BLOB;

CREATE TABLE plugin_registry (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    fingerprint TEXT NOT NULL
) STRICT;
