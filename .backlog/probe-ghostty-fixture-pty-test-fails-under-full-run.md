---
worth: maybe
where: spikes/probe-ghostty-surface/src/fixture.rs:2138
added: 2026-11-26
---
# Тест фикстуры пробы ghostty на настоящем PTY изредка падает в полном прогоне

`fixture::tests::fixture_reports_ready_reads_replies_and_winch_on_a_real_pty` упал на проверке `ready.tty.is_some_and(|tty| tty.starts_with("/dev/tty"))`.
Наблюдение одно: `cargo test --workspace --locked --no-fail-fast` 2026-11-26, журнал `target/task8/test3a-all.log:989`, остальные 291 тест цели прошли.
Отдельно тот же тест прошёл три раза подряд (`cargo test -p probe-ghostty-surface --bin probe-ghostty-surface fixture_reports_ready`), в двух прогонах `make check` того же дня цель прошла целиком (292 теста).

Причина не установлена: значение `ready.tty` в сообщении падения не печатается, поэтому неизвестно, было ли имя пустым или другого вида.
Первый шаг — вывести значение в сообщении проверки и повторить полный прогон несколько раз.

Второе наблюдение: `make check` 2026-11-26, журнал `target/task9/check3b1-first.log:997`, та же проверка, остальные 291 тест цели прошли; повтор `make check` без изменений прошёл целиком.

Второе наблюдение 2026-11-26: `make check` после рубежа `make e2e` задачи 10 плана `navigation-keymap`, журнал `target/task10/check15.log:995`, та же проверка, 291 тест цели прошёл; повтор `make check` без правок прошёл, код 0 (`target/task10/check16.log`).

Наблюдение 2026-11-27: `make check` после рубежа `make e2e` среза 6 задачи 4 плана `command-overlays`, журнал `target/task4-e2e/check.log:874`, та же проверка, 291 тест цели прошёл.
