---
worth: yes
where: crates/shellio-appkit/src/components/text_field.rs
added: 2026-11-26
---
# Прерванный ввод в поле не объявляет конец ввода наблюдателю

`TextField::set_hidden(true)` (`text_field.rs:350`) и операция `TextFieldOp::Editable(false)` (`:485`) прерывают ввод через `abortEditing`.
`abortEditing` не вызывает `textDidEndEditing:`, поэтому наблюдатель `App::on_field_focus` не получает `FieldFocus::Ended`, и режим панели за полем не согласуется: запись `Input` маршрутизатора остаётся.

Строка фильтра закрыта отдельно: `on_filter_row` (`apps/shellio/src/window_ui.rs`) после закрытия строки сам зовёт `follow_field` (срез 7 задачи 10 плана `navigation-keymap`).
Остальные вызывающие не закрыты: `place_filters` прячет строки других панелей, поле тела плагина может стать нередактируемым во время ввода.
В прогонах эти пути не наблюдались.

Нужно объявлять `FieldFocus::Ended` в обоих местах прерывания, чтобы наблюдатель не зависел от того, кто прервал ввод.
