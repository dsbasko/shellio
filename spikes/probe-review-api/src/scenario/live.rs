//! Сценарии живых столбцов. Задача 12 даёт только `capabilities` в столбце `gitlab_ce`: хост
//! поднимает и настраивает `run.sh` (`gitlab/up.sh`, `--gitlab-ce-provision`), процесс сценария
//! получает описание хоста файлом и токены ролей окружением. Остальные строки и облачные хосты —
//! задача 13.
//!
//! Итог `capabilities` на `gitlab_ce` (ожидание — в отчёте до прогона):
//! - `pass` — возможности ревьюера и автора прочитаны адаптером через сторожа записи, факты стенда
//!   совпали: версия по `GET /metadata` — версия закреплённого тега, редакция CE, проект
//!   приватный, роль ревьюера Developer, записей нет;
//! - расхождение с фактами стенда, отказ транспорта, 5xx и отказ сторожа — ошибка исполнения;
//! - прочий отказ чтения (ошибка GraphQL, ответ не той формы, 4xx) — `fail`: возможности не
//!   определяются чтением;
//! - наличие `mergeRequestRequestChanges` в схеме и роль автора — наблюдения, не условие итога.
//!
//! ⚠️ Повтор после первого прогона: форма ответа introspection (`__type` или `__schema`) и размер
//! тела записываются в заметки клетки для запроса адаптера с `variables` и для дополнительного
//! запроса ревьюера с литералом `__type(name: "Mutation")` без `variables`. Это наблюдения, не
//! условие итога; отказ транспорта и 5xx дополнительного запроса — ошибка исполнения, как у
//! остальных чтений.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Context;
use crate::github::GitHub;
use crate::gitlab::capabilities::{self as gl_caps, IntrospectionReply};
use crate::gitlab::{ApiError, Capabilities, DEVELOPER, GitLab};
use crate::live::github::GitHubRepo;
use crate::live::gitlab::GitLabRepo;
use crate::live::guard::{GuardError, GuardTarget, Guarded, WriteScope};
use crate::live::keychain::Keychain;
use crate::live::lifecycle::{Cleanup, Roles, StepError, Swept, sweep_stale};
use crate::live::provision::{self, CeHost, CeTokens, Pin};
use crate::live::repo::{LiveError, LiveRepo};
use crate::live::sandbox::{self, Sandbox, SandboxError};
use crate::live::{Preflight, credential, preflight};
use crate::model::{Provider, ReviewTarget};
use crate::results::{Column, Execution, Host, ResultFile, Status};
use crate::scrub::{LABEL_AUTHOR, LABEL_REVIEWER, Scrubber};
use crate::transport::http::{HttpConfig, HttpTransport, Secret};
use crate::transport::{Trace, Traced, Transport};

use super::Scenario;

/// Причина `not_run` каждой клетки столбца `ghes`.
pub const GHES_REASON: &str = "нет инсталляции GitHub Enterprise Server, передача P4.1";

/// Откуда живой процесс берёт предусловия облачных столбцов. Тесты подставляют файл песочниц и
/// команду `security`, чтобы не читать настоящие `~/.config` и Keychain.
pub struct LiveEnv {
    /// Путь `sandbox.toml`; `None` — файла нет.
    pub sandbox: Option<PathBuf>,
    pub keychain: Keychain,
}

impl LiveEnv {
    pub fn system() -> LiveEnv {
        LiveEnv {
            sandbox: sandbox::default_path(),
            keychain: Keychain::system(),
        }
    }
}

pub fn run(scenario: Scenario, ctx: &Context, result: &mut ResultFile) {
    run_in(scenario, ctx, result, LiveEnv::system);
}

/// Клетка живого столбца; предусловия облачного хоста читаются только у облачного столбца.
pub fn run_in(
    scenario: Scenario,
    ctx: &Context,
    result: &mut ResultFile,
    env: impl FnOnce() -> LiveEnv,
) {
    if !scenario.columns().contains(&ctx.column) {
        result.execution = Execution::error(format!(
            "сценарий {} не идёт в столбце {}",
            scenario.name(),
            ctx.column.name()
        ));
        return;
    }
    match ctx.column {
        Column::Fake => {
            result.execution = Execution::error("столбец fake не живой".to_owned());
        }
        Column::Ghes => {
            result.status = Status::NotRun;
            result.reason = GHES_REASON.to_owned();
        }
        Column::GithubCloud | Column::GitlabCloud => cloud(scenario, ctx, result, &env()),
        Column::GitlabCe => match scenario {
            Scenario::Capabilities => capabilities_ce(result),
            _ => not_implemented(scenario, ctx, result),
        },
    }
}

fn not_implemented(scenario: Scenario, ctx: &Context, result: &mut ResultFile) {
    result.execution = Execution::error(format!(
        "сценарий {} в столбце {} не реализован",
        scenario.name(),
        ctx.column.name()
    ));
}

/// Предусловия облачного столбца: файл песочниц, хост, токены обеих ролей. Без них клетка —
/// `not_run` с причиной, испорченный файл — ошибка исполнения.
fn cloud(scenario: Scenario, ctx: &Context, result: &mut ResultFile, env: &LiveEnv) {
    let sandbox = match &env.sandbox {
        Some(path) => Sandbox::load(path),
        None => Err(SandboxError::Missing),
    };
    let host = match preflight(ctx.column, sandbox, &env.keychain) {
        Preflight::Ready(host) => host,
        Preflight::NotRun(reason) => {
            result.status = Status::NotRun;
            result.reason = reason;
            return;
        }
        Preflight::Error(err) => {
            result.execution = Execution::error(err);
            return;
        }
    };
    let (author_secret, reviewer_secret) = match host
        .accounts
        .author()
        .and_then(|author| Ok((author, host.accounts.reviewer()?)))
    {
        Ok(secrets) => secrets,
        Err(reason) => {
            result.status = Status::NotRun;
            result.reason = reason;
            return;
        }
    };
    let mut scrubber = Scrubber::new();
    host.accounts.register(&mut scrubber);
    scrubber.add_repo(&host.host.repo);
    let trace = Trace::new();
    let guarded = |secret: &Secret, actor: &'static str| {
        let http = HttpTransport::new(
            HttpConfig::default(),
            Some(credential(ctx.column, secret.clone())),
        );
        Guarded::new(
            Traced::new(http, trace.clone(), actor),
            host.target.clone(),
            host.scope.clone(),
        )
    };
    let author = guarded(author_secret, "author");
    let reviewer = guarded(reviewer_secret, "reviewer");
    let runs_root = ctx.out.parent().and_then(Path::parent);
    let opened = open_roles(
        ctx.column,
        &author,
        &reviewer,
        runs_root.map(|root| (root, ctx.out.as_path())),
        &mut scrubber,
        result,
    );
    result.trace = trace
        .entries(&scrubber)
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    let guards = [("author", &author), ("reviewer", &reviewer)];
    let mut errors = guard_errors(&guards);
    if let Err(verdict) = opened {
        if errors.is_empty() {
            verdict.apply(result, &scrubber);
            return;
        }
        errors.push(format!("{verdict:?}"));
    }
    if !errors.is_empty() {
        Verdict::Error(errors.join("; ")).apply(result, &scrubber);
        return;
    }
    if let Some(denied) = guard_denied(&guards) {
        denied.apply(result, &scrubber);
        return;
    }
    not_implemented(scenario, ctx, result);
}

