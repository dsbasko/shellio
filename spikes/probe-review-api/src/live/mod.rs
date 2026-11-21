//! Живой контур пробы (план P0.7, «Живой контур»): файл песочниц, токены из Keychain, сторож
//! записи и жизненный цикл клетки на настоящих хостах. Тесты ходят только во встроенный
//! транспорт и в подставную команду `security`; живые хосты — только в `make probe-review-live`.

pub mod github;
pub mod gitlab;
pub mod guard;
pub mod keychain;
pub mod lifecycle;
pub mod provision;
pub mod repo;
pub mod sandbox;

use crate::results::Column;
use crate::scrub::Scrubber;
use crate::transport::http::{Credential, Secret};

use guard::{GuardTarget, WriteScope};
use keychain::{Keychain, TokenError};
use sandbox::{HostEntry, Sandbox, SandboxError};

/// Токены ролей облачного хоста. Ошибка роли — причина `not_run` её клеток.
#[derive(Debug)]
pub struct Accounts {
    pub author: Result<Secret, TokenError>,
    pub reviewer: Result<Secret, TokenError>,
}

impl Accounts {
    pub fn read(keychain: &Keychain, service: &str, host: &HostEntry) -> Accounts {
        Accounts {
            author: keychain.token(service, &host.author),
            reviewer: keychain.token(service, &host.reviewer),
        }
    }

    /// Токен автора или причина `not_run` «нет автора: …».
    pub fn author(&self) -> Result<&Secret, String> {
        self.author
            .as_ref()
            .map_err(|err| format!("нет автора: {}", err.not_run_reason()))
    }

    /// Токен ревьюера или причина `not_run` «нет ревьюера: …».
    pub fn reviewer(&self) -> Result<&Secret, String> {
        self.reviewer
            .as_ref()
            .map_err(|err| format!("нет ревьюера: {}", err.not_run_reason()))
    }

    /// Токены — в очиститель до первого вызова.
    pub fn register(&self, scrubber: &mut Scrubber) {
        for secret in [&self.author, &self.reviewer].into_iter().flatten() {
            scrubber.add_token(secret.expose());
        }
    }
}

/// Схема учётных данных столбца: GitHub — `Authorization: Bearer`, GitLab — `PRIVATE-TOKEN`.
pub fn credential(column: Column, secret: Secret) -> Credential {
    match column {
        Column::GithubCloud | Column::Ghes => Credential::bearer(secret),
        Column::Fake | Column::GitlabCloud | Column::GitlabCe => Credential::private_token(secret),
    }
}

/// Цель сторожа облачного хоста: адреса API столбца и путь из `sandbox.toml`.
pub fn cloud_target(host: &HostEntry) -> Option<GuardTarget> {
    match host.column {
        Column::GithubCloud => Some(GuardTarget::github(
            &crate::github::Endpoints::cloud(),
            &host.repo,
        )),
        Column::GitlabCloud => Some(GuardTarget::gitlab(
            &crate::gitlab::Endpoints::cloud(),
            &host.repo,
        )),
        Column::Fake | Column::GitlabCe | Column::Ghes => None,
    }
}

/// Предусловия облачного столбца.
#[derive(Debug)]
pub enum Preflight {
    Ready(Box<CloudHost>),
    /// Предусловия нет: клетки хоста — `not_run` с причиной.
    NotRun(String),
    /// Файл песочниц испорчен: ошибка исполнения.
    Error(String),
}

/// Облачный хост, готовый к клеткам: запись, цель сторожа и токены ролей.
#[derive(Debug)]
pub struct CloudHost {
    pub host: HostEntry,
    pub target: GuardTarget,
    pub scope: WriteScope,
    pub accounts: Accounts,
}

/// Файл песочниц и токены облачного столбца.
pub fn preflight(
    column: Column,
    sandbox: Result<Sandbox, SandboxError>,
    keychain: &Keychain,
) -> Preflight {
    let sandbox = match sandbox {
        Ok(sandbox) => sandbox,
        Err(err) => {
            return match err.not_run_reason() {
                Some(reason) => Preflight::NotRun(reason),
                None => Preflight::Error(err.to_string()),
            };
        }
    };
    let host = match sandbox.host(column) {
        Ok(host) => host.clone(),
        Err(reason) => return Preflight::NotRun(reason),
    };
    let Some(target) = cloud_target(&host) else {
        return Preflight::Error(format!("столбец {} не облачный", column.name()));
    };
    let accounts = Accounts::read(keychain, &sandbox.service, &host);
    Preflight::Ready(Box::new(CloudHost {
        scope: WriteScope::from_sandbox(&sandbox, column),
        host,
        target,
        accounts,
    }))
}

#[cfg(test)]
mod tests;
