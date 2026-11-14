# Документация shellio

- [Требования и их трассировка](requirements.md)
- [Архитектура](architecture.md)
- [Пользовательские потоки и адаптация панелей](ux.md)
- [Дорожная карта: фазы и планы](roadmap.md)
- [Решения (ADR)](adr/README.md)
- `plans/` — детальные ralphex-планы, создаются по мере работы
- [Отчёты проб фазы 0](probes/) — отчёт `<id>-<slug>.md` и выбранные данные в `data/` ([ADR-0034](adr/0034-probe-code.md)). Первый — [P0.2 probe-appkit-shell](probes/p0.2-appkit-shell.md)
- Сборка: цели `Makefile` описаны в разделе «Сборка» [AGENTS.md](../AGENTS.md), идентичность подписи — в [scripts/signing/README.md](../scripts/signing/README.md), патчи ghostty — в [scripts/ghostty-patches/README.md](../scripts/ghostty-patches/README.md)
- [Интерактивный прототип](../prototype/index.html) — эталон поведения и вида панелей, открывается в браузере. Это HTML и JavaScript, а не код продукта. В ADR-0023 и ADR-0024 он упомянут по прежнему адресу `~/Box/shellio/prototype`.
