Серия `t15-20261121`, сборка `21a981e31bf46b640f9f94a4eb6eebf51b7efca455d6696a7595ff91862aa7b5`.

| Строка | fake | github_cloud | gitlab_cloud | gitlab_ce | ghes |
| --- | --- | --- | --- | --- | --- |
| `gh-local-create` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `gh-pending-cycle` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `gh-pending-foreign` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `gh-anchors` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `gh-viewed` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `gl-draft-publish` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `gl-foreign-draft` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `gl-approve-sha` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `gl-request-changes` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `gl-anchors` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `head-change` | pass | — | not_run: нет sandbox.toml | fail: не прошли 4 из 13 проверок: записи ревьюера: {"POST draft_notes": 3, "PUT draft_notes/{id}/publish": 2}, ожидалось {"POST draft_notes": 3, "PUT draft_notes/{id}/publish": 3}; путь «остаться»: черновик T создан и опубликован на v1: POST draft_notes: неожиданный ответ: черновик 10 записан с другой тройкой SHA (head b5d384eb30a1ca745dda95799e8c3539417fcd1e); черновиков ревьюера после отправки: 1, ожидалось 0; автор: заметки с позициями черновиков: {"перепривязать", "сохранится"}, ожидалось {"остаться", "перепривязать", "сохранится"} (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `journal-crash` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `reconcile-ambiguous` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `reconcile-foreign` | pass | not_run: нет sandbox.toml | — | — | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |
| `reconcile-deleted` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `reconcile-paged` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `partial` | pass | — | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | — |
| `capabilities` | pass (GitHub API 2022-11-28; GitLab 17.6.0 (CE)) | not_run: нет sandbox.toml | not_run: нет sandbox.toml | pass (GitLab 19.6.1 (CE, образ 19.6.1-ce.0), free) | not_run: нет инсталляции GitHub Enterprise Server, передача P4.1 |

Самопроверка стенда, стоимость и устойчивость (в матрицу не входят):

| Сценарий | fake | github_cloud | gitlab_cloud | gitlab_ce | ghes |
| --- | --- | --- | --- | --- | --- |
| `rate-limit` | pass | — | — | — | — |
| `smoke` | pass | — | — | — | — |
| `volume` | pass | — | — | — | — |

Каталоги прогона:

- `20261121-034220-95762` (fake, завершён), уборка docker: cleaned
- `20261121-034234-96304` (live, завершён), уборка docker: cleaned
- `20261121-034606-873` (live, завершён), уборка docker: cleaned
- `20261121-034608-1000` (live, завершён), уборка docker: cleaned
- `20261121-034608-1116` (live, завершён), уборка docker: cleaned