/// Роли живой клетки до её записей: аккаунты обеих ролей (логин и отображаемое имя) — в
/// очиститель, затем уборка по журналам прежних процессов столбца (`runs`: корень прогонов и
/// свой каталог процесса). Сценарии строк продолжают с теми же сторожами.
pub fn open_roles<T: Transport>(
    column: Column,
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    runs: Option<(&Path, &Path)>,
    scrubber: &mut Scrubber,
    result: &mut ResultFile,
) -> Result<(), Verdict> {
    let author_repo = live_repo(author).map_err(Verdict::Error)?;
    let reviewer_repo = live_repo(reviewer).map_err(Verdict::Error)?;
    for (repo, label, role) in [
        (&author_repo, LABEL_AUTHOR, "автора"),
        (&reviewer_repo, LABEL_REVIEWER, "ревьюера"),
    ] {
        match repo.whoami() {
            Ok(account) => account.register(scrubber, label),
            Err(LiveError::Status {
                status: status @ (401 | 403),
                ..
            }) => {
                return Err(Verdict::NotRun(format!(
                    "токен без прав: аккаунт {role} не прочитан, код {status}"
                )));
            }
            Err(err) => return Err(Verdict::Error(format!("аккаунт {role}: {err}"))),
        }
    }
    if let Some((root, own)) = runs {
        let roles = Roles {
            author: author_repo.as_ref(),
            reviewer: Some(reviewer_repo.as_ref()),
        };
        let swept = sweep_stale(&roles, root, own, column, author.target());
        record_swept(result, &swept, scrubber);
    }
    Ok(())
}

/// Хост и токены из окружения процесса, которое задаёт `run.sh`.
fn prepared() -> Result<(CeHost, CeTokens, Pin), String> {
    let path = std::env::var_os(provision::ENV_HOST)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("предусловие: не задана {}", provision::ENV_HOST))?;
    let host = CeHost::load(std::path::Path::new(&path))?;
    let tokens = std::env::var(provision::ENV_TOKENS)
        .map_err(|_| format!("предусловие: не задана {}", provision::ENV_TOKENS))
        .and_then(|text| CeTokens::parse(&text))?;
    Ok((host, tokens, Pin::embedded()?))
}

/// Очиститель столбца: токены, адрес, проект, логины и отображаемые имена ролей.
pub fn ce_scrubber(host: &CeHost, tokens: &CeTokens) -> Scrubber {
    let mut scrubber = Scrubber::new();
    scrubber
        .add_token(tokens.author.expose())
        .add_token(tokens.reviewer.expose())
        .add_gitlab_ce(host.address())
        .add_repo(&host.project)
        .add_login(&host.author.username, LABEL_AUTHOR)
        .add_login(&host.author.name, LABEL_AUTHOR)
        .add_login(&host.reviewer.username, LABEL_REVIEWER)
        .add_login(&host.reviewer.name, LABEL_REVIEWER);
    if let Some(web_host) = &host.web_host {
        scrubber.add_gitlab_ce(web_host);
    }
    scrubber
}

/// Запрос introspection `__type` на GraphQL: `Some(true)` — имя в `variables`, `Some(false)` —
/// литерал без `variables`.
pub fn introspection_request(request: &crate::transport::Request) -> Option<bool> {
    let body = request.body_json()?;
    let query = body.get("query")?.as_str()?;
    if !query.contains("__type") {
        return None;
    }
    Some(body.get("variables").is_some_and(|v| !v.is_null()))
}

/// Заметка клетки о форме ответа introspection.
pub fn describe_reply(how: &str, reply: &IntrospectionReply) -> String {
    format!(
        "наблюдение: introspection {how} — ответ {}, {} байт, mergeRequestRequestChanges {}",
        reply.form,
        reply.bytes,
        match reply.request_changes {
            Some(true) => "есть",
            Some(false) => "нет",
            None => "не определяется",
        }
    )
}

/// Заметки клетки до `result.json`: тексты ответов хоста (`ApiError`) несут адреса и имена.
pub fn scrub_notes(scrubber: &Scrubber, notes: &mut [String]) {
    for note in notes {
        *note = scrubber.scrub_text(note);
    }
}

/// Причина `not_run` ветви, которой нужен серверный отказ: живой хост его не даёт.
pub const INJECTION_UNAVAILABLE: &str = "инъекция недоступна на живом хосте";
/// Причина `not_run` клетки GitLab, чей новый MR не заполнил `diff_refs` и версию в срок.
pub const MR_NOT_READY: &str = "MR не готов за 120 с";
/// Причина `not_run` клетки GitLab, чей MR не учёл смену head в срок.
pub const MR_HEAD_STALE: &str = "MR не обновил diff_refs за 120 с";

/// Адаптер GitLab поверх сторожа роли с адресами его цели: сторож сравнивает адрес GraphQL
/// точным равенством, другой вид того же адреса он бы не пропустил.
pub fn gitlab_adapter<'a, T: Transport>(
    guard: &'a Guarded<T>,
    review: &ReviewTarget,
) -> Result<GitLab<&'a Guarded<T>>, ApiError> {
    GitLab::new(guard, guard.target().gitlab_endpoints(), review)
}

/// Адаптер GitHub поверх сторожа роли с адресами его цели, как [`gitlab_adapter`].
pub fn github_adapter<'a, T: Transport>(
    guard: &'a Guarded<T>,
    review: &ReviewTarget,
) -> Result<GitHub<&'a Guarded<T>>, crate::github::ApiError> {
    GitHub::new(guard, guard.target().github_endpoints(), review)
}

/// Репозиторий жизненного цикла поверх того же сторожа, что и адаптер ревью роли.
pub fn live_repo<'a, T: Transport>(
    guard: &'a Guarded<T>,
) -> Result<Box<dyn LiveRepo + 'a>, String> {
    match guard.target().provider {
        Provider::GitHub => GitHubRepo::new(guard).map(|repo| Box::new(repo) as Box<dyn LiveRepo>),
        Provider::GitLab => GitLabRepo::new(guard).map(|repo| Box::new(repo) as Box<dyn LiveRepo>),
    }
}

