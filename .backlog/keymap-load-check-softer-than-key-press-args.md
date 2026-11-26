---
worth: yes
where: apps/shellio/src/keymap_service.rs
added: 2026-11-26
---
# Проверка `args` при загрузке `keymap.toml` мягче, чем при нажатии

`host_args` (`keymap_service.rs:177`) принимает у `panel.select` любую строку `panel`, а у `tab.select` любое `index` от 1 в `u64`.
`key_actions::recognize` при нажатии разбирает `panel` как `PanelId` и требует, чтобы `index` помещался в `NonZeroU32` (`key_actions.rs:118`, `:126`).
Строка `args = { panel = "invalid" }` или `index = 4294967296` проходит загрузку без диагностики, заменяет умолчание и отказывает на каждом нажатии.
ADR-0010 требует отключать строку с `args`, которые отвергает проверка действия, с номером строки в диагностике.

Нужно, чтобы `Actions::check` для действий хоста звал `key_actions::recognize`, а `HostArgs` и `host_args` ушли: одно правило при загрузке и при нажатии.
