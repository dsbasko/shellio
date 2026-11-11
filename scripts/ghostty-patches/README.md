# Патчи libghostty

`scripts/ghostty/build.sh` накладывает патчи этого каталога на `GHOSTTY_REV` из `scripts/ghostty/pin` по порядку имён. Каждый патч сначала проходит `git apply --check`, и сборка останавливается, если он не накладывается. Имена и содержимое патчей входят в штамп набора, поэтому изменённый, добавленный или удалённый патч делает опубликованный набор устаревшим ([ADR-0003](../../docs/adr/0003-terminal-libghostty.md)).

## 0001-drain-mailbox-during-surface-teardown.patch

Назначение. Переносит на `83fb956` (v1.4.0) разгрузку очереди приложения из [ghostty PR 14444](https://github.com/ghostty-org/ghostty/pull/14444). При закрытии поверхности главный поток ждёт завершения рабочих потоков и кадров Metal, а и рабочие потоки, и колбэки завершения кадра могут блокироваться на записи в полную очередь приложения. Временный потребитель освобождает место в очереди, пока не завершится `renderer.deinit()`, и сохраняет сообщения других поверхностей до следующего такта приложения. Он заканчивает работу ограниченным последним проходом, отбрасывает сообщения закрытой поверхности, в том числе оставшиеся от предыдущих закрытий, и освобождает их данные.

Условие удаления. Патч удаляется, когда `GHOSTTY_REV` содержит равноценное исправление upstream, у которого разгрузка остаётся активной до конца `renderer.deinit()`. Порядок вызовов в нынешней редакции PR 14444 этому условию ещё не отвечает. Удаление патча, как и обновление ревизии, выполняется отдельным планом по процедуре ADR-0003.

Источник. Файл перенесён без изменений из agterm, `scripts/ghostty-patches/0001-drain-mailbox-during-surface-teardown.patch`, коммит `6511255e04a7c93be1fa1d8c7e16749b70cf3524`. Связка этой ревизии и патча подтверждена на macOS 27. Перенос разрешён [ADR-0022](../../docs/adr/0022-licensing-and-borrowing.md) при сохранении уведомления ниже.

Правка. Патч накладывается на чистую копию `GHOSTTY_REV`. Там же выполняются `zig fmt --check src/App.zig src/Surface.zig` и `zig build test -Dtest-filter=teardown -Demit-macos-app=false`. Новый патч берётся из `git diff -- src/App.zig src/Surface.zig` и проверяется `git apply --check` на второй чистой копии. Ёмкость очереди не меняется, и `ghostty_surface_free` не уходит с потока приложения.

## Уведомление об авторстве

Патч `0001-drain-mailbox-during-surface-teardown.patch` получен из agterm и распространяется на условиях лицензии MIT:

```text
MIT License

Copyright (c) 2026 Umputun

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Код ghostty, к которому применяется патч, распространяется по лицензии MIT проекта ghostty. Её текст входит в набор как `vendor/ghostty/LICENSE`.
