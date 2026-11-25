---
worth: yes
where: Cargo.toml
added: 2026-11-25
---
# Манифесты Cargo не называют лицензию

ADR-0039 выбрал для проекта MIT, текст лежит в `LICENSE`.
В `[workspace.package]` корневого `Cargo.toml` поля `license` нет, а манифесты crate его не наследуют: у них есть `version.workspace`, `edition.workspace`, `rust-version.workspace` и `publish.workspace`, но нет `license.workspace`.

Нужно добавить `license = "MIT"` в `[workspace.package]` и `license.workspace = true` в манифест каждого crate.