/// Отказ сторожа каждой роли: причина проверки и записи, которые он не пропустил. Любая строка
/// — ошибка исполнения клетки.
pub fn guard_errors<T: Transport>(guards: &[(&str, &Guarded<T>)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (actor, guard) in guards {
        match guard.refusal() {
            // Нет доступа к репозиторию — предусловие ([`guard_denied`]); записи после такого отказа
            // не пропущены именно из-за него.
            Some(GuardError::Denied(_)) => continue,
            Some(refusal) => errors.push(format!("{actor}: {refusal}")),
            None => {}
        }
        let refused = guard.refused_writes();
        if !refused.is_empty() {
            errors.push(format!(
                "{actor}: сторож не пропустил {}",
                refused.join(", ")
            ));
        }
    }
    errors
}

/// Отказ сторожа роли по 401, 403 или 404 на чтение репозитория — `not_run` по [`guard_verdict`];
/// его [`guard_errors`] не перечисляет.
pub fn guard_denied<T: Transport>(guards: &[(&str, &Guarded<T>)]) -> Option<Verdict> {
    guards
        .iter()
        .find_map(|(actor, guard)| match guard.refusal() {
            Some(denied @ GuardError::Denied(_)) => match guard_verdict(&denied) {
                Verdict::NotRun(reason) => Some(Verdict::NotRun(format!("{actor}: {reason}"))),
                other => Some(other),
            },
            _ => None,
        })
}

/// Уборка клетки в `result.json` через очиститель: не убранное — `leftovers`, ошибки шагов —
/// заметки «уборка: …» (что осталось, решает чтение после уборки).
pub fn record_cleanup(result: &mut ResultFile, cleanup: &Cleanup, scrubber: &Scrubber) {
    result.leftovers.extend(
        cleanup
            .leftovers
            .iter()
            .map(|item| scrubber.scrub_text(item)),
    );
    result.notes.extend(
        cleanup
            .errors
            .iter()
            .map(|err| scrubber.scrub_text(&format!("уборка: {err}"))),
    );
}

/// Уборка по журналам прежних процессов в `result.json` через очиститель: не убранное —
/// `leftovers` с источником, нечитаемый журнал — тоже `leftovers`: что в нём, не известно.
pub fn record_swept(result: &mut ResultFile, swept: &[Swept], scrubber: &Scrubber) {
    for item in swept {
        match &item.cleanup {
            Ok(cleanup) => {
                result
                    .leftovers
                    .extend(cleanup.leftovers.iter().map(|left| {
                        scrubber.scrub_text(&format!("прежний процесс {}: {left}", item.source))
                    }));
                result.notes.extend(cleanup.errors.iter().map(|err| {
                    scrubber.scrub_text(&format!("уборка прежнего процесса {}: {err}", item.source))
                }));
                result.notes.push(scrubber.scrub_text(&format!(
                    "уборка прежнего процесса {}: убрано {}, осталось {}",
                    item.source,
                    cleanup.removed.len(),
                    cleanup.leftovers.len()
                )));
            }
            Err(err) => result.leftovers.push(scrubber.scrub_text(&format!(
                "прежний процесс {}: уборка не выполнена: {err}",
                item.source
            ))),
        }
    }
}

/// Итог клетки по отказу хоста или стенда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    NotRun(String),
    /// Возможности нет у проверенной роли или редакции: клетка несёт версию и тариф хоста.
    Absent(String),
    Fail(String),
    /// Ошибка исполнения.
    Error(String),
}

impl Verdict {
    /// Итог в `result.json` через очиститель. `absent` без версии хоста — ошибка исполнения:
    /// версию клетка записывает до итога.
    pub fn apply(self, result: &mut ResultFile, scrubber: &Scrubber) {
        match self {
            Verdict::NotRun(reason) => {
                result.status = Status::NotRun;
                result.reason = scrubber.scrub_text(&reason);
            }
            Verdict::Absent(reason) => {
                if result
                    .host
                    .version
                    .as_deref()
                    .is_none_or(|v| v.trim().is_empty())
                {
                    result.execution = Execution::error(
                        scrubber.scrub_text(&format!("absent без версии хоста: {reason}")),
                    );
                    return;
                }
                result.status = Status::Absent;
                result.reason = scrubber.scrub_text(&reason);
            }
            Verdict::Fail(reason) => {
                result.status = Status::Fail;
                result.reason = scrubber.scrub_text(&reason);
            }
            Verdict::Error(reason) => {
                result.execution = Execution::error(scrubber.scrub_text(&reason));
            }
        }
    }
}

/// Отказ сторожа: 401, 403 или 404 на чтение репозитория — предусловие (токен без прав или нет
/// песочницы), остальное — ошибка исполнения.
pub fn guard_verdict(err: &GuardError) -> Verdict {
    match err {
        GuardError::Denied(status) => Verdict::NotRun(format!(
            "токен без прав или нет репозитория: код {status} на чтение репозитория сторожем"
        )),
        other => Verdict::Error(other.to_string()),
    }
}

/// Шаг жизненного цикла клетки: отказ сторожа по [`guard_verdict`], 401 и 403 — `not_run`
/// «токен без прав», остальное — ошибка исполнения.
pub fn step_verdict(err: &StepError) -> Verdict {
    match &err.error {
        LiveError::Guard(guard) => match guard_verdict(guard) {
            Verdict::NotRun(reason) => Verdict::NotRun(format!("{}: {reason}", err.step)),
            other => other,
        },
        LiveError::Status {
            status: 401 | 403, ..
        } => Verdict::NotRun(format!("токен без прав: {err}")),
        _ => Verdict::Error(err.to_string()),
    }
}

