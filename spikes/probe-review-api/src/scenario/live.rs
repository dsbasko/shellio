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
use crate::live::repo::{Account, LiveError, LiveRepo};
use crate::live::sandbox::{self, Sandbox, SandboxError};
use crate::live::{CloudHost, Preflight, credential, preflight};
use crate::model::{Provider, ReviewTarget};
use crate::results::{Column, Execution, Host, ResultFile, Status};
use crate::scrub::{LABEL_AUTHOR, LABEL_REVIEWER, Scrubber};
use crate::transport::http::{HttpConfig, HttpTransport, Secret};
use crate::transport::{Trace, Traced, Transport};

use super::Scenario;

mod gh_cells;
mod gl_cells;

pub use gl_cells::{
    AuthorCommit, GlCx, HeadMover, Poll, ProcessClient, SECOND_FILE, SecondClient, SecondOp,
};

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
            _ => gitlab_ce(scenario, ctx, result),
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

/// Файл песочниц и запись хоста облачного столбца: нет предусловия — `not_run`, испорченный файл —
/// ошибка исполнения.
fn cloud_host(column: Column, env: &LiveEnv) -> Result<Box<CloudHost>, Verdict> {
    let sandbox = match &env.sandbox {
        Some(path) => Sandbox::load(path),
        None => Err(SandboxError::Missing),
    };
    match preflight(column, sandbox, &env.keychain) {
        Preflight::Ready(host) => Ok(host),
        Preflight::NotRun(reason) => Err(Verdict::NotRun(reason)),
        Preflight::Error(err) => Err(Verdict::Error(err)),
    }
}

/// Сторож роли облачного хоста над HTTP с меткой следа `actor`.
fn cloud_guard(
    column: Column,
    host: &CloudHost,
    secret: &Secret,
    trace: &Trace,
    actor: &'static str,
) -> Guarded<Traced<HttpTransport>> {
    let http = HttpTransport::new(
        HttpConfig::default(),
        Some(credential(column, secret.clone())),
    );
    Guarded::new(
        Traced::new(http, trace.clone(), actor),
        host.target.clone(),
        host.scope.clone(),
    )
}

/// Корень инсталляции GitLab из адреса REST цели (`<корень>/api/v4`).
pub fn gitlab_root(target: &GuardTarget) -> String {
    target
        .rest
        .strip_suffix("/api/v4")
        .unwrap_or(&target.rest)
        .to_owned()
}

