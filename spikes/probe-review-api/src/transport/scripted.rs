//! Встроенный транспорт со сценарием для юнит-тестов адаптеров и отправки.
//!
//! Сценарий — очередь шагов: ожидаемый метод и путь и ответ или отказ. Каждый запрос
//! записывается по порядку. Неожиданный запрос шаг не съедает, получает отказ `NotSent` и
//! попадает в нарушения; [`Scripted::finish`] возвращает ошибку, если нарушения есть или шаги
//! остались. Срок не ждётся: таймаут и обрыв возвращаются сразу.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use super::{Failure, FailureKind, Method, Request, Response, Transport};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Respond(Response),
    Fail(Failure),
}

impl Reply {
    pub fn json(status: u16, value: &Value) -> Reply {
        Reply::Respond(Response::json(status, value))
    }

    pub fn status(status: u16) -> Reply {
        Reply::Respond(Response::new(status))
    }

    /// Запрос не ушёл: сервер его не видел.
    pub fn not_sent() -> Reply {
        Reply::Fail(Failure::not_sent("сценарий: соединение не установлено"))
    }

    /// Запрос дошёл, ответа нет до срока.
    pub fn timeout() -> Reply {
        Reply::Fail(Failure::timeout("сценарий: срок ответа истёк"))
    }

    /// Запрос дошёл, соединение оборвано до ответа.
    pub fn disconnect() -> Reply {
        Reply::Fail(Failure::disconnected("сценарий: обрыв после отправки"))
    }
}

#[derive(Clone, Debug)]
struct Step {
    method: Option<Method>,
    path: Option<String>,
    reply: Reply,
}

impl Step {
    /// Путь без `?` сравнивается с путём запроса без строки запроса, путь с `?` — целиком.
    fn matches(&self, request: &Request) -> bool {
        let method = self.method.is_none_or(|method| method == request.method);
        let path = self.path.as_deref().is_none_or(|expected| {
            let actual = request.path();
            if expected.contains('?') {
                actual == expected
            } else {
                actual.split_once('?').map_or(actual, |(path, _)| path) == expected
            }
        });
        method && path
    }

    fn describe(&self) -> String {
        format!(
            "{} {}",
            self.method.map_or("*", Method::as_str),
            self.path.as_deref().unwrap_or("*")
        )
    }
}

/// Запрос, который получил встроенный транспорт, и дошёл ли он до «сервера».
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    pub request: Request,
    pub delivered: bool,
}

#[derive(Default)]
struct State {
    steps: VecDeque<Step>,
    exchanges: Vec<Exchange>,
    violations: Vec<String>,
}

#[derive(Default)]
pub struct Scripted {
    state: Mutex<State>,
}

impl Scripted {
    pub fn new() -> Scripted {
        Scripted::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(self, method: Option<Method>, path: Option<String>, reply: Reply) -> Scripted {
        self.lock().steps.push_back(Step {
            method,
            path,
            reply,
        });
        self
    }

    /// Следующий запрос должен быть `method path`.
    pub fn expect(self, method: Method, path: impl Into<String>, reply: Reply) -> Scripted {
        self.push(Some(method), Some(path.into()), reply)
    }

    /// Следующий запрос — любой.
    pub fn then(self, reply: Reply) -> Scripted {
        self.push(None, None, reply)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.lock()
            .exchanges
            .iter()
            .map(|exchange| exchange.request.clone())
            .collect()
    }

    /// Запросы, которые дошли до «сервера»: все, кроме отказа `NotSent` и нарушений.
    pub fn delivered(&self) -> Vec<Request> {
        self.lock()
            .exchanges
            .iter()
            .filter(|exchange| exchange.delivered)
            .map(|exchange| exchange.request.clone())
            .collect()
    }

    pub fn remaining(&self) -> usize {
        self.lock().steps.len()
    }

    /// Сценарий исполнен ровно: нарушений нет, шагов не осталось. Возвращает все запросы.
    pub fn finish(&self) -> Result<Vec<Request>, String> {
        let state = self.lock();
        let mut problems = state.violations.clone();
        problems.extend(
            state
                .steps
                .iter()
                .map(|step| format!("шаг не исполнен: {}", step.describe())),
        );
        if problems.is_empty() {
            Ok(state
                .exchanges
                .iter()
                .map(|exchange| exchange.request.clone())
                .collect())
        } else {
            Err(problems.join("; "))
        }
    }
}

impl Transport for Scripted {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let mut state = self.lock();
        let step = match state.steps.front() {
            Some(step) if step.matches(request) => state.steps.pop_front(),
            Some(step) => {
                let violation = format!(
                    "неожиданный запрос {} {}, ожидался {}",
                    request.method,
                    request.path(),
                    step.describe()
                );
                state.violations.push(violation);
                None
            }
            None => {
                let violation = format!(
                    "неожиданный запрос {} {}: сценарий исчерпан",
                    request.method,
                    request.path()
                );
                state.violations.push(violation);
                None
            }
        };
        let reply = step.map_or_else(
            || Reply::Fail(Failure::not_sent("сценарий: неожиданный запрос")),
            |step| step.reply,
        );
        let delivered =
            !matches!(&reply, Reply::Fail(failure) if failure.kind == FailureKind::NotSent);
        state.exchanges.push(Exchange {
            request: request.clone(),
            delivered,
        });
        match reply {
            Reply::Respond(response) => Ok(response),
            Reply::Fail(failure) => Err(failure),
        }
    }
}
