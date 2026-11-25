---
worth: yes
where: docs/adr/0010-input-and-navigation.md:104
added: 2026-11-24
---
# ADR-0010, «Уточнено в P0.3», не отражает приёмочный проход

Раздел «Уточнено в P0.3» в `docs/adr/0010-input-and-navigation.md` написан по автоматическому прогону 2026-11-15: «ручной проход P0.3 не проведён», `Ctrl-\ Ctrl-\ Ctrl-n` в `:terminal` nvim переводит его в Normal (`mode(1)` `t` → `nt`).

Отчёт `docs/probes/p0.3-ghostty-surface.md` (строки 15, 407, 475) записал приёмочный проход 2026-11-18: `h4-nvim-terminal-chord` — `fail` под русской раскладкой, `mode(1)` остаётся `t`; `Cmd-K` при marked text доходит до привязки ghostty. Обе находки переданы P2.2. ADR-0003 и строка ADR-0010 в `docs/adr/README.md` обновлены, сам раздел ADR-0010 — нет.

Нужно дополнить раздел выводом приёмочного прохода и передачей в P2.2.
