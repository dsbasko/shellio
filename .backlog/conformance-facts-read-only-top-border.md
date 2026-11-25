---
worth: yes
where: prototype/js/conformance.js
added: 2026-11-25
---
# Факты сверки читают только верхнюю границу элемента

`conformance.facts()` берёт цвет, толщину и стиль линии только у верхней стороны (`borderTopColor`, `borderTopWidth`, `borderTopStyle`), а контракт полей (`crates/shellio-conformance/src/fields.rs`) знает одну линию на элемент без стороны.

Линия, нарисованная другой стороной, в паспорт не попадает. Пример: `.titlebar` задаёт `border-bottom: 1px solid var(--line)` (`prototype/css/app.css:15`), а элемент `chrome` в паспортах `chrome-titlebar` записан без `line_color` и с `borderStyle: none` в фактах. Так же пропадут разделители строки вкладок и строки статуса, если они заданы нижней или боковой границей.

Нужно решить форму поля (сторона линии или четыре стороны) в паспорте, у продукта (`style` в `ui.inspect`) и у оракула, поднять `SNAP_SCHEMA` и переснять паспорта до первого судящего прогона (задача 9 плана `prototype-conformance`).
