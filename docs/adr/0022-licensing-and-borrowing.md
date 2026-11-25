# ADR-0022: Лицензии и заимствование кода

- Статус: Accepted
- Дата: 2026-11-09

## Контекст

Проект вдохновлён agterm и detached.
Лицензии у них разные.

## Решение

- agterm (MIT), zmx (MIT): код можно переносить с сохранением уведомления об авторстве.
- detached (GPL-3.0-or-later): код не копируется. Берутся только идеи и уроки, описанные в его документах. Иначе проект унаследует GPL.
- libghostty (MIT): статически линкуется, уведомление входит в поставку.
- Новые зависимости проверяются на совместимость с выбранной лицензией проекта.

## Отвергнутые варианты

- Принять GPL ради кода detached: основные компоненты detached связаны с GPUI и в AppKit-стеке всё равно не переносятся.

## Последствия

Сторонние уведомления собираются в `THIRD_PARTY_NOTICES` при релизе.

## Основания

`agterm/LICENSE`, `detached/LICENSE`, `neurosnap/zmx` (MIT).

## Уточнено в `prototype-conformance` (планирование)

Решение внеочередного плана `prototype-conformance` [дорожной карты](../roadmap.md), принятое при планировании 2026-11-25: по решению владельца моноширинный шрифт JetBrains Mono вкладывается и в прототип, и в бандл продукта ([ADR-0024](0024-main-screen-design.md), [ADR-0037](0037-prototype-conformance.md)).
Решение не меняется.

- Лицензия. JetBrains Mono распространяется под SIL Open Font License 1.1. OFL разрешает вкладывать файлы шрифта в программу и распространять их вместе с ней при сохранении уведомления об авторстве и текста лицензии, продавать сам шрифт отдельно нельзя. С лицензией проекта это совместимо. Файлы не изменяются, не урезаются и не переименовываются.
- Источник это официальный выпуск JetBrains Mono v2.304 от 2023-01-14: архив `https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip` длиной 5622857 байт, SHA-256 `6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`. Из архива берутся четыре статических TTF и текст лицензии:

  | Файл архива | Начертание | Байт | SHA-256 |
  | --- | --- | --- | --- |
  | `fonts/ttf/JetBrainsMono-Regular.ttf` | 400 | 273900 | `a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f` |
  | `fonts/ttf/JetBrainsMono-Medium.ttf` | 500 | 273860 | `31c92d01a8a08528b718a43addf0ad3df0af2ca4b7b3290a452f70f358e14d3d` |
  | `fonts/ttf/JetBrainsMono-SemiBold.ttf` | 600 | 277092 | `1b3bfa1ed5665a4ce3f9feb68d2d4e40e70bf8b4b7d9a3edd418f321b4e166a0` |
  | `fonts/ttf/JetBrainsMono-Bold.ttf` | 700 | 277828 | `5590990c82e097397517f275f430af4546e1c45cff408bde4255dad142479dcb` |
  | `OFL.txt` | лицензия | 4399 | `30f0c136e3c88e422d0791acd97238870f9054a9729bc34cf2ff0d4ed8cac4ad` |

- Прототип. Те же четыре файла и `OFL.txt` лежат в `prototype/fonts/`. `prototype/css/tokens.css` объявляет их через `@font-face`, ссылки на Google Fonts из `prototype/index.html` удаляются, сетевых шрифтов у прототипа нет. Inter не вкладывается: в стеке `--ui` его опережает `-apple-system`.
- Бандл. Те же четыре файла лежат в `Contents/Resources/Fonts/` с ключом `ATSApplicationFontsPath` в `Info.plist`, лицензия: `Contents/Resources/licenses/JetBrainsMono-OFL.txt`. `scripts/check-bundle.sh` сверяет наличие и SHA-256 каждого файла с `prototype/fonts/`. Отсутствующий или подменённый шрифт: отказ, а не тихий запасной шрифт. Уведомление входит в `THIRD_PARTY_NOTICES` при релизе.
- Недоступный источник. Если официальный выпуск недоступен по закреплённому адресу или суммы не совпали, это ошибка исполнения: подмена другим источником не делается.

Версия, статические TTF вместо вариативного шрифта и имя файла лицензии в бандле выбраны исполнителем при записи раздела по правилу плана.
План закрепляет источник (официальный выпуск), но не версию: v2.304 это последний выпуск на 2026-11-25.
Статические файлы нужны потому, что одни и те же файлы читают `@font-face` с явными начертаниями и `ATSApplicationFontsPath`, а `check-bundle.sh` сверяет их побайтно.

## Уточнено в `prototype-conformance`

Вывод плана [`prototype-conformance`](../plans/20261125-prototype-conformance.md) по `make check` и приёмочному проходу исполнителя 2026-11-25.
Решение не меняется, статус остаётся `Accepted`.

- Четыре файла JetBrains Mono v2.304 и `OFL.txt` лежат в `prototype/fonts/` и совпадают с закреплённым выпуском: тест `prototype_fonts_are_the_pinned_release` (`crates/shellio-conformance/tests/repo.rs`).
- В `prototype/index.html` и `prototype/css/tokens.css` нет адресов Google Fonts, в `tokens.css` четыре `@font-face`.
- Бандл несёт те же файлы и лицензию. Отказы `check-bundle.sh` на отсутствующем и подменённом шрифте проверяет `scripts/test-bundle.sh`.
- Обе стороны рисуют вложенным файлом, а не одноимённым системным шрифтом. Пункт `fonts-bundled` итогового `make e2e` получил `pass`: у плашки `⌘K` семейство `JetBrains Mono`, файл лежит в `Contents/Resources/Fonts/` бандла. Подмену системным шрифтом того же имени оракул отличает (`mono_text_from_a_system_font_of_the_same_name_is_a_font_mismatch`).

## Условие пересмотра

Смена лицензии проекта.
