---
worth: maybe
where: apps/shellio/tests/mode_race.rs:284
added: 2026-11-27
---
# Тест второго запуска галереи `mode_race` упирается в охранник зависания в полном прогоне

`gallery_second_launch_does_not_change_normal_owner` упал в `make check` 2026-11-27 на `guard` с «ожидание теста зависло: Elapsed(())», цель шла 40,72 с, журнал `target/task6s4/check.log:2669`.
Правка перед прогоном касалась только `scripts/e2e/conformance.sh`, продукт не менялся.
Отдельно `cargo test -p shellio --test mode_race --locked` прошёл три раза подряд за 6–13 с.

Причина не установлена: охранник не говорит, какое ожидание не дождалось (`window_mode`, выход владельца или второй запуск).
Первый шаг — назвать ожидание в сообщении охранника и повторить полный `make check` несколько раз.
