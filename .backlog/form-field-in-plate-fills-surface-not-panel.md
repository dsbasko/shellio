---
worth: yes
where: crates/shellio-appkit/src/components/text_field.rs
added: 2026-11-26
---
# Поле формы внутри плитки красится `surface`, а прототип там даёт `panel`

Внутри плитки панели прототип переопределяет `--surface` цветом панели: `.panel:not(.flat) { --surface: var(--panel); --head: var(--panel-head); }` (`prototype/css/app.css:61`).
Поэтому `.field` с `background: var(--surface)` (`prototype/css/app.css:292`) в теле панели получает `panel`: 255, 255, 255 в светлой теме и 10, 9, 8 в тёмной.
Продукт у вида `TextFieldKind::Field` берёт `colors.surface` (`TextFieldKind::fill`): 250, 249, 246 и 25, 24, 22 — вне допуска канала сверки.

Сверка этого не видит: поле «Демо: поле» лежит под исключением тел демо.
У строки фильтра (`TextFieldKind::Filter`) то же расхождение исправлено в задаче 6 плана `navigation-keymap` по паспорту `panel-filter`.

Нужно решить, какой цвет получает поле формы в теле панели и вне плитки, и дать компоненту заливку по месту, а не одну на вид.
