---
worth: yes
where: docs/adr/0018-storage-and-config.md
added: 2026-11-26
---
# ADR-0018: «тесты не зависят от наблюдения за файлом», а два теста `make check` ждут FSEvents

Раздел «Уточнено в P1.6», пункт «Тесты»: «От наблюдения за файлом тесты не зависят: `keymap.reload` применяет файл синхронно».
`watcher_applies_an_edit_in_a_directory_created_after_start` и `watcher_applies_an_edit_under_a_symlinked_path` (`apps/shellio/src/keymap_service.rs`, `watcher_follows`) запускают службу с наблюдением, `keymap.reload` не зовут и ждут версию, которую публикует только FSEvents, с крайним сроком `WATCH_GUARD` 30 с.
Оба входят в `make check` и CI: машина, которая не доставит событие, уронит их по сроку.

Нужно записать в ADR-0018, что эти два теста зависят от FSEvents с крайним сроком 30 с, либо вынести их из `make check`.
