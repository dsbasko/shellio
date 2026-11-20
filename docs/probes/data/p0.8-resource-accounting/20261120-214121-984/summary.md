| Сценарий | Точка | Клетка | Итог | Основание |
| --- | --- | --- | --- | --- |
| `arena` | — | `arena-grow` | pass | A Usage { used_bytes: 100000, capacity_bytes: 131072, leases: 1, requested_by: {} }, B Usage { used_bytes: 6000, capacity_bytes: 8192, leases: 1, requested_by: {} } |
| `arena` | — | `arena-reuse` | pass | A Usage { used_bytes: 50000, capacity_bytes: 131072, leases: 1, requested_by: {} } |
| `arena` | — | `arena-release` | pass | освобождено 65536, A после большого выделения Usage { used_bytes: 250000, capacity_bytes: 265536, leases: 1, requested_by: {} } |
| `arena` | — | `arena-git` | pass | Git Usage { used_bytes: 10000, capacity_bytes: 32768, leases: 1, requested_by: {"A", "B"} } |
| `attach-move` | — | `attach-closed` | pass | сессия из 3 процессов осталась у панели, x1 — «вышел» |
| `attach-move` | — | `attach-move` | pass | после переноса сессия та же, x2 — «свежий» |
| `churn` | — | `churn-fresh` | pass | a2: Some((Some(Measured(1311128)), Missing(Fresh { reused_pid: false }))) |
| `churn` | — | `churn-exited` | pass | c/p/c1 и a1 — «вышел» без памяти, q2 — заметка «вышел» |
| `churn` | — | `churn-partial` | pass | partial true, CPU 3/7 (доля 0.43), память 4/5, учтены [1334, 1336, 1338], ожидались [1334, 1336, 1338]; причины: PID 1335: Exited; PID 1337: Exited; PID 1339: Fresh { reused_pid: false }; PID 1340: Unread { unread: Unconfirmed { error: Exited } } |
| `churn` | — | `churn-zombie` | pass | q1 PID 1340 в списке зомби; чтение снимка Some((Err(Unconfirmed { error: Exited }), Err(Exited), Err(Exited))); запись сводки Some((Some(Missing(Unread { unread: Unconfirmed { error: Exited } })), Missing(Unread { unread: Unconfirmed { error: Exited } }))) |
| `clock-bench` | — | `clock-accuracy-clock_gettime` | pass | участков 30, вне допуска 0, отказов чтения 0, худший — System 500 мкс: часы 500.2 мкс, оракул 504.0 мкс, доля допуска 0.018 |
| `clock-bench` | — | `clock-accuracy-thread_info` | pass | участков 30, вне допуска 0, отказов чтения 0, худший — User 500 мкс: часы 499.0 мкс, оракул 502.0 мкс, доля допуска 0.014 |
| `clock-bench` | — | `clock-cost` | pass | медиана CPU на чтение: clock_gettime 92.9 нс, thread_info 406.3 нс, партий по 50000: 100 |
| `cost` | background | `cost/background` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `cost` | idle | `cost/idle` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `cost` | panels-16 | `cost/panels-16` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `cost` | pids-128 | `cost/pids-128` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `cost` | pids-32 | `cost/pids-32` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `cost` | pids-8 | `cost/pids-8` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `eperm-inject` | — | `eperm-inject` | pass | e/p/c1 PID 1425 — отказ EPERM в памяти и CPU, покрытие памяти 2/3, partial, контроль прочитан |
| `formula-cpu` | — | `formula-cpu` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `formula-mem` | — | `formula-mem-16` | pass | снимок 18137496 байт, m0 18121112 байт, разница +16384 байт, допуск 1048576, m1 18153880 байт |
| `formula-mem` | — | `formula-mem-128` | pass | снимок 135627256 байт, m0 135610872 байт, разница +16384 байт, допуск 1048576, m1 135643640 байт |
| `formula-mem` | — | `formula-mem-512` | pass | снимок 538493720 байт, m0 538477336 байт, разница +16384 байт, допуск 1048576, m1 538493720 байт |
| `formula-mem` | — | `formula-mem-sum` | pass | сводка 692258472 байт, Σ записей 692258472 байт, покрытие памяти 3/3, partial false |
| `git-shared` | — | `git-shared-group` | pass | группа «Git»: 2 процесса, память 2655024 байт, запросившие A и B |
| `git-shared` | — | `git-shared-once` | pass | каждый процесс группы учтён один раз, панелям A и B не начислен |
| `list-bench` | — | `list-fields` | fail | PID 801, proc_pidinfo ответил 527, отказы по errno {1: 273, 3: 1} |
| `list-bench` | — | `list-cost` | pass | медиана CPU потока: sysctl 129.5 мкс, proc_listallpids + proc_pidinfo 373.6 мкс, пар 100 |
| `login` | — | `login` | pass | login PID 1397 не прочитан (errno 1), ребёнок PID 1398 прочитан, покрытие памяти 2/3, partial |
| `reparent` | — | `reparent` | pass | сирота PID 1307, родитель 1, источники {Session, Group} |
| `sessions` | — | `sessions-s1` | pass | членов 5, память 6670352 байт, покрытие памяти 5/5, CPU 5/5 |
| `sessions` | — | `sessions-s2` | pass | членов 5, память 6653968 байт, покрытие памяти 5/5, CPU 5/5 |
| `sessions` | — | `sessions-foreign` | pass | PID 1240 ни в одном итоге |
| `smoke` | — | `smoke` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-blocking` | — | `span-blocking` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-busy` | — | `span-busy` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-migrate` | — | `span-migrate` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-nested` | — | `span-nested` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-parallel` | — | `span-parallel` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `span-wait` | — | `span-wait` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |
| `units` | — | `units` | not_run (шумно) | 3 попытки подряд не начаты: шумно, шумно, шумно; окон проверки фона 87 |

| Передача | Итог | Оракул | Основание |
| --- | --- | --- | --- |
| `surface-redraw` | not_run (нет поверхности) | кадры поверхности libghostty в окнах с проходами сэмплера и обновлением чисел индикатора против окон без них | проба headless и не создаёт поверхность libghostty: утверждение ADR-0011 «обновление чисел не вызывает перерисовку терминала» не наблюдается; передача P2.4, замер на продуктовом сэмплере |
| `surface-memory` | not_run (нет поверхности) | прирост `ri_phys_footprint` процесса GUI на открытую поверхность libghostty | поверхности в пробе нет; P0.3 мерил сводный `phys_footprint` процесса без приписывания ресурсам GPU: +107,5 МиБ первая поверхность, +12,6 МиБ каждая следующая (docs/probes/p0.3-ghostty-surface.md, раздел «Остаток для следующих шагов», строка P0.8); передача P2.4 |
| `surface-gpu` | not_run (нет поверхности) | ресурсы GPU поверхности (swap chain, текстуры целей рендера), приписанные поверхности записью Instruments | поверхности и записи Instruments в пробе нет; P0.3 ресурсы GPU поверхности не приписал (docs/probes/p0.3-ghostty-surface.md, раздел «Остаток для следующих шагов», строка P0.8); по ADR-0011 шрифты и Metal общие, без замера панель показывает их как «общее», а из footprint GUI в измеренной сводке ничего не вычитается; передача P2.4 |
| `surface-remainder` | not_run (нет поверхности) | остаток `ri_phys_footprint` процесса GUI над состоянием до первой поверхности после закрытия всех поверхностей, по циклам | поверхности в пробе нет; необъяснённый остаток P0.3 после циклов +24,7 МиБ разово, дальше около +3 МиБ за цикл (docs/probes/p0.3-ghostty-surface.md, раздел «Остаток для следующих шагов», строка P0.8); по ADR-0011 он «общее» и панелям не начисляется, `footprint − арены` не считается; передача P2.4 |