/// Предусловия облачного столбца: файл песочниц, хост, токены обеих ролей. Без них клетка —
/// `not_run` с причиной, испорченный файл — ошибка исполнения.
fn cloud(scenario: Scenario, ctx: &Context, result: &mut ResultFile, env: &LiveEnv) {
    let host = match cloud_host(ctx.column, env) {
        Ok(host) => host,
        Err(verdict) => {
            verdict.apply(result, &Scrubber::new());
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
    let author = cloud_guard(ctx.column, &host, author_secret, &trace, "author");
    let reviewer = cloud_guard(ctx.column, &host, reviewer_secret, &trace, "reviewer");
    let runs_root = ctx.out.parent().and_then(Path::parent);
    let opened = open_roles(
        ctx.column,
        &author,
        &reviewer,
        runs_root.map(|root| (root, ctx.out.as_path())),
        &mut scrubber,
        result,
    );
    let gitlab_rows = ctx.column == Column::GitlabCloud && scenario != Scenario::Capabilities;
    let host_read = match &opened {
        Ok(_) if gitlab_rows => Some(gitlab_host(&reviewer)),
        _ => None,
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
    let guards = [("author", &author), ("reviewer", &reviewer)];
    let mut errors = guard_errors(&guards);
    let reviewer_account = match opened {
        Ok(account) => Some(account),
        Err(verdict) if errors.is_empty() => {
            verdict.apply(result, &scrubber);
            return;
        }
        Err(verdict) => {
            errors.push(format!("{verdict:?}"));
            None
        }
    };
    match host_read {
        Some(Ok((read, note))) => {
            result.host = read;
            result.notes.extend(note);
        }
        Some(Err(err)) => errors.push(err),
        None => {}
    }
    if !errors.is_empty() {
        Verdict::Error(errors.join("; ")).apply(result, &scrubber);
        return;
    }
    if let Some(denied) = guard_denied(&guards) {
        denied.apply(result, &scrubber);
        return;
    }
    let secrets = (author_secret, reviewer_secret);
    match reviewer_account {
        Some(account) if gitlab_rows => {
            gitlab_cloud(
                scenario,
                ctx,
                &host,
                secrets,
                account.login,
                &scrubber,
                result,
            );
        }
        Some(account) if ctx.column == Column::GithubCloud && gh_cells::implemented(scenario) => {
            github_cloud(
                scenario,
                ctx,
                &host,
                secrets,
                account.login,
                &scrubber,
                result,
            );
        }
        Some(_) if scenario == Scenario::Capabilities => {
            in_cell_trace(result, |trace, result| {
                let author = cloud_guard(ctx.column, &host, secrets.0, trace, "author");
                let reviewer = cloud_guard(ctx.column, &host, secrets.1, trace, "reviewer");
                capabilities_cloud(&author, &reviewer, trace, &scrubber, result);
            });
        }
        _ => not_implemented(scenario, ctx, result),
    }
}

/// Тело клетки над своим следом: вызовы открытия ролей (`result.trace` и `result.calls` до
/// вызова) остаются в `result.json` перед вызовами клетки, но в след, по которому судит клетка,
/// не входят.
fn in_cell_trace(result: &mut ResultFile, body: impl FnOnce(&Trace, &mut ResultFile)) {
    let open_trace = std::mem::take(&mut result.trace);
    let open_calls = std::mem::take(&mut result.calls);
    let trace = Trace::new();
    body(&trace, result);
    let cell_trace = std::mem::replace(&mut result.trace, open_trace);
    result.trace.extend(cell_trace);
    for (key, count) in open_calls {
        *result.calls.entry(key).or_default() += count;
    }
}

/// `capabilities` облачного столбца над сторожами обеих ролей: возможности ревьюера и автора
/// читаются адаптером провайдера, записей нет (POST GraphQL — чтение). Итог: прочитано —
/// `pass`, наличие мутаций — наблюдения, как у `gitlab_ce`; отказ транспорта, 5xx, отказ сторожа,
/// записи и не приватный репозиторий — ошибка исполнения; 401, 403 и 404 — `not_run` «токен без
/// прав…»; лимит — `not_run` «лимит»; роль ревьюера GitLab ниже Developer — `not_run` по
/// предусловию песочницы; прочий отказ чтения — `fail`.
pub fn capabilities_cloud<T: Transport>(
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    trace: &Trace,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) {
    let target = reviewer.target().clone();
    let mut read = CapsRead::default();
    match target.provider {
        Provider::GitLab => gitlab_cloud_caps(author, reviewer, &mut read, result),
        Provider::GitHub => github_cloud_caps(author, reviewer, &mut read, result),
    }
    let guards = [("reviewer", reviewer), ("author", author)];
    read.errors.extend(guard_errors(&guards));
    let writes: Vec<String> = trace
        .calls()
        .iter()
        .filter(|call| call.request.method.is_write() && call.request.url != target.graphql)
        .map(|call| format!("{} {}", call.request.method, call.request.path()))
        .collect();
    if !writes.is_empty() {
        read.errors.push(format!(
            "записи при чтении возможностей: {}",
            writes.join(", ")
        ));
    }
    result.trace = trace
        .entries(scrubber)
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    result.calls = trace
        .counts(scrubber)
        .into_iter()
        .map(|(key, count)| (key, count as u64))
        .collect();
    result.server_before = Some(scrubber.scrub_json(&Value::Object(read.state)));
    result.notes.extend(read.notes);
    if result.trace.is_empty() {
        read.errors.push("нет следа вызовов".to_owned());
    }
    scrub_notes(scrubber, &mut result.notes);
    let verdict = if !read.errors.is_empty() {
        Some(Verdict::Error(read.errors.join("; ")))
    } else if let Some(denied) = guard_denied(&guards) {
        Some(denied)
    } else if let Some(denied) = read.denied {
        Some(Verdict::NotRun(denied))
    } else if let Some(limit) = read.limited {
        Some(Verdict::NotRun(limit))
    } else if !read.failed.is_empty() {
        Some(Verdict::Fail(format!(
            "возможности не определились чтением: {}",
            read.failed.join("; ")
        )))
    } else {
        read.precondition.map(Verdict::NotRun)
    };
    match (verdict, read.pass) {
        (Some(verdict), _) => verdict.apply(result, scrubber),
        (None, Some(reason)) => {
            result.status = Status::Pass;
            result.reason = scrubber.scrub_text(&reason);
        }
        (None, None) => {
            Verdict::Error("capabilities: итог не собран".to_owned()).apply(result, scrubber)
        }
    }
}

/// Собранное чтением возможностей облачного столбца.
#[derive(Default)]
struct CapsRead {
    state: serde_json::Map<String, Value>,
    errors: Vec<String>,
    failed: Vec<String>,
    notes: Vec<String>,
    limited: Option<String>,
    /// 401, 403 или 404 на чтение возможностей: токен без прав — предусловие, а не наблюдение.
    denied: Option<String>,
    precondition: Option<String>,
    pass: Option<String>,
}

/// Причина `not_run` отказа 401, 403 или 404 на чтение возможностей роли.
fn caps_denied(status: u16, actor: &str) -> String {
    format!("токен без прав или нет репозитория: код {status} на чтение возможностей {actor}")
}

fn gitlab_cloud_caps<T: Transport>(
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    read: &mut CapsRead,
    result: &mut ResultFile,
) {
    let review = ReviewTarget {
        provider: Provider::GitLab,
        host: gitlab_root(reviewer.target()),
        repo: reviewer.target().repo.clone(),
        number: 0,
    };
    let caps = |guard| gitlab_adapter(guard, &review).and_then(|gl| gl.capabilities());
    let reviewer_caps = caps(reviewer);
    let author_caps = caps(author);
    for (actor, caps) in [("reviewer", &reviewer_caps), ("author", &author_caps)] {
        match caps {
            Ok(caps) => {
                read.state.insert(actor.to_owned(), caps.to_json());
            }
            Err(ApiError::RateLimited { status, .. }) => {
                read.limited = Some(format!(
                    "лимит: код {status} на чтение возможностей {actor}"
                ));
            }
            Err(ApiError::Status {
                status: status @ (401 | 403 | 404),
                ..
            }) => read.denied = Some(caps_denied(*status, actor)),
            Err(err) => match classify_read(err) {
                Ok(observed) => read.failed.push(format!("{actor}: {observed}")),
                Err(stand) => read.errors.push(format!("{actor}: {stand}")),
            },
        }
    }
    let Ok(caps) = &reviewer_caps else {
        return;
    };
    let edition = match caps.host.enterprise {
        Some(true) => "EE",
        Some(false) => "CE",
        None => "редакция не видна",
    };
    result.host = Host {
        version: Some(format!("GitLab {} ({edition})", caps.host.version)),
        tier: caps.plan.clone(),
    };
    if !caps.private() {
        read.errors.push(format!(
            "проект песочницы не приватный: {}",
            caps.visibility
        ));
    }
    if !caps.can_review() {
        read.precondition = Some(format!(
            "предусловие: роль ревьюера {:?}, нужна Developer или выше",
            caps.access_level
        ));
    }
    if let Ok(author) = &author_caps {
        read.notes.push(format!(
            "наблюдение: роль автора в своём проекте {:?}",
            author.access_level
        ));
    }
    let tier = match (&caps.plan, caps.plan_source) {
        (Some(plan), Some(source)) => format!("{plan} (из {source})"),
        (Some(plan), None) => plan.clone(),
        (None, _) => "не виден токену".to_owned(),
    };
    let request_changes = if caps.request_changes() {
        "есть"
    } else {
        "нет"
    };
    read.notes.push(format!(
        "наблюдение: mergeRequestRequestChanges в схеме — {request_changes} (ответ {}); тариф \
{tier}",
        caps.introspection
    ));
    read.pass = Some(format!(
        "возможности прочитаны через сторожа без записей: GitLab {} {edition}, проект приватный, \
роль ревьюера не ниже Developer, mergeRequestRequestChanges в схеме — {request_changes}, тариф \
{tier}",
        caps.host.version
    ));
}

fn github_cloud_caps<T: Transport>(
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    read: &mut CapsRead,
    result: &mut ResultFile,
) {
    use crate::github::ApiError as GhError;
    let review = ReviewTarget {
        provider: Provider::GitHub,
        host: reviewer.target().rest.clone(),
        repo: reviewer.target().repo.clone(),
        number: 0,
    };
    let caps = |guard| github_adapter(guard, &review).and_then(|gh| gh.capabilities());
    let reviewer_caps = caps(reviewer);
    let author_caps = caps(author);
    for (actor, caps) in [("reviewer", &reviewer_caps), ("author", &author_caps)] {
        match caps {
            Ok(caps) => {
                read.state.insert(actor.to_owned(), caps.to_json());
            }
            Err(GhError::RateLimited { status, .. }) => {
                read.limited = Some(format!(
                    "лимит: код {status} на чтение возможностей {actor}"
                ));
            }
            Err(err @ (GhError::NotSent(_) | GhError::Transport(_))) => {
                read.errors.push(format!("{actor}: {err}"));
            }
            Err(err @ GhError::Status { status, .. }) if *status >= 500 => {
                read.errors.push(format!("{actor}: {err}"));
            }
            Err(GhError::Status {
                status: status @ (401 | 403 | 404),
                ..
            }) => read.denied = Some(caps_denied(*status, actor)),
            Err(err) => read.failed.push(format!("{actor}: {err}")),
        }
    }
    let Ok(caps) = &reviewer_caps else {
        return;
    };
    let versions = match &caps.api.supported {
        Some(list) if list.contains(&caps.api.requested) => {
            format!("версия API {} есть в списке хоста", caps.api.requested)
        }
        Some(list) => format!(
            "версии API {} нет в списке хоста ({})",
            caps.api.requested,
            list.join(", ")
        ),
        None => format!(
            "версии не читаются (GET /versions — 404), запрошена {}",
            caps.api.requested
        ),
    };
    result.host = Host {
        version: Some(format!("github.com, REST API {}", caps.api.requested)),
        tier: caps.plan.clone(),
    };
    if !caps.repo_private {
        read.errors
            .push("репозиторий песочницы не приватный".to_owned());
    }
    let has = |yes: bool| if yes { "есть" } else { "нет" };
    let observed = format!(
        "серверный PENDING — {}, просмотренные файлы — {}, resolve — {}; {versions}",
        has(caps.server_pending()),
        has(caps.viewed()),
        has(caps.resolve())
    );
    read.notes.push(format!("наблюдение: {observed}"));
    if let Ok(author) = &author_caps {
        read.notes.push(format!(
            "наблюдение: право записи автора — {}",
            has(author.can_push)
        ));
    }
    read.pass = Some(format!(
        "возможности прочитаны через сторожа без записей: репозиторий приватный, {observed}"
    ));
}

/// Версия и тариф облачного GitLab чтением возможностей ревьюера до записей клетки: `absent`
/// без версии — ошибка исполнения. Отказ хоста на чтение — заметка, версия не записывается;
/// отказ транспорта и 5xx — ошибка исполнения.
pub fn gitlab_host<T: Transport>(reviewer: &Guarded<T>) -> Result<(Host, Option<String>), String> {
    let review = ReviewTarget {
        provider: Provider::GitLab,
        host: gitlab_root(reviewer.target()),
        repo: reviewer.target().repo.clone(),
        number: 0,
    };
    match gitlab_adapter(reviewer, &review).and_then(|gl| gl.capabilities()) {
        Ok(caps) => Ok((
            Host {
                version: Some(format!(
                    "GitLab {} ({})",
                    caps.host.version,
                    match caps.host.enterprise {
                        Some(true) => "EE",
                        Some(false) => "CE",
                        None => "редакция не видна",
                    }
                )),
                tier: caps.plan.clone(),
            },
            None,
        )),
        Err(err) => match classify_read(&err) {
            Ok(observed) => Ok((
                Host::default(),
                Some(format!("наблюдение: версия хоста не прочитана: {observed}")),
            )),
            Err(stand) => Err(format!("reviewer: версия хоста: {stand}")),
        },
    }
}

/// Клетка строки GitLab на gitlab.com: та же обвязка, что у `gitlab_ce`, над своим следом.
/// Вызовы открытия ролей идут в `result.json` перед вызовами клетки: уборка прежних процессов
/// пишет, а оракулы строк судят записи следа клетки.
fn gitlab_cloud(
    scenario: Scenario,
    ctx: &Context,
    host: &CloudHost,
    (author_secret, reviewer_secret): (&Secret, &Secret),
    reviewer_login: String,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            result.execution = Execution::error(format!("бинарь пробы: {}", err.kind()));
            return;
        }
    };
    let column = Column::GitlabCloud;
    in_cell_trace(result, |trace, result| {
        let author = cloud_guard(column, host, author_secret, trace, "author");
        let reviewer = cloud_guard(column, host, reviewer_secret, trace, "reviewer");
        let second = ProcessClient {
            exe,
            column,
            out: ctx.out.clone(),
        };
        let head = AuthorCommit { author: &author };
        let cx = GlCx {
            author: &author,
            reviewer: &reviewer,
            trace,
            host_url: gitlab_root(&host.target),
            reviewer_login,
            state_dir: &ctx.state_dir,
            poll: Poll::LIVE,
            second: &second,
            head: &head,
        };
        gl_cells::run_gl_cell(scenario, &cx, &ctx.out, scrubber, result);
    });
}

/// Клетка строки GitHub на github.com: обвязка `run_live_cell` над своим следом, как у
/// [`gitlab_cloud`].
fn github_cloud(
    scenario: Scenario,
    ctx: &Context,
    host: &CloudHost,
    (author_secret, reviewer_secret): (&Secret, &Secret),
    reviewer_login: String,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) {
    let column = Column::GithubCloud;
    result.host.version = Some("github.com".to_owned());
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            result.execution = Execution::error(format!("бинарь пробы: {}", err.kind()));
            return;
        }
    };
    in_cell_trace(result, |trace, result| {
        let author = cloud_guard(column, host, author_secret, trace, "author");
        let reviewer = cloud_guard(column, host, reviewer_secret, trace, "reviewer");
        let second = ProcessClient {
            exe,
            column,
            out: ctx.out.clone(),
        };
        let cx = gh_cells::GhCx {
            author: &author,
            reviewer: &reviewer,
            trace,
            host_url: "github.com".to_owned(),
            reviewer_login,
            state_dir: &ctx.state_dir,
            poll: Poll::LIVE,
            second: &second,
        };
        gh_cells::run_gh_cell(scenario, &cx, &ctx.out, scrubber, result);
    });
}

