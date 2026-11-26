---
worth: yes
where: crates/shellio-conformance/src/scene.rs:149
added: 2026-11-26
---
# Сцена сверки хранит режим и подсказки, которых никто не читает

После среза 3а задачи 8 плана `navigation-keymap` левую часть строки статуса сцены считает общая отрисовка прототипа: режим следует за получателем ввода, подсказки — за возможностями плагина и действующим keymap.
Поля `SceneStatus::mode` и `SceneStatus::hints` (`crates/shellio-conformance/src/scene.rs:152`, `:157`) и поле `mode` сцены (`conformance/scenes/p15.json:145`, `"mode": "nav"`) остались: прототип их не читает (`prototype/js/conformance.js`, `prototype/js/core/shell.js` к ним не обращаются), сцена `p15` блок `status` не задаёт.
Сообщение состояний отказа тоже идёт не из сцены: `status-split-no-room` и `status-wrong-axis` получают его от отказа, `status-keymap-error` — из поля `message` шага `keymap`.

Нужно убрать поля из типа сцены вместе с их проверками и тестами, а `mode` — из `p15.json`.
Правка сцены меняет штамп всех паспортов: она идёт одним коммитом с полным `make passports`, поэлементная разница — только штамп.