/// Признак тарифа или лицензии в тексте отказа GitLab.
pub fn mentions_tier(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "licens",
        "premium",
        "ultimate",
        "tier",
        "subscription",
        "paid plan",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// Отказ `approve` GitLab: по правам или тарифу — `absent`, 409 — наблюдение для оракула
/// клетки (`fail` по умолчанию: вызывающий решает по ветви), отказ транспорта и 5xx — ошибка.
pub fn approve_verdict(err: &ApiError) -> Verdict {
    match err {
        ApiError::Absent(what) => Verdict::Absent(format!("approve: нет возможности: {what}")),
        ApiError::NotPermitted {
            access_level,
            required,
        } => Verdict::Absent(format!("approve: роль {access_level:?} ниже {required}")),
        ApiError::Status {
            status: 401 | 403,
            message,
        } => Verdict::Absent(format!("approve: отказ по правам или тарифу: {message}")),
        ApiError::Status { status, message } if mentions_tier(message) => {
            Verdict::Absent(format!("approve: код {status}, признак тарифа: {message}"))
        }
        other => read_or_fail("approve", other),
    }
}

/// Отказ `mergeRequestRequestChanges`: признак тарифа или лицензии и отсутствие в схеме —
/// `absent`, другой отказ — `fail` с текстом ответа, отказ транспорта и 5xx — ошибка.
pub fn request_changes_verdict(err: &ApiError) -> Verdict {
    let text = err.to_string();
    match err {
        ApiError::Absent(what) => Verdict::Absent(format!("mergeRequestRequestChanges: {what}")),
        ApiError::Graphql(_)
        | ApiError::Payload(_)
        | ApiError::Unavailable(_)
        | ApiError::Status { .. }
            if mentions_tier(&text) =>
        {
            Verdict::Absent(format!(
                "mergeRequestRequestChanges: признак тарифа: {text}"
            ))
        }
        other => read_or_fail("mergeRequestRequestChanges", other),
    }
}

/// Отказ действия, не опознанный как отсутствие возможности: явный лимит — `not_run` «лимит»
/// (запись не принята, повтора с ожиданием нет), неподтверждённое чтение итога — по ошибке
/// чтения, прочее — по [`classify_read`].
fn read_or_fail(what: &str, err: &ApiError) -> Verdict {
    match err {
        ApiError::RateLimited { status, detail } => {
            return Verdict::NotRun(format!("лимит: {what}: код {status}: {detail}"));
        }
        ApiError::Unverified(inner) => {
            return match read_or_fail(what, inner) {
                Verdict::Fail(reason) => {
                    Verdict::Fail(format!("итог записи не прочитан: {reason}"))
                }
                Verdict::Error(reason) => {
                    Verdict::Error(format!("итог записи не прочитан: {reason}"))
                }
                other => other,
            };
        }
        _ => {}
    }
    match classify_read(err) {
        Ok(observed) => Verdict::Fail(format!("{what}: {observed}")),
        Err(stand) => Verdict::Error(format!("{what}: {stand}")),
    }
}

/// Как читается отказ возможностей: ошибка стенда или наблюдение.
pub fn classify_read(err: &ApiError) -> Result<String, String> {
    match err {
        ApiError::NotSent(_) | ApiError::Transport(_) => Err(err.to_string()),
        ApiError::Status { status, .. } if *status >= 500 => Err(err.to_string()),
        _ => Ok(err.to_string()),
    }
}

/// Факты стенда, с которыми сверяются возможности ревьюера.
pub fn stand_mismatches(caps: &Capabilities, host: &CeHost, pin: &Pin) -> Vec<String> {
    let mut problems = Vec::new();
    if !pin.matches_version(&caps.host.version) {
        problems.push(format!(
            "версия {} не совпадает с закреплённой {}",
            caps.host.version,
            pin.version()
        ));
    }
    if caps.host.enterprise != Some(false) {
        problems.push(format!(
            "редакция не CE: enterprise {:?}",
            caps.host.enterprise
        ));
    }
    if !caps.private() {
        problems.push(format!("проект не приватный: {}", caps.visibility));
    }
    if caps.access_level != Some(DEVELOPER) {
        problems.push(format!(
            "роль ревьюера {:?}, настроена Developer",
            caps.access_level
        ));
    }
    if caps.username != host.reviewer.username {
        problems.push("токен ревьюера читает другого пользователя".to_owned());
    }
    problems
}

fn capabilities_ce(result: &mut ResultFile) {
    let (host, tokens, pin) = match prepared() {
        Ok(prepared) => prepared,
        Err(err) => {
            result.execution = Execution::error(err);
            return;
        }
    };
    let scrubber = ce_scrubber(&host, &tokens);
    let endpoints = host.endpoints();
    let target = GuardTarget::gitlab(&endpoints, &host.project);
    let review = ReviewTarget {
        provider: Provider::GitLab,
        host: host.url.clone(),
        repo: host.project.clone(),
        number: 0,
    };
    let trace = Trace::new();
    let guarded = |secret: &Secret, actor: &'static str| {
        let http = HttpTransport::new(
            HttpConfig::default(),
            Some(credential(Column::GitlabCe, secret.clone())),
        );
        Guarded::new(
            Traced::new(http, trace.clone(), actor),
            target.clone(),
            WriteScope::Loopback,
        )
    };
    let reviewer = guarded(&tokens.reviewer, "reviewer");
    let author = guarded(&tokens.author, "author");
    let read = |transport: &Guarded<Traced<HttpTransport>>| {
        gitlab_adapter(transport, &review).and_then(|gl| gl.capabilities())
    };
    let reviewer_caps = read(&reviewer);
    let author_caps = read(&author);
    let literal = gitlab_adapter(&reviewer, &review).and_then(|gl| gl.introspect_literal());

    let mut errors = guard_errors(&[("reviewer", &reviewer), ("author", &author)]);
    let writes: Vec<String> = trace
        .calls()
        .iter()
        .filter(|call| call.request.method.is_write() && call.request.url != endpoints.graphql)
        .map(|call| format!("{} {}", call.request.method, call.request.path()))
        .collect();
    if !writes.is_empty() {
        errors.push(format!(
            "записи при чтении возможностей: {}",
            writes.join(", ")
        ));
    }

    let mut failed = Vec::new();
    let mut state = serde_json::Map::new();
    for (actor, caps) in [("reviewer", &reviewer_caps), ("author", &author_caps)] {
        match caps {
            Ok(caps) => {
                state.insert(actor.to_owned(), caps.to_json());
            }
            Err(err) => match classify_read(err) {
                Ok(observed) => failed.push(format!("{actor}: {observed}")),
                Err(stand) => errors.push(format!("{actor}: {stand}")),
            },
        }
    }
    if let Ok(caps) = &reviewer_caps {
        errors.extend(stand_mismatches(caps, &host, &pin));
    }
    let literal_note = match &literal {
        Ok(reply) => Some(describe_reply("с литералом без variables", reply)),
        Err(err) => match classify_read(err) {
            Ok(observed) => Some(format!(
                "наблюдение: introspection ревьюера с литералом без variables не прочитана: \
{observed}"
            )),
            Err(stand) => {
                errors.push(format!("reviewer: introspection с литералом: {stand}"));
                None
            }
        },
    };

    result.trace = trace
        .entries(&scrubber)
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    result.calls = trace
        .counts(&scrubber)
        .into_iter()
        .map(|(key, count)| (key, count as u64))
        .collect();
    result.server_before = Some(scrubber.scrub_json(&Value::Object(state)));
    result.host = Host {
        version: Some(format!(
            "GitLab {} (CE, образ {})",
            reviewer_caps
                .as_ref()
                .map_or(host.version.as_str(), |caps| caps.host.version.as_str()),
            host.tag
        )),
        tier: reviewer_caps
            .as_ref()
            .ok()
            .and_then(|caps| caps.plan.clone()),
    };
    for call in trace.calls() {
        let Some(variables) = introspection_request(&call.request) else {
            continue;
        };
        if !variables {
            continue;
        }
        if let Ok(response) = &call.outcome {
            let reply = response
                .body_json()
                .map(|body| gl_caps::reply_of(&body, response.body.len()))
                .unwrap_or(IntrospectionReply {
                    form: "не JSON",
                    bytes: response.body.len(),
                    request_changes: None,
                });
            result.notes.push(format!(
                "{} ({})",
                describe_reply("с variables", &reply),
                call.actor
            ));
        }
    }
    result.notes.extend(literal_note);
    result.notes.push(format!(
        "токен root настройки: {}",
        match host.root_token.as_str() {
            "password_grant" => "OAuth password grant",
            "rails_runner" => "gitlab-rails runner (password grant отклонён)",
            other => other,
        }
    ));
    if let Ok(caps) = &reviewer_caps {
        result.notes.push(format!(
            "наблюдение: mergeRequestRequestChanges в схеме — {} (ответ {}); тариф {}",
            if caps.request_changes() {
                "есть"
            } else {
                "нет"
            },
            caps.introspection,
            match (&caps.plan, caps.plan_source) {
                (Some(plan), Some(source)) => format!("{plan} (из {source})"),
                (Some(plan), None) => plan.clone(),
                (None, _) => "не виден токену".to_owned(),
            }
        ));
    }
    if let Ok(caps) = &author_caps {
        result.notes.push(format!(
            "наблюдение: роль автора в своём проекте {:?}",
            caps.access_level
        ));
    }
    if result.trace.is_empty() {
        errors.push("нет следа вызовов".to_owned());
    }
    scrub_notes(&scrubber, &mut result.notes);

    if !errors.is_empty() {
        result.execution = Execution::error(scrubber.scrub_text(&errors.join("; ")));
        return;
    }
    if !failed.is_empty() {
        result.status = Status::Fail;
        result.reason = scrubber.scrub_text(&format!(
            "возможности не определились чтением: {}",
            failed.join("; ")
        ));
        return;
    }
    if let Ok(caps) = &reviewer_caps {
        result.status = Status::Pass;
        result.reason = format!(
            "возможности прочитаны через сторожа без записей: GitLab {} CE, проект приватный, \
роль ревьюера Developer, mergeRequestRequestChanges в схеме — {}",
            caps.host.version,
            if caps.request_changes() {
                "есть"
            } else {
                "нет"
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::{self, HostVersion};
    use crate::live::provision::CeAccount;
    use crate::transport::Failure;

    fn host() -> CeHost {
        CeHost {
            schema: 1,
            url: "http://127.0.0.1:55001".to_owned(),
            image: "gitlab/gitlab-ce@sha256:x".to_owned(),
            tag: "19.6.1-ce.0".to_owned(),
            version: "19.6.1".to_owned(),
            revision: None,
            enterprise: Some(false),
            project: "probe-author/shellio-review-sandbox".to_owned(),
            default_branch: "main".to_owned(),
            author: CeAccount {
                id: 2,
                username: "probe-author".to_owned(),
                name: "Probe Author".to_owned(),
            },
            reviewer: CeAccount {
                id: 3,
                username: "probe-reviewer".to_owned(),
                name: "Probe Reviewer".to_owned(),
            },
            root_token: "password_grant".to_owned(),
            web_host: Some("95b8b759b74e".to_owned()),
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            username: "probe-reviewer".to_owned(),
            host: HostVersion {
                version: "19.6.1".to_owned(),
                revision: None,
                enterprise: Some(false),
                source: "metadata",
            },
            plan: None,
            plan_source: None,
            visibility: "private".to_owned(),
            access_level: Some(DEVELOPER),
            mutations: vec![gitlab::capabilities::REQUEST_CHANGES.to_owned()],
            introspection: gitlab::capabilities::FORM_TYPE,
        }
    }

    fn pin() -> Result<Pin, String> {
        Pin::embedded()
    }

    #[test]
    fn provisioned_facts_match() -> Result<(), String> {
        let pin = pin()?;
        let mut host = host();
        let mut caps = caps();
        caps.host.version = pin.version().to_owned();
        host.version = pin.version().to_owned();
        assert!(stand_mismatches(&caps, &host, &pin).is_empty());
        caps.mutations.clear();
        assert!(
            stand_mismatches(&caps, &host, &pin).is_empty(),
            "мутация — наблюдение, не факт стенда"
        );
        Ok(())
    }

    #[test]
    fn stand_mismatches_are_listed() -> Result<(), String> {
        let pin = pin()?;
        let mut caps = caps();
        caps.host.version = "18.0.0".to_owned();
        caps.host.enterprise = Some(true);
        caps.visibility = "internal".to_owned();
        caps.access_level = Some(20);
        caps.username = "someone".to_owned();
        assert_eq!(stand_mismatches(&caps, &host(), &pin).len(), 5);
        Ok(())
    }

    #[test]
    fn introspection_requests_and_replies_are_described() {
        use crate::transport::{Method, Request};
        use serde_json::json;
        let url = "http://127.0.0.1:1/api/graphql";
        let with_variables = Request::new(Method::Post, url)
            .json(&json!({"query": "query($name: String!) { __type(name: $name) { fields { name } } }", "variables": {"name": "Mutation"}}));
        let literal =
            Request::new(Method::Post, url).json(&json!({"query": gl_caps::INTROSPECT_LITERAL}));
        let other =
            Request::new(Method::Post, url).json(&json!({"query": "{ currentUser { id } }"}));
        assert_eq!(introspection_request(&with_variables), Some(true));
        assert_eq!(introspection_request(&literal), Some(false));
        assert_eq!(introspection_request(&other), None);
        let note = describe_reply(
            "с литералом без variables",
            &IntrospectionReply {
                form: gl_caps::FORM_SCHEMA,
                bytes: 9_400_000,
                request_changes: Some(true),
            },
        );
        assert!(
            note.contains("__schema") && note.contains("9400000 байт") && note.contains("есть"),
            "{note}"
        );
    }

    #[test]
    fn read_failures_split_into_stand_errors_and_observations() {
        let stand = [
            ApiError::NotSent(Failure::not_sent("x")),
            ApiError::Transport(Failure::timeout("x")),
            ApiError::Status {
                status: 502,
                message: String::new(),
            },
        ];
        for err in stand {
            assert!(classify_read(&err).is_err(), "{err:?}");
        }
        let observed = [
            ApiError::Unexpected("__type(Mutation) без fields".to_owned()),
            ApiError::Status {
                status: 403,
                message: String::new(),
            },
            ApiError::Unavailable("x".to_owned()),
        ];
        for err in observed {
            assert!(classify_read(&err).is_ok(), "{err:?}");
        }
    }

    #[test]
    fn scrubber_hides_tokens_address_project_and_names() {
        let tokens = CeTokens {
            author: Secret::new("glpat-authorsecret1"),
            reviewer: Secret::new("glpat-reviewersecret2"),
        };
        let scrubber = ce_scrubber(&host(), &tokens);
        let text = scrubber.scrub_text(
            "http://127.0.0.1:55001/api/v4/projects/probe-author%2Fshellio-review-sandbox \
glpat-authorsecret1 glpat-reviewersecret2 Probe Author probe-reviewer Probe Reviewer \
http://95b8b759b74e/x git@95b8b759b74e:x.git",
        );
        for secret in [
            "55001",
            "95b8b759b74e",
            "probe-author",
            "probe-reviewer",
            "Probe Author",
            "Probe Reviewer",
            "secret",
        ] {
            assert!(!text.contains(secret), "{secret} в «{text}»");
        }
        assert!(text.contains("<gitlab-ce>"), "{text}");
    }

    fn ctx(column: Column, dir: &std::path::Path) -> Context {
        Context {
            column,
            out: dir.to_path_buf(),
            state_dir: dir.to_path_buf(),
        }
    }

    fn untouched() -> LiveEnv {
        panic!("предусловия облачного хоста не должны читаться")
    }

    /// Подставной `security`: токен есть только у `author`, остальные — код 44.
    fn stub_env(dir: &std::path::Path, sandbox: Option<&str>) -> Result<LiveEnv, String> {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("security");
        std::fs::write(
            &program,
            "#!/bin/sh\ncase \"$5\" in\n  github-author|gitlab-author) printf 'tok-author\\n' ;;\n  *) exit 44 ;;\nesac\n",
        )
        .map_err(|e| e.to_string())?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let path = dir.join("sandbox.toml");
        if let Some(text) = sandbox {
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
        }
        Ok(LiveEnv {
            sandbox: Some(path),
            keychain: Keychain::with_program(&program, std::time::Duration::from_secs(10)),
        })
    }

    fn cell(scenario: Scenario, column: Column, env: impl FnOnce() -> LiveEnv) -> ResultFile {
        let dir = std::env::temp_dir();
        let mut result = ResultFile::new("s", "b", scenario.name(), column);
        run_in(scenario, &ctx(column, &dir), &mut result, env);
        result
    }

    #[test]
    fn ghes_cells_are_not_run_without_reading_preconditions() {
        for scenario in Scenario::ALL
            .into_iter()
            .filter(|s| s.columns().contains(&Column::Ghes))
        {
            let result = cell(scenario, Column::Ghes, untouched);
            assert!(result.execution.is_ok(), "{}", scenario.name());
            assert_eq!(result.status, Status::NotRun);
            assert_eq!(result.reason, GHES_REASON);
            assert!(result.trace.is_empty());
            assert!(result.problems().is_empty(), "{:?}", result.problems());
        }
    }

    #[test]
    fn scenario_outside_its_columns_is_execution_error_before_preconditions() {
        let result = cell(Scenario::Partial, Column::GithubCloud, untouched);
        assert!(!result.execution.is_ok());
        let result = cell(Scenario::GhAnchors, Column::GitlabCloud, untouched);
        assert!(!result.execution.is_ok());
    }

    #[test]
    fn cloud_cells_without_preconditions_are_not_run() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let missing = cell(Scenario::GhLocalCreate, Column::GithubCloud, || {
            stub_env(dir.path(), None).unwrap_or_else(|_| untouched())
        });
        assert!(missing.execution.is_ok());
        assert_eq!(missing.status, Status::NotRun);
        assert_eq!(missing.reason, "нет sandbox.toml");

        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let env = stub_env(dir.path(), Some("[gitlab_cloud]\nrepo = \"g/p\"\n"))?;
        let no_host = cell(Scenario::GhViewed, Column::GithubCloud, || env);
        assert_eq!(no_host.status, Status::NotRun);
        assert_eq!(no_host.reason, "нет хоста github_cloud в sandbox.toml");

        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let env = stub_env(dir.path(), Some("[gitlab_cloud]\nrepo = \"g/p\"\n"))?;
        let no_reviewer = cell(Scenario::GlDraftPublish, Column::GitlabCloud, || env);
        assert!(no_reviewer.execution.is_ok());
        assert_eq!(no_reviewer.status, Status::NotRun);
        assert!(
            no_reviewer.reason.starts_with("нет ревьюера: ")
                && no_reviewer.reason.contains("gitlab-reviewer"),
            "{}",
            no_reviewer.reason
        );
        assert!(no_reviewer.problems().is_empty());

        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let env = stub_env(
            dir.path(),
            Some("[gitlab_cloud]\nrepo = \"g/p\"\nauthor = \"nobody\"\n"),
        )?;
        let no_author = cell(Scenario::Capabilities, Column::GitlabCloud, || env);
        assert!(
            no_author.reason.starts_with("нет автора: "),
            "{}",
            no_author.reason
        );
        Ok(())
    }

    #[test]
    fn broken_sandbox_is_execution_error() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let env = stub_env(dir.path(), Some("[github_cloud]\nrepo = 5\n"))?;
        let result = cell(Scenario::Capabilities, Column::GithubCloud, || env);
        assert!(!result.execution.is_ok());
        Ok(())
    }

    #[test]
    fn notes_pass_through_scrubber() {
        let tokens = CeTokens {
            author: Secret::new("glpat-authorsecret1"),
            reviewer: Secret::new("glpat-reviewersecret2"),
        };
        let scrubber = ce_scrubber(&host(), &tokens);
        let mut notes = vec![format!(
            "наблюдение: introspection ревьюера с литералом без variables не прочитана: {}",
            ApiError::Status {
                status: 403,
                message: "probe-reviewer: http://127.0.0.1:55001/api/graphql".to_owned(),
            }
        )];
        scrub_notes(&scrubber, &mut notes);
        assert!(
            !notes[0].contains("probe-reviewer") && !notes[0].contains("55001"),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("<reviewer>"), "{}", notes[0]);
    }

    #[test]
    fn capabilities_without_host_description_is_execution_error() -> Result<(), String> {
        if std::env::var_os(provision::ENV_HOST).is_some() {
            return Ok(());
        }
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let ctx = Context {
            column: Column::GitlabCe,
            out: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        let mut result = ResultFile::new("s", "b", "capabilities", Column::GitlabCe);
        Scenario::Capabilities.run(&ctx, &mut result);
        assert!(!result.execution.is_ok());
        assert!(result.trace.is_empty());
        Ok(())
    }

    // --- общий сток живой клетки, сторож, очистка, итоги отказов ---

    use crate::live::lifecycle::{CellNames, LEDGER_FILE, Ledger, SWEPT_FILE, run_cell};
    use crate::transport::scripted::{Reply, Scripted};
    use crate::transport::{Method, Request};
    use serde_json::json;

    fn gh_target() -> GuardTarget {
        GuardTarget::github(
            &crate::github::Endpoints::new("https://gh.test", "https://gh.test/graphql"),
            "owner/sandbox",
        )
    }

    fn listed() -> WriteScope {
        WriteScope::Listed(vec!["owner/sandbox".to_owned()])
    }

    fn gh_repo(private: bool) -> Reply {
        Reply::json(
            200,
            &json!({"full_name": "owner/sandbox", "private": private, "default_branch": "main"}),
        )
    }

    #[test]
    fn adapters_take_endpoints_from_the_guard_target() -> Result<(), String> {
        let scripted = Scripted::new()
            .expect(Method::Get, "/repos/owner/sandbox", gh_repo(true))
            .expect(
                Method::Post,
                "/graphql",
                Reply::json(200, &json!({"data": {}})),
            );
        let guard = Guarded::new(&scripted, gh_target(), listed());
        let endpoints = guard.target().github_endpoints();
        assert_eq!(endpoints.graphql, "https://gh.test/graphql");
        guard
            .send(&Request::new(Method::Post, endpoints.graphql.clone()).json(&json!({})))
            .map_err(|f| f.to_string())?;
        assert!(guard_errors(&[("reviewer", &guard)]).is_empty());
        // Тот же адрес в другом виде сторож не пропускает: отсюда общий источник адресов.
        let other = format!("{}/", endpoints.graphql);
        assert!(
            guard
                .send(&Request::new(Method::Post, other).json(&json!({})))
                .is_err()
        );
        let errors = guard_errors(&[("reviewer", &guard)]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("reviewer: сторож не пропустил POST"),
            "{errors:?}"
        );
        scripted.finish()?;

        let ce = crate::gitlab::Endpoints::instance("http://127.0.0.1:55001");
        let target = GuardTarget::gitlab(&ce, "probe-author/shellio-review-sandbox");
        assert_eq!(target.gitlab_endpoints().rest, ce.rest);
        assert_eq!(target.gitlab_endpoints().graphql, ce.graphql);
        let review = ReviewTarget {
            provider: Provider::GitLab,
            host: "http://127.0.0.1:55001".to_owned(),
            repo: target.repo.clone(),
            number: 1,
        };
        let idle = Scripted::new();
        let guard = Guarded::new(&idle, target, WriteScope::Loopback);
        assert!(gitlab_adapter(&guard, &review).is_ok());
        assert!(live_repo(&guard).is_ok());
        idle.finish()?;
        Ok(())
    }

    #[test]
    fn guard_refusal_is_listed_for_the_cell() -> Result<(), String> {
        let scripted = Scripted::new().expect(Method::Get, "/repos/owner/sandbox", gh_repo(false));
        let guard = Guarded::new(&scripted, gh_target(), listed());
        assert!(
            guard
                .send(&Request::new(
                    Method::Post,
                    "https://gh.test/repos/owner/sandbox/pulls"
                ))
                .is_err()
        );
        let errors = guard_errors(&[("author", &guard)]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("не приватный"), "{errors:?}");
        let mut result = ResultFile::new("s", "b", "gh-viewed", Column::GithubCloud);
        Verdict::Error(errors.join("; ")).apply(&mut result, &Scrubber::new());
        assert!(!result.execution.is_ok());
        scripted.finish()?;
        Ok(())
    }

    fn cloud_scrubber() -> Scrubber {
        let mut scrubber = Scrubber::new();
        scrubber
            .add_token("ghp_secretvalue123")
            .add_repo("owner/sandbox")
            .add_login("alice", LABEL_AUTHOR)
            .add_login("Alice Liddell", LABEL_AUTHOR);
        scrubber
    }

    #[test]
    fn cleanup_leftovers_errors_and_sweeps_pass_through_scrubber() {
        let scrubber = cloud_scrubber();
        let mut result = ResultFile::new("s", "b", "gh-viewed", Column::GithubCloud);
        record_cleanup(
            &mut result,
            &Cleanup {
                removed: vec!["PR #4 закрыт".to_owned()],
                errors: vec![
                    "удаление ветки: код 403: alice has no access to owner/sandbox".to_owned(),
                ],
                leftovers: vec!["PR #4 открыт в owner/sandbox, автор Alice Liddell".to_owned()],
            },
            &scrubber,
        );
        record_swept(
            &mut result,
            &[Swept {
                source: "old/gh-viewed-github_cloud".to_owned(),
                cleanup: Err("token ghp_secretvalue123 rejected".to_owned()),
            }],
            &scrubber,
        );
        let text = format!("{:?} {:?}", result.leftovers, result.notes);
        for secret in [
            "alice",
            "Alice Liddell",
            "owner/sandbox",
            "ghp_secretvalue123",
        ] {
            assert!(
                !text.to_lowercase().contains(&secret.to_lowercase()),
                "{secret} в {text}"
            );
        }
        assert_eq!(result.leftovers.len(), 2);
        assert!(result.leftovers[1].starts_with("прежний процесс old/gh-viewed-github_cloud"));
        assert!(
            result.notes[0].starts_with("уборка: "),
            "{:?}",
            result.notes
        );
    }

    #[test]
    fn host_refusals_build_not_run_and_absent_cells() {
        let scrubber = Scrubber::new();
        assert!(matches!(
            guard_verdict(&GuardError::Denied(404)),
            Verdict::NotRun(_)
        ));
        assert!(matches!(
            guard_verdict(&GuardError::NotPrivate("private: false".to_owned())),
            Verdict::Error(_)
        ));

        let status = |status: u16, message: &str| ApiError::Status {
            status,
            message: message.to_owned(),
        };
        let absent = approve_verdict(&status(401, "401 Unauthorized"));
        assert!(matches!(absent, Verdict::Absent(_)), "{absent:?}");
        let mut with_version = ResultFile::new("s", "b", "gl-approve-sha", Column::GitlabCloud);
        with_version.host = Host {
            version: Some("GitLab 19.6.1 (CE)".to_owned()),
            tier: Some("free".to_owned()),
        };
        absent.clone().apply(&mut with_version, &scrubber);
        assert_eq!(with_version.status, Status::Absent);
        assert!(
            with_version.problems().is_empty(),
            "{:?}",
            with_version.problems()
        );
        let mut without = ResultFile::new("s", "b", "gl-approve-sha", Column::GitlabCloud);
        absent.apply(&mut without, &scrubber);
        assert!(
            !without.execution.is_ok(),
            "absent без версии — ошибка исполнения"
        );
        assert!(matches!(
            approve_verdict(&ApiError::ShaRejected("409".to_owned())),
            Verdict::Fail(_)
        ));
        assert!(matches!(
            approve_verdict(&status(502, "")),
            Verdict::Error(_)
        ));

        let licensed = ApiError::Graphql(vec![crate::gitlab::GraphqlError {
            code: None,
            message: "This feature requires a Premium license".to_owned(),
        }]);
        assert!(matches!(
            request_changes_verdict(&licensed),
            Verdict::Absent(_)
        ));
        assert!(matches!(
            request_changes_verdict(&ApiError::Absent("mergeRequestRequestChanges")),
            Verdict::Absent(_)
        ));
        let other = ApiError::Payload(vec!["Reviewer not found".to_owned()]);
        assert!(matches!(request_changes_verdict(&other), Verdict::Fail(_)));
        assert!(matches!(
            request_changes_verdict(&ApiError::Transport(crate::transport::Failure::timeout(
                "x"
            ))),
            Verdict::Error(_)
        ));

        assert!(matches!(
            approve_verdict(&ApiError::RateLimited {
                status: 429,
                detail: "Retry later".to_owned()
            }),
            Verdict::NotRun(reason) if reason.starts_with("лимит")
        ));
        let lost = ApiError::Unverified(Box::new(ApiError::Transport(
            crate::transport::Failure::timeout("x"),
        )));
        assert!(matches!(request_changes_verdict(&lost), Verdict::Error(_)));
        let lost_5xx = ApiError::Unverified(Box::new(status(503, "")));
        assert!(matches!(approve_verdict(&lost_5xx), Verdict::Error(_)));

        let mut not_run = ResultFile::new("s", "b", "gl-anchors", Column::GitlabCloud);
        Verdict::NotRun(MR_NOT_READY.to_owned()).apply(&mut not_run, &scrubber);
        assert_eq!(not_run.status, Status::NotRun);
        assert_eq!(not_run.reason, MR_NOT_READY);
    }

    #[test]
    fn denied_repository_read_opens_no_cell_and_is_not_run() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let scripted = Scripted::new().expect(
            Method::Get,
            "/repos/owner/sandbox",
            Reply::json(404, &json!({"message": "Not Found"})),
        );
        let guard = Guarded::new(&scripted, gh_target(), listed());
        let author = live_repo(&guard)?;
        let roles = Roles {
            author: author.as_ref(),
            reviewer: None,
        };
        let mut ledger = Ledger::create(&dir.path().join(LEDGER_FILE))?;
        let names = CellNames::new("run1", "gh-viewed")?;
        let run = run_cell(&roles, &mut ledger, &names, "t", |_| Ok(()));
        assert!(
            guard_errors(&[("author", &guard)]).is_empty(),
            "нет доступа — не ошибка сторожа"
        );
        assert!(
            matches!(guard_denied(&[("author", &guard)]), Some(Verdict::NotRun(reason)) if reason.starts_with("author: токен без прав"))
        );
        let open = run.open_error.ok_or("ожидалась ошибка открытия")?;
        let verdict = step_verdict(&open);
        assert!(
            matches!(&verdict, Verdict::NotRun(reason) if reason.starts_with("сторож автора: токен без прав")),
            "{verdict:?}"
        );
        assert!(scripted.delivered().iter().all(|r| !r.method.is_write()));
        scripted.finish()?;
        Ok(())
    }

    #[test]
    fn open_roles_registers_names_and_sweeps_stale_ledgers() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|e| e.to_string())?;
        let stale = root.path().join("old").join("gh-viewed-github_cloud");
        let own = root.path().join("new").join("gh-viewed-github_cloud");
        for dir in [&stale, &own] {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        Ledger::create_for(&stale.join(LEDGER_FILE), &gh_target())?;
        Ledger::create_for(&own.join(LEDGER_FILE), &gh_target())?;
        let foreign = root.path().join("old").join("gh-anchors-github_cloud");
        std::fs::create_dir_all(&foreign).map_err(|e| e.to_string())?;
        Ledger::create_for(
            &foreign.join(LEDGER_FILE),
            &GuardTarget::github(&crate::github::Endpoints::cloud(), "owner/other"),
        )?;
        let author_t = Scripted::new().expect(
            Method::Get,
            "/user",
            Reply::json(
                200,
                &json!({"login": "alice", "id": 1, "name": "Alice Liddell"}),
            ),
        );
        let reviewer_t = Scripted::new().expect(
            Method::Get,
            "/user",
            Reply::json(200, &json!({"login": "bob", "id": 2, "name": null})),
        );
        let author = Guarded::new(&author_t, gh_target(), listed());
        let reviewer = Guarded::new(&reviewer_t, gh_target(), listed());
        let mut scrubber = Scrubber::new();
        let mut result = ResultFile::new("s", "b", "gh-viewed", Column::GithubCloud);
        open_roles(
            Column::GithubCloud,
            &author,
            &reviewer,
            Some((root.path(), own.as_path())),
            &mut scrubber,
            &mut result,
        )
        .map_err(|v| format!("{v:?}"))?;
        assert_eq!(
            scrubber.scrub_text("alice Alice Liddell bob"),
            "<author> <author> <reviewer>"
        );
        assert!(
            stale.join(SWEPT_FILE).exists(),
            "пустой журнал убран и отмечен"
        );
        assert!(!own.join(SWEPT_FILE).exists(), "свой журнал не трогается");
        assert!(
            !foreign.join(SWEPT_FILE).exists(),
            "журнал другой цели не трогается"
        );
        assert_eq!(result.leftovers.len(), 1, "{:?}", result.leftovers);
        assert!(
            result.leftovers[0].starts_with(
                "прежний процесс old/gh-anchors-github_cloud: уборка не выполнена: журнал другого"
            ),
            "{:?}",
            result.leftovers
        );
        assert_eq!(
            result.notes,
            ["уборка прежнего процесса old/gh-viewed-github_cloud: убрано 0, осталось 0"]
        );
        author_t.finish()?;
        reviewer_t.finish()?;

        let denied = Scripted::new().expect(Method::Get, "/user", Reply::json(401, &json!({})));
        let idle = Scripted::new();
        let author = Guarded::new(&denied, gh_target(), listed());
        let reviewer = Guarded::new(&idle, gh_target(), listed());
        let verdict = open_roles(
            Column::GithubCloud,
            &author,
            &reviewer,
            None,
            &mut Scrubber::new(),
            &mut result,
        );
        assert!(
            matches!(&verdict, Err(Verdict::NotRun(reason)) if reason.starts_with("токен без прав")),
            "{verdict:?}"
        );
        denied.finish()?;
        idle.finish()?;
        Ok(())
    }
}