/// Роли живой клетки до её записей: аккаунты обеих ролей (логин и отображаемое имя) — в
/// очиститель, затем уборка по журналам прежних процессов столбца (`runs`: корень прогонов и
/// свой каталог процесса). Сценарии строк продолжают с теми же сторожами. Ответ — аккаунт
/// ревьюера: его логин ведёт сессию журнала и черновики второго клиента.
pub fn open_roles<T: Transport>(
    column: Column,
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    runs: Option<(&Path, &Path)>,
    scrubber: &mut Scrubber,
    result: &mut ResultFile,
) -> Result<Account, Verdict> {
    let author_repo = live_repo(author).map_err(Verdict::Error)?;
    let reviewer_repo = live_repo(reviewer).map_err(Verdict::Error)?;
    let mut reviewer_account = None;
    for (repo, label, role) in [
        (&author_repo, LABEL_AUTHOR, "автора"),
        (&reviewer_repo, LABEL_REVIEWER, "ревьюера"),
    ] {
        match repo.whoami() {
            Ok(account) => {
                account.register(scrubber, label);
                if label == LABEL_REVIEWER {
                    reviewer_account = Some(account);
                }
            }
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
    reviewer_account.ok_or_else(|| Verdict::Error("аккаунт ревьюера не прочитан".to_owned()))
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

/// Клетка строки GitLab на локальном GitLab CE: хост и токены из окружения `run.sh`, сторож
/// каждой роли с проверкой `127.0.0.1`.
fn gitlab_ce(scenario: Scenario, ctx: &Context, result: &mut ResultFile) {
    let (host, tokens, _) = match prepared() {
        Ok(prepared) => prepared,
        Err(err) => {
            result.execution = Execution::error(err);
            return;
        }
    };
    let scrubber = ce_scrubber(&host, &tokens);
    let target = GuardTarget::gitlab(&host.endpoints(), &host.project);
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
    let author = guarded(&tokens.author, "author");
    let reviewer = guarded(&tokens.reviewer, "reviewer");
    result.host = Host {
        version: Some(format!("GitLab {} (CE, образ {})", host.version, host.tag)),
        tier: None,
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            result.execution = Execution::error(format!("бинарь пробы: {}", err.kind()));
            return;
        }
    };
    let second = ProcessClient {
        exe,
        column: Column::GitlabCe,
        out: ctx.out.clone(),
    };
    let head = AuthorCommit { author: &author };
    let cx = GlCx {
        author: &author,
        reviewer: &reviewer,
        trace: &trace,
        host_url: host.url.clone(),
        reviewer_login: host.reviewer.username.clone(),
        state_dir: &ctx.state_dir,
        poll: Poll::LIVE,
        second: &second,
        head: &head,
    };
    gl_cells::run_gl_cell(scenario, &cx, &ctx.out, &scrubber, result);
}

/// Процесс второго клиента ревьюера (`--second-client gl-create-draft`, `gl-delete-draft` или
/// `gl-plant-notes`): черновик на новой строке MR `iid`, удаление черновика по ID или заметки MR.
/// Ответ — строка JSON для stdout (`id` созданного или удалённого черновика либо последней
/// заметки, `not_run` или `error`, текст очищен) и признак
/// ошибки исполнения; очищенный след и ответ — в `<out>/second-client.json`.
pub fn second_client(column: Column, iid: u64, op: &SecondOp, out: &Path) -> (Value, bool) {
    second_client_in(column, iid, op, out, LiveEnv::system)
}

/// [`second_client`] с подставляемыми предусловиями облачного столбца: `gitlab_ce` берёт хост и
/// токены из окружения `run.sh`, `gitlab_cloud` — из `sandbox.toml` и Keychain.
pub fn second_client_in(
    column: Column,
    iid: u64,
    op: &SecondOp,
    out: &Path,
    env: impl FnOnce() -> LiveEnv,
) -> (Value, bool) {
    let trace = Trace::new();
    let (done, scrubber) = match (column, op.provider()) {
        (Column::GitlabCe, Provider::GitLab) => second_ce(iid, op, &trace),
        (Column::GitlabCloud, Provider::GitLab) | (Column::GithubCloud, Provider::GitHub) => {
            second_cloud(column, iid, op, &trace, &env())
        }
        _ => (
            Err(Verdict::Error(format!(
                "второй клиент: операция {} в столбце {} не реализована",
                op.name(),
                column.name()
            ))),
            Scrubber::new(),
        ),
    };
    let (reply, ok) = match done {
        Ok(id) => (serde_json::json!({ "id": id }), true),
        Err(Verdict::Error(err)) => (
            serde_json::json!({ "error": scrubber.scrub_text(&err) }),
            false,
        ),
        Err(Verdict::NotRun(reason) | Verdict::Absent(reason) | Verdict::Fail(reason)) => (
            serde_json::json!({ "not_run": scrubber.scrub_text(&reason) }),
            true,
        ),
    };
    let record = serde_json::json!({
        "reply": reply,
        "trace": trace.entries(&scrubber),
    });
    match crate::results::write_json(out, &second_file_in(out), &record) {
        Ok(_) => (reply, ok),
        Err(err) => (
            serde_json::json!({
                "error": scrubber.scrub_text(&format!("след второго клиента: {err}"))
            }),
            false,
        ),
    }
}

/// Свободное имя файла следа второго клиента: [`SECOND_FILE`], затем `second-client-<N>.json`,
/// чтобы второй вызов в той же клетке не перезаписал след первого.
fn second_file_in(out: &Path) -> String {
    (1..)
        .map(|n: u32| match n {
            1 => SECOND_FILE.to_owned(),
            n => format!("second-client-{n}.json"),
        })
        .find(|name| !out.join(name).exists())
        .unwrap_or_else(|| SECOND_FILE.to_owned())
}

/// Операция второго клиента от имени ревьюера через его сторожа.
fn second_op<T: Transport>(
    guard: &Guarded<T>,
    review: &ReviewTarget,
    login: &str,
    op: &SecondOp,
) -> Result<String, Verdict> {
    match op {
        SecondOp::CreateDraft { line } => gl_cells::foreign_draft(guard, review, login, *line),
        SecondOp::DeleteDraft { draft } => gl_cells::foreign_delete(guard, review, draft),
        SecondOp::PlantNotes { count } => gl_cells::foreign_notes(guard, review, *count),
        SecondOp::GhCreatePending { count } => {
            gh_cells::foreign_pending(guard, review, login, *count)
        }
        SecondOp::GhCreateReview { line } => gh_cells::foreign_review(guard, review, login, *line),
    }
}

fn second_ce(iid: u64, op: &SecondOp, trace: &Trace) -> (Result<String, Verdict>, Scrubber) {
    let (host, tokens, _) = match prepared() {
        Ok(prepared) => prepared,
        Err(err) => return (Err(Verdict::Error(err)), Scrubber::new()),
    };
    let scrubber = ce_scrubber(&host, &tokens);
    let http = HttpTransport::new(
        HttpConfig::default(),
        Some(credential(Column::GitlabCe, tokens.reviewer.clone())),
    );
    let guard = Guarded::new(
        Traced::new(http, trace.clone(), gl_cells::ACTOR_SECOND),
        GuardTarget::gitlab(&host.endpoints(), &host.project),
        WriteScope::Loopback,
    );
    let review = ReviewTarget {
        provider: Provider::GitLab,
        host: host.url.clone(),
        repo: host.project.clone(),
        number: iid,
    };
    let done = second_op(&guard, &review, &host.reviewer.username, op);
    (done, scrubber)
}

/// Второй клиент на gitlab.com или github.com: предусловия — как у клетки (`sandbox.toml`, токены
/// обеих ролей). Токен автора нужен очистителю: ответ о MR или PR несёт логин и имя автора, а
/// файла хоста с ними, как у `gitlab_ce`, у облачного столбца нет.
fn second_cloud(
    column: Column,
    iid: u64,
    op: &SecondOp,
    trace: &Trace,
    env: &LiveEnv,
) -> (Result<String, Verdict>, Scrubber) {
    let mut scrubber = Scrubber::new();
    let host = match cloud_host(column, env) {
        Ok(host) => host,
        Err(verdict) => return (Err(verdict), scrubber),
    };
    let (author_secret, reviewer_secret) = match host
        .accounts
        .author()
        .and_then(|author| Ok((author, host.accounts.reviewer()?)))
    {
        Ok(secrets) => secrets,
        Err(reason) => return (Err(Verdict::NotRun(reason)), scrubber),
    };
    host.accounts.register(&mut scrubber);
    scrubber.add_repo(&host.host.repo);
    let author = cloud_guard(column, &host, author_secret, trace, gl_cells::ACTOR_SECOND);
    let reviewer = cloud_guard(
        column,
        &host,
        reviewer_secret,
        trace,
        gl_cells::ACTOR_SECOND,
    );
    let done = second_cloud_op(column, &author, &reviewer, iid, op, &mut scrubber);
    (done, scrubber)
}

/// Операция второго клиента облачного столбца над сторожами обеих ролей: аккаунты ролей — в
/// очиститель и логин ревьюера (без уборки прежних процессов), затем операция ревьюера. Отказ
/// сторожа — ошибка исполнения, нет доступа к репозиторию — `not_run`.
pub fn second_cloud_op<T: Transport>(
    column: Column,
    author: &Guarded<T>,
    reviewer: &Guarded<T>,
    iid: u64,
    op: &SecondOp,
    scrubber: &mut Scrubber,
) -> Result<String, Verdict> {
    let mut scratch = ResultFile::new("", "", "second-client", column);
    let opened = open_roles(column, author, reviewer, None, scrubber, &mut scratch);
    let guards = [("author", author), ("reviewer", reviewer)];
    let errors = guard_errors(&guards);
    if !errors.is_empty() {
        return Err(Verdict::Error(errors.join("; ")));
    }
    if let Some(denied) = guard_denied(&guards) {
        return Err(denied);
    }
    let account = opened?;
    let target = reviewer.target();
    let review = ReviewTarget {
        provider: target.provider,
        host: match target.provider {
            Provider::GitLab => gitlab_root(target),
            Provider::GitHub => "github.com".to_owned(),
        },
        repo: target.repo.clone(),
        number: iid,
    };
    let done = second_op(reviewer, &review, &account.login, op);
    let errors = guard_errors(&guards);
    if !errors.is_empty() {
        return Err(Verdict::Error(errors.join("; ")));
    }
    done
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

        let absent = Verdict::Absent("approve: отказ по правам или тарифу".to_owned());
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
            read_or_fail("approve", &ApiError::ShaRejected("409".to_owned())),
            Verdict::Fail(_)
        ));
        assert!(matches!(
            read_or_fail(
                "approve",
                &ApiError::RateLimited {
                    status: 429,
                    detail: "Retry later".to_owned()
                }
            ),
            Verdict::NotRun(reason) if reason.starts_with("лимит")
        ));
        for lost in [
            ApiError::Transport(crate::transport::Failure::timeout("x")),
            ApiError::Status {
                status: 503,
                message: String::new(),
            },
        ] {
            let verdict = read_or_fail("approve", &ApiError::Unverified(Box::new(lost)));
            assert!(
                matches!(&verdict, Verdict::Error(reason) if reason.starts_with("итог записи не прочитан")),
                "{verdict:?}"
            );
        }

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

    // --- облачный GitLab: второй клиент и версия хоста против forge-fake ---

    const FAKE_REPO: &str = "probe-author/shellio-review-sandbox";
    const FAKE_AUTHOR: &str = "probe-author";
    const FAKE_REVIEWER: &str = "probe-reviewer";
    const FAKE_AUTHOR_TOKEN: &str = "sentinel-cloud-author-7e1a";
    const FAKE_REVIEWER_TOKEN: &str = "sentinel-cloud-reviewer-3b9c";
    const FAKE_IID: u64 = 4;
    const FAKE_DIFF: &str = "@@ -1,8 +1,7 @@\n line 01\n line 02\n-line 03\n line 04\n-line 05\n\
+line 05 changed\n line 06\n line 07\n line 08\n@@ -22,6 +21,8 @@\n line 22\n line 23\n line 24\n\
+line 24a\n+line 24b\n line 25\n line 26\n line 27\n";

    fn cloud_fake(
        private: bool,
    ) -> Result<crate::fake::FakeServer<crate::fake::gitlab::GitlabFake>, String> {
        use crate::fake::gitlab::{Access, FakeDiff, GitlabFake};
        let mut fake = GitlabFake::new(FAKE_REPO);
        if !private {
            fake.set_visibility("public");
        }
        fake.add_user(FAKE_AUTHOR, FAKE_AUTHOR_TOKEN, Some(Access::Developer));
        fake.add_user(FAKE_REVIEWER, FAKE_REVIEWER_TOKEN, Some(Access::Developer));
        fake.add_mr(
            FAKE_IID,
            FAKE_AUTHOR,
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            vec![FakeDiff::modified("probe-review/fixture.txt", FAKE_DIFF)],
        );
        fake.add_reviewer(FAKE_IID, FAKE_REVIEWER)?;
        crate::fake::FakeServer::start(fake)
    }

    fn fake_guard(base: &str, token: &str, trace: &Trace) -> Guarded<Traced<HttpTransport>> {
        let credential = crate::transport::http::Credential::private_token(Secret::new(token));
        Guarded::new(
            Traced::new(
                HttpTransport::new(HttpConfig::default(), Some(credential)),
                trace.clone(),
                gl_cells::ACTOR_SECOND,
            ),
            GuardTarget::gitlab(&gitlab::Endpoints::instance(base), FAKE_REPO),
            WriteScope::Loopback,
        )
    }

    fn cloud_scrubber_for_fake() -> Scrubber {
        let mut scrubber = Scrubber::new();
        scrubber
            .add_token(FAKE_AUTHOR_TOKEN)
            .add_token(FAKE_REVIEWER_TOKEN)
            .add_repo(FAKE_REPO);
        scrubber
    }

    #[test]
    fn cloud_second_client_registers_both_roles_and_creates_the_reviewer_draft()
    -> Result<(), String> {
        let server = cloud_fake(true)?;
        let trace = Trace::new();
        let author = fake_guard(server.base(), FAKE_AUTHOR_TOKEN, &trace);
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let mut scrubber = cloud_scrubber_for_fake();
        let id = second_cloud_op(
            Column::GitlabCloud,
            &author,
            &reviewer,
            FAKE_IID,
            &SecondOp::CreateDraft { line: 24 },
            &mut scrubber,
        )
        .map_err(|v| format!("{v:?}"))?;
        let drafts = server.with(|fake| fake.drafts(FAKE_IID));
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id.to_string(), id);
        assert_eq!(drafts[0].author, FAKE_REVIEWER, "черновик ревьюера");
        assert!(author.refused_writes().is_empty() && reviewer.refused_writes().is_empty());

        let text = serde_json::to_string(&trace.entries(&scrubber)).map_err(|e| e.to_string())?;
        for secret in [
            FAKE_AUTHOR,
            FAKE_REVIEWER,
            FAKE_REPO,
            FAKE_AUTHOR_TOKEN,
            FAKE_REVIEWER_TOKEN,
        ] {
            assert!(!text.contains(secret), "{secret} в следе: {text}");
        }
        assert!(
            text.contains(LABEL_AUTHOR),
            "автор MR очищен меткой: {text}"
        );
        assert!(
            trace
                .calls()
                .iter()
                .all(|call| call.actor == gl_cells::ACTOR_SECOND)
        );
        Ok(())
    }

    #[test]
    fn cloud_second_client_refuses_a_public_project_without_writes() -> Result<(), String> {
        let server = cloud_fake(false)?;
        let trace = Trace::new();
        let author = fake_guard(server.base(), FAKE_AUTHOR_TOKEN, &trace);
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let done = second_cloud_op(
            Column::GitlabCloud,
            &author,
            &reviewer,
            FAKE_IID,
            &SecondOp::CreateDraft { line: 24 },
            &mut cloud_scrubber_for_fake(),
        );
        assert!(matches!(&done, Err(Verdict::Error(_))), "{done:?}");
        assert!(server.with(|fake| fake.drafts(FAKE_IID)).is_empty());
        assert!(
            trace
                .calls()
                .iter()
                .all(|call| !call.request.method.is_write())
        );
        Ok(())
    }

    #[test]
    fn second_client_without_cloud_preconditions_is_not_run_and_github_is_an_error()
    -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let op = SecondOp::CreateDraft { line: 24 };
        let (reply, ok) = second_client_in(
            Column::GitlabCloud,
            FAKE_IID,
            &op,
            dir.path(),
            untouched_cloud,
        );
        assert!(ok);
        assert!(reply["not_run"].is_string(), "{reply}");
        let record =
            std::fs::read_to_string(dir.path().join(SECOND_FILE)).map_err(|e| e.to_string())?;
        let record: Value = serde_json::from_str(&record).map_err(|e| e.to_string())?;
        assert_eq!(record["reply"], reply);
        assert_eq!(record["trace"], serde_json::json!([]));
        let (again, _) = second_client_in(
            Column::GitlabCloud,
            FAKE_IID,
            &op,
            dir.path(),
            untouched_cloud,
        );
        let second = std::fs::read_to_string(dir.path().join("second-client-2.json"))
            .map_err(|e| e.to_string())?;
        let second: Value = serde_json::from_str(&second).map_err(|e| e.to_string())?;
        assert_eq!(
            second["reply"], again,
            "второй вызов не перезаписал след первого"
        );

        for column in [Column::GithubCloud, Column::Ghes, Column::Fake] {
            let (reply, ok) = second_client_in(column, FAKE_IID, &op, dir.path(), || {
                panic!("предусловия облачного GitLab не читаются")
            });
            assert!(!ok);
            assert!(
                reply["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("не реализован"))
            );
        }

        let pending = SecondOp::GhCreatePending { count: 2 };
        let (reply, ok) = second_client_in(
            Column::GithubCloud,
            FAKE_IID,
            &pending,
            dir.path(),
            untouched_cloud,
        );
        assert!(ok);
        assert!(reply["not_run"].is_string(), "{reply}");
        for column in [
            Column::GitlabCe,
            Column::GitlabCloud,
            Column::Ghes,
            Column::Fake,
        ] {
            let (reply, ok) = second_client_in(column, FAKE_IID, &pending, dir.path(), || {
                panic!(
                    "операция GitHub в столбце {} не читает предусловий",
                    column.name()
                )
            });
            assert!(!ok);
            assert!(
                reply["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("gh-create-pending")),
                "{reply}"
            );
        }
        Ok(())
    }

    /// Предусловия без файла песочниц и с командой `security`, которой нет: до неё не доходит.
    fn untouched_cloud() -> LiveEnv {
        LiveEnv {
            sandbox: None,
            keychain: Keychain::with_program(
                std::path::Path::new("/nonexistent/security"),
                std::time::Duration::from_secs(1),
            ),
        }
    }

    #[test]
    fn gitlab_host_reads_version_through_the_reviewer_guard() -> Result<(), String> {
        let server = cloud_fake(true)?;
        let trace = Trace::new();
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let (host, note) = gitlab_host(&reviewer)?;
        assert!(
            host.version
                .as_deref()
                .is_some_and(|v| v.starts_with("GitLab ")),
            "{host:?}"
        );
        assert_eq!(note, None);
        assert!(reviewer.refused_writes().is_empty());
        assert_eq!(
            gitlab_root(&GuardTarget::gitlab(&gitlab::Endpoints::cloud(), "a/b")),
            "https://gitlab.com"
        );

        let silent = Scripted::new();
        let guard = Guarded::new(
            &silent,
            GuardTarget::gitlab(
                &gitlab::Endpoints::instance("http://127.0.0.1:1"),
                FAKE_REPO,
            ),
            WriteScope::Loopback,
        );
        assert!(
            gitlab_host(&guard).is_err(),
            "отказ транспорта — ошибка исполнения"
        );
        Ok(())
    }

    fn caps_cell(
        author: &Guarded<Traced<HttpTransport>>,
        reviewer: &Guarded<Traced<HttpTransport>>,
        trace: &Trace,
        column: Column,
    ) -> ResultFile {
        let mut result = ResultFile::new("s", "b", "capabilities", column);
        result.trace = vec![serde_json::json!({"open": true})];
        result.calls.insert("GET /user".to_owned(), 2);
        in_cell_trace(&mut result, |cell_trace, result| {
            assert!(cell_trace.calls().is_empty());
            capabilities_cloud(author, reviewer, trace, &cloud_scrubber_for_fake(), result);
        });
        result
    }

    #[test]
    fn gitlab_cloud_capabilities_read_without_writes_and_record_the_version() -> Result<(), String>
    {
        let server = cloud_fake(true)?;
        let trace = Trace::new();
        let author = fake_guard(server.base(), FAKE_AUTHOR_TOKEN, &trace);
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let result = caps_cell(&author, &reviewer, &trace, Column::GitlabCloud);
        assert_eq!(result.status, Status::Pass, "{result:?}");
        assert!(result.execution.is_ok(), "{:?}", result.execution);
        assert!(
            result
                .host
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("GitLab "))
        );
        assert_eq!(
            result.trace[0],
            serde_json::json!({"open": true}),
            "открытие ролей первым"
        );
        assert!(result.trace.len() > 1);
        assert!(result.calls.get("GET /user").is_some_and(|n| *n >= 2));
        assert!(result.server_before.is_some());
        let text = serde_json::to_string(&result).map_err(|e| e.to_string())?;
        for secret in [FAKE_REPO, FAKE_AUTHOR_TOKEN, FAKE_REVIEWER_TOKEN] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }
        Ok(())
    }

    #[test]
    fn gitlab_cloud_reporter_reviewer_is_not_run_by_precondition() -> Result<(), String> {
        use crate::fake::gitlab::{Access, GitlabFake};
        let mut fake = GitlabFake::new(FAKE_REPO);
        fake.add_user(FAKE_AUTHOR, FAKE_AUTHOR_TOKEN, Some(Access::Developer));
        fake.add_user(FAKE_REVIEWER, FAKE_REVIEWER_TOKEN, Some(Access::Reporter));
        let server = crate::fake::FakeServer::start(fake)?;
        let trace = Trace::new();
        let author = fake_guard(server.base(), FAKE_AUTHOR_TOKEN, &trace);
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let result = caps_cell(&author, &reviewer, &trace, Column::GitlabCloud);
        assert_eq!(result.status, Status::NotRun, "{result:?}");
        assert!(
            result.reason.starts_with("предусловие: роль ревьюера"),
            "{}",
            result.reason
        );
        Ok(())
    }

    #[test]
    fn github_cloud_capabilities_read_without_writes() -> Result<(), String> {
        use crate::fake::github::{GithubFake, Permission};
        let mut fake = GithubFake::new("probe-author", "shellio-review-sandbox");
        fake.add_user(FAKE_AUTHOR, FAKE_AUTHOR_TOKEN, Permission::Write);
        fake.add_user(FAKE_REVIEWER, FAKE_REVIEWER_TOKEN, Permission::Write);
        let server = crate::fake::FakeServer::start(fake)?;
        let trace = Trace::new();
        let guard = |token: &str| {
            let credential = crate::transport::http::Credential::bearer(Secret::new(token));
            Guarded::new(
                Traced::new(
                    HttpTransport::new(HttpConfig::default(), Some(credential)),
                    trace.clone(),
                    "role",
                ),
                GuardTarget::github(&crate::github::Endpoints::single(server.base()), FAKE_REPO),
                WriteScope::Loopback,
            )
        };
        let author = guard(FAKE_AUTHOR_TOKEN);
        let reviewer = guard(FAKE_REVIEWER_TOKEN);
        let result = caps_cell(&author, &reviewer, &trace, Column::GithubCloud);
        assert_eq!(result.status, Status::Pass, "{result:?}");
        assert!(
            result.reason.contains("серверный PENDING — есть"),
            "{}",
            result.reason
        );
        assert!(result.host.version.is_some());

        let mut public = GithubFake::new("probe-author", "shellio-review-sandbox");
        public.set_private(false);
        public.add_user(FAKE_AUTHOR, FAKE_AUTHOR_TOKEN, Permission::Write);
        public.add_user(FAKE_REVIEWER, FAKE_REVIEWER_TOKEN, Permission::Write);
        let server = crate::fake::FakeServer::start(public)?;
        let trace = Trace::new();
        let guard = |token: &str| {
            let credential = crate::transport::http::Credential::bearer(Secret::new(token));
            Guarded::new(
                Traced::new(
                    HttpTransport::new(HttpConfig::default(), Some(credential)),
                    trace.clone(),
                    "role",
                ),
                GuardTarget::github(&crate::github::Endpoints::single(server.base()), FAKE_REPO),
                WriteScope::Loopback,
            )
        };
        let result = caps_cell(
            &guard(FAKE_AUTHOR_TOKEN),
            &guard(FAKE_REVIEWER_TOKEN),
            &trace,
            Column::GithubCloud,
        );
        assert!(
            !result.execution.is_ok(),
            "публичный репозиторий: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn cloud_capabilities_of_a_token_without_access_are_not_run() -> Result<(), String> {
        use crate::fake::gitlab::{Access, GitlabFake};
        let mut fake = GitlabFake::new(FAKE_REPO);
        fake.add_user(FAKE_AUTHOR, FAKE_AUTHOR_TOKEN, Some(Access::Developer));
        fake.add_user(FAKE_REVIEWER, FAKE_REVIEWER_TOKEN, None);
        let server = crate::fake::FakeServer::start(fake)?;
        let trace = Trace::new();
        let author = fake_guard(server.base(), FAKE_AUTHOR_TOKEN, &trace);
        let reviewer = fake_guard(server.base(), FAKE_REVIEWER_TOKEN, &trace);
        let result = caps_cell(&author, &reviewer, &trace, Column::GitlabCloud);
        assert!(result.execution.is_ok(), "{:?}", result.execution);
        assert_eq!(result.status, Status::NotRun, "{result:?}");
        assert!(
            result.reason.starts_with("токен без прав"),
            "{}",
            result.reason
        );
        Ok(())
    }
}
