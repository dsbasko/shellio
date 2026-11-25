---
worth: yes
where: Makefile:22
added: 2026-11-25
---
# make probe-resources без RUN получает RUN=scripts/run.sh и отказывает

Переменная `RUN := scripts/run.sh` в `Makefile` задаёт путь сценария запуска владельца. Рецепт `probe-resources` передаёт пробе `RUN="$(RUN)"` как идентификатор прогона. Без `RUN=<run-id>` в командной строке подставляется значение переменной сценария: `make -n probe-resources` печатает `POINTS="" RUN="scripts/run.sh" spikes/probe-resource-accounting/run.sh scenarios`.

`prepare_run` в `spikes/probe-resource-accounting/run.sh` берёт `RUN_ID="${RUN:-}"` и на значении с `/` выходит с «RUN «scripts/run.sh» должен быть именем каталога». Новый прогон без явного `RUN` из цели не начинается.

Проверено 2026-11-25 сухим прогоном цели и чтением `prepare_run`, сама проба не запускалась.

Нужно развести имена: переименовать переменную сценария запуска в `Makefile` или передавать пробе идентификатор прогона из переменной с другим именем.
