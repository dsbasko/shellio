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

use serde_json::Value;

use super::Context;
use crate::gitlab::capabilities::{self as gl_caps, IntrospectionReply};
use crate::gitlab::{ApiError, Capabilities, DEVELOPER, GitLab};
use crate::live::credential;
use crate::live::guard::{GuardTarget, Guarded, WriteScope};
use crate::live::provision::{self, CeHost, CeTokens, Pin};
use crate::model::{Provider, ReviewTarget};
use crate::results::{Column, Execution, Host, ResultFile, Status};
use crate::scrub::{LABEL_AUTHOR, LABEL_REVIEWER, Scrubber};
use crate::transport::http::{HttpConfig, HttpTransport, Secret};
use crate::transport::{Trace, Traced};

use super::Scenario;

pub fn run(scenario: Scenario, ctx: &Context, result: &mut ResultFile) {
    match (scenario, ctx.column) {
        (Scenario::Capabilities, Column::GitlabCe) => capabilities_ce(result),
        _ => {
            result.execution = Execution::error(format!(
                "сценарий {} в столбце {} не реализован",
                scenario.name(),
                ctx.column.name()
            ));
        }
    }
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
        GitLab::new(transport, endpoints.clone(), &review).and_then(|gl| gl.capabilities())
    };
    let reviewer_caps = read(&reviewer);
    let author_caps = read(&author);
    let literal =
        GitLab::new(&reviewer, endpoints.clone(), &review).and_then(|gl| gl.introspect_literal());

    let mut errors = Vec::new();
    for (actor, guard) in [("reviewer", &reviewer), ("author", &author)] {
        if let Some(refusal) = guard.refusal() {
            errors.push(format!("{actor}: {refusal}"));
        }
        let refused = guard.refused_writes();
        if !refused.is_empty() {
            errors.push(format!(
                "{actor}: сторож не пропустил {}",
                refused.join(", ")
            ));
        }
    }
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
glpat-authorsecret1 glpat-reviewersecret2 Probe Author probe-reviewer Probe Reviewer",
        );
        for secret in [
            "55001",
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
}
