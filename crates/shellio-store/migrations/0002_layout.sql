-- Схема v2: панели рабочих областей и их раскладки (ADR-0018, «Уточнено в P1.4 (планирование)»).
-- Раскладка — JSON модели в конверте с версией формата. Её проверяет загрузка, а не SQLite:
-- испорченная раскладка загружается находкой, а не роняет вставку при внешней правке.

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
