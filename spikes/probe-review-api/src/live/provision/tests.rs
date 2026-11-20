//! Закрепление образа и настройка GitLab CE против встроенного транспорта.

use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::transport::http::AuthScheme;
use crate::transport::scripted::{Reply, Scripted};

const URL: &str = "http://127.0.0.1:55001";
const PIN: &str = "\
# комментарий
image = gitlab/gitlab-ce
tag = 19.6.1-ce.0
index = sha256:d3727f8b1a1ee6de44916bb9113f3dc40339dba2d6a6ad6ef331d0f239004056

digest = sha256:3aa073ef2ce0058aa067bec15c7b63b4e09200dc07994790bf6eff5e7edde289
platform = linux/arm64
date = 2026-11-20
";

fn pin() -> Pin {
    match Pin::parse(PIN) {
        Ok(pin) => pin,
        Err(err) => panic!("образец pin: {err}"),
    }
}

#[test]
fn embedded_pin_is_valid_and_pins_index_digest() -> Result<(), String> {
    let pin = Pin::embedded()?;
    assert_eq!(pin.image, IMAGE);
    assert_eq!(pin.platform, PLATFORM);
    assert!(pin.reference().starts_with("gitlab/gitlab-ce@sha256:"));
    assert!(pin.reference().ends_with(&pin.index));
    assert!(pin.tag.ends_with("-ce.0"));
    Ok(())
}

#[test]
fn pin_version_and_reported_versions() {
    let pin = pin();
    assert_eq!(pin.version(), "19.6.1");
    assert_eq!(
        pin.reference(),
        "gitlab/gitlab-ce@sha256:d3727f8b1a1ee6de44916bb9113f3dc40339dba2d6a6ad6ef331d0f239004056"
    );
    assert!(pin.matches_version("19.6.1"));
    assert!(pin.matches_version("19.6.1-ce"));
    assert!(!pin.matches_version("19.6.10"));
    assert!(!pin.matches_version("19.6.0"));
    assert!(!pin.matches_version("19.6"));
}

#[test]
fn pin_rejects_broken_files() {
    let replace = |from: &str, to: &str| PIN.replace(from, to);
    let cases = [
        (
            replace("platform = linux/arm64", "platform = linux/amd64"),
            "платформа",
        ),
        (replace("tag = 19.6.1-ce.0", "tag = 19.6.1-rc1.ce.0"), "тег"),
        (replace("tag = 19.6.1-ce.0", "tag = latest"), "тег"),
        (
            replace("image = gitlab/gitlab-ce", "image = gitlab/gitlab-ee"),
            "образ",
        ),
        (replace("date = 2026-11-20", "date = 20.11.2026"), "дата"),
        (
            replace("index = sha256:d3727f8b", "index = sha256:D3727F8B"),
            "index",
        ),
        (
            replace("digest = sha256:3aa0", "digest = md5:3aa0"),
            "digest",
        ),
        (
            replace("date = 2026-11-20", "date = 2026-11-20\ndate = 2026-11-21"),
            "повтор",
        ),
        (
            replace("date = 2026-11-20", "date = 2026-11-20\nextra = 1"),
            "неизвестный",
        ),
        (replace("date = 2026-11-20", ""), "нет ключа «date»"),
        (
            replace("date = 2026-11-20", "date 2026-11-20"),
            "ключ = значение",
        ),
        (replace("tag = 19.6.1-ce.0", "tag = 19.6.1 -ce.0"), "пробел"),
        (
            replace(
                "sha256:3aa073ef2ce0058aa067bec15c7b63b4e09200dc07994790bf6eff5e7edde289",
                "sha256:d3727f8b1a1ee6de44916bb9113f3dc40339dba2d6a6ad6ef331d0f239004056",
            ),
            "совпадают",
        ),
    ];
    for (text, needle) in cases {
        match Pin::parse(&text) {
            Ok(pin) => panic!("принят испорченный pin ({needle}): {pin:?}"),
            Err(err) => assert!(err.contains(needle), "«{err}» без «{needle}»"),
        }
    }
}

/// Транспорты ролей настройки: без учётных данных, root, автор, ревьюер. Фабрика выбирает
/// транспорт по токену и запоминает схему учётных данных.
struct Roles {
    anonymous: Scripted,
    root: Scripted,
    author: Scripted,
    reviewer: Scripted,
    schemes: RefCell<Vec<(String, Option<AuthScheme>)>>,
}

impl Roles {
    fn new(anonymous: Scripted, root: Scripted, author: Scripted, reviewer: Scripted) -> Roles {
        Roles {
            anonymous,
            root,
            author,
            reviewer,
            schemes: RefCell::new(Vec::new()),
        }
    }

    fn connect(&self, credential: Option<Credential>) -> &Scripted {
        let (role, scheme, transport) = match &credential {
            None => ("anonymous", None, &self.anonymous),
            Some(c) => match c.secret.expose() {
                "oauth-root-token" | "pat-root-token" => ("root", Some(c.scheme), &self.root),
                "pat-author-token" => ("author", Some(c.scheme), &self.author),
                "pat-reviewer-token" => ("reviewer", Some(c.scheme), &self.reviewer),
                other => panic!("неизвестный токен {other}"),
            },
        };
        self.schemes.borrow_mut().push((role.to_owned(), scheme));
        transport
    }

    fn finish(&self) -> Result<[Vec<Request>; 4], String> {
        Ok([
            self.anonymous.finish()?,
            self.root.finish()?,
            self.author.finish()?,
            self.reviewer.finish()?,
        ])
    }
}

fn grant_ok() -> Scripted {
    Scripted::new().expect(
        Method::Post,
        "/oauth/token",
        Reply::json(
            200,
            &json!({"access_token": "oauth-root-token", "token_type": "Bearer", "scope": "api"}),
        ),
    )
}

fn root_ok(version: &str) -> Scripted {
    Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"id": 1, "username": "root", "is_admin": true})),
        )
        .expect(
            Method::Get,
            "/api/v4/metadata",
            Reply::json(
                200,
                &json!({"version": version, "revision": "abc123", "enterprise": false}),
            ),
        )
        .expect(
            Method::Post,
            "/api/v4/users",
            Reply::json(201, &json!({"id": 2, "username": "probe-author"})),
        )
        .expect(
            Method::Post,
            "/api/v4/users/2/personal_access_tokens",
            Reply::json(201, &json!({"id": 10, "token": "pat-author-token"})),
        )
        .expect(
            Method::Post,
            "/api/v4/users",
            Reply::json(201, &json!({"id": 3, "username": "probe-reviewer"})),
        )
        .expect(
            Method::Post,
            "/api/v4/users/3/personal_access_tokens",
            Reply::json(201, &json!({"id": 11, "token": "pat-reviewer-token"})),
        )
}

fn author_ok(level: u64) -> Scripted {
    Scripted::new()
        .expect(
            Method::Post,
            "/api/v4/projects",
            Reply::json(
                201,
                &json!({
                    "id": 7,
                    "path_with_namespace": "probe-author/shellio-review-sandbox",
                    "visibility": "private",
                    "default_branch": "main",
                    "web_url": "http://95b8b759b74e/probe-author/shellio-review-sandbox",
                }),
            ),
        )
        .expect(
            Method::Post,
            "/api/v4/projects/7/members",
            Reply::json(201, &json!({"id": 3, "access_level": level})),
        )
}

fn reviewer_ok(level: u64) -> Scripted {
    Scripted::new().expect(
        Method::Get,
        "/api/v4/projects/probe-author%2Fshellio-review-sandbox",
        Reply::json(
            200,
            &json!({
                "visibility": "private",
                "permissions": {"project_access": {"access_level": level}, "group_access": null},
            }),
        ),
    )
}

fn body(request: &Request) -> Value {
    request.body_json().unwrap_or(Value::Null)
}

fn password() -> RootAuth {
    RootAuth::Password(Secret::new("pw-of-the-run"))
}

#[test]
fn provisions_users_tokens_and_private_project() -> Result<(), String> {
    let roles = Roles::new(
        grant_ok(),
        root_ok("19.6.1"),
        author_ok(30),
        reviewer_ok(30),
    );
    let (host, tokens) =
        provision(URL, &pin(), password(), |c| roles.connect(c)).map_err(|e| e.to_string())?;
    let [anonymous, root, author, reviewer] = roles.finish()?;

    let grant = String::from_utf8(anonymous[0].body.clone().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    assert_eq!(
        grant,
        "grant_type=password&username=root&password=pw-of-the-run&scope=api"
    );
    assert_eq!(anonymous[0].url, format!("{URL}/oauth/token"));

    assert_eq!(
        body(&root[2]),
        json!({
            "username": "probe-author",
            "name": "Probe Author",
            "email": "probe-author@probe-review.test",
            "force_random_password": true,
            "skip_confirmation": true,
        })
    );
    assert_eq!(body(&root[4])["username"], "probe-reviewer");
    for token_request in [&root[3], &root[5]] {
        assert_eq!(
            body(token_request),
            json!({"name": "probe-review", "scopes": ["api"]})
        );
    }
    assert_eq!(
        body(&author[0]),
        json!({
            "name": "shellio-review-sandbox",
            "path": "shellio-review-sandbox",
            "visibility": "private",
            "initialize_with_readme": true,
        })
    );
    assert_eq!(body(&author[1]), json!({"user_id": 3, "access_level": 30}));
    assert_eq!(reviewer.len(), 1);
    assert!(
        root.iter()
            .chain(&author)
            .chain(&reviewer)
            .chain(&anonymous)
            .all(|r| r.url.starts_with(URL))
    );

    assert_eq!(
        roles.schemes.borrow().as_slice(),
        [
            ("anonymous".to_owned(), None),
            ("root".to_owned(), Some(AuthScheme::Bearer)),
            ("author".to_owned(), Some(AuthScheme::PrivateToken)),
            ("reviewer".to_owned(), Some(AuthScheme::PrivateToken)),
        ]
    );

    assert_eq!(host.url, URL);
    assert_eq!(host.address(), "127.0.0.1:55001");
    assert_eq!(host.version, "19.6.1");
    assert_eq!(host.enterprise, Some(false));
    assert_eq!(host.project, "probe-author/shellio-review-sandbox");
    assert_eq!(host.default_branch, "main");
    assert_eq!(host.author.username, "probe-author");
    assert_eq!(host.reviewer.id, 3);
    assert_eq!(host.root_token, "password_grant");
    assert_eq!(host.web_host.as_deref(), Some("95b8b759b74e"));
    assert_eq!(host.image, pin().reference());
    assert_eq!(tokens.author.expose(), "pat-author-token");
    assert_eq!(tokens.reviewer.expose(), "pat-reviewer-token");

    let description = serde_json::to_string(&host).map_err(|e| e.to_string())?;
    assert!(!description.contains("token\":\"pat"), "{description}");
    assert!(!description.contains("pw-of-the-run"));
    Ok(())
}

#[test]
fn refused_password_grant_stops_before_any_admin_call() -> Result<(), String> {
    for (status, error) in [(400, "unsupported_grant_type"), (401, "invalid_client")] {
        let anonymous = Scripted::new().expect(
            Method::Post,
            "/oauth/token",
            Reply::json(
                status,
                &json!({"error": error, "error_description": "not allowed"}),
            ),
        );
        let roles = Roles::new(anonymous, Scripted::new(), Scripted::new(), Scripted::new());
        let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
        match outcome {
            Err(ProvisionError::GrantRefused(detail)) => {
                assert!(detail.contains(error), "{detail}");
                assert!(detail.contains(&status.to_string()), "{detail}");
            }
            other => panic!("ждался отказ в гранте, получено {other:?}"),
        }
        roles.finish()?;
    }
    Ok(())
}

#[test]
fn root_token_from_rails_runner_skips_password_grant() -> Result<(), String> {
    let roles = Roles::new(
        Scripted::new(),
        root_ok("19.6.1-ce"),
        author_ok(30),
        reviewer_ok(30),
    );
    let auth = RootAuth::Token(Secret::new("pat-root-token"));
    let (host, _) =
        provision(URL, &pin(), auth, |c| roles.connect(c)).map_err(|e| e.to_string())?;
    roles.finish()?;
    assert_eq!(host.root_token, "rails_runner");
    assert_eq!(
        roles.schemes.borrow().first(),
        Some(&("root".to_owned(), Some(AuthScheme::PrivateToken)))
    );
    Ok(())
}

#[test]
fn non_loopback_root_is_refused_without_requests() -> Result<(), String> {
    for url in [
        "https://gitlab.com",
        "http://127.0.0.2:8080",
        "http://user@127.0.0.1:80",
    ] {
        let roles = Roles::new(
            Scripted::new(),
            Scripted::new(),
            Scripted::new(),
            Scripted::new(),
        );
        let outcome = provision(url, &pin(), password(), |c| roles.connect(c));
        assert!(
            matches!(outcome, Err(ProvisionError::NotLoopback(_))),
            "{url}: {outcome:?}"
        );
        assert!(
            roles.schemes.borrow().is_empty(),
            "{url}: транспорт построен"
        );
        roles.finish()?;
    }
    Ok(())
}

#[test]
fn version_other_than_pin_stops_before_users() -> Result<(), String> {
    let root = Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"username": "root", "is_admin": true})),
        )
        .expect(
            Method::Get,
            "/api/v4/metadata",
            Reply::json(200, &json!({"version": "19.5.8", "enterprise": false})),
        );
    let roles = Roles::new(grant_ok(), root, Scripted::new(), Scripted::new());
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    match outcome {
        Err(ProvisionError::Step { step, detail }) => {
            assert_eq!(step, "GET /metadata");
            assert!(
                detail.contains("19.5.8") && detail.contains("19.6.1"),
                "{detail}"
            );
        }
        other => panic!("ждалась ошибка версии, получено {other:?}"),
    }
    roles.finish()?;
    Ok(())
}

#[test]
fn non_admin_root_token_is_refused() -> Result<(), String> {
    let root = Scripted::new().expect(
        Method::Get,
        "/api/v4/user",
        Reply::json(200, &json!({"username": "root", "is_admin": false})),
    );
    let roles = Roles::new(grant_ok(), root, Scripted::new(), Scripted::new());
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    assert!(
        matches!(
            &outcome,
            Err(ProvisionError::Step {
                step: "GET /user root",
                ..
            })
        ),
        "{outcome:?}"
    );
    roles.finish()?;
    Ok(())
}

#[test]
fn reviewer_without_developer_role_is_a_setup_error() -> Result<(), String> {
    let roles = Roles::new(
        grant_ok(),
        root_ok("19.6.1"),
        author_ok(30),
        reviewer_ok(20),
    );
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    assert!(
        matches!(
            &outcome,
            Err(ProvisionError::Step {
                step: "GET /projects/:id ревьюером",
                ..
            })
        ),
        "{outcome:?}"
    );
    roles.finish()?;

    let roles = Roles::new(
        grant_ok(),
        root_ok("19.6.1"),
        author_ok(40),
        Scripted::new(),
    );
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    assert!(
        matches!(
            &outcome,
            Err(ProvisionError::Step {
                step: "POST /projects/:id/members",
                ..
            })
        ),
        "{outcome:?}"
    );
    roles.finish()?;
    Ok(())
}

#[test]
fn public_or_foreign_project_is_a_setup_error() -> Result<(), String> {
    for (path, visibility) in [
        ("probe-author/shellio-review-sandbox", "internal"),
        ("root/shellio-review-sandbox", "private"),
    ] {
        let author = Scripted::new().expect(
            Method::Post,
            "/api/v4/projects",
            Reply::json(
                201,
                &json!({
                    "id": 7,
                    "path_with_namespace": path,
                    "visibility": visibility,
                    "default_branch": "main",
                }),
            ),
        );
        let roles = Roles::new(grant_ok(), root_ok("19.6.1"), author, Scripted::new());
        let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
        assert!(
            matches!(
                &outcome,
                Err(ProvisionError::Step {
                    step: "POST /projects",
                    ..
                })
            ),
            "{path} {visibility}: {outcome:?}"
        );
        roles.finish()?;
    }
    Ok(())
}

#[test]
fn failed_user_creation_reports_status_and_message() -> Result<(), String> {
    let root = Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"username": "root", "is_admin": true})),
        )
        .expect(
            Method::Get,
            "/api/v4/metadata",
            Reply::json(200, &json!({"version": "19.6.1"})),
        )
        .expect(
            Method::Post,
            "/api/v4/users",
            Reply::json(409, &json!({"message": "Email has already been taken"})),
        );
    let roles = Roles::new(grant_ok(), root, Scripted::new(), Scripted::new());
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    match outcome {
        Err(ProvisionError::Step { step, detail }) => {
            assert_eq!(step, "POST /users");
            assert!(detail.contains("409") && detail.contains("already been taken"));
        }
        other => panic!("ждалась ошибка шага, получено {other:?}"),
    }
    roles.finish()?;
    Ok(())
}

#[test]
fn transport_failure_is_a_step_error() -> Result<(), String> {
    let anonymous = Scripted::new().expect(Method::Post, "/oauth/token", Reply::timeout());
    let roles = Roles::new(anonymous, Scripted::new(), Scripted::new(), Scripted::new());
    let outcome = provision(URL, &pin(), password(), |c| roles.connect(c));
    assert!(
        matches!(
            &outcome,
            Err(ProvisionError::Step {
                step: "POST /oauth/token",
                ..
            })
        ),
        "{outcome:?}"
    );
    roles.finish()?;
    Ok(())
}

#[test]
fn tokens_round_trip_and_reject_garbage() -> Result<(), String> {
    let tokens = CeTokens {
        author: Secret::new("pat-author-token"),
        reviewer: Secret::new("pat-reviewer-token"),
    };
    assert_eq!(CeTokens::parse(&tokens.to_json())?, tokens);
    assert!(!format!("{tokens:?}").contains("pat-"));
    for text in [
        "",
        "{}",
        r#"{"author": "pat-author-token"}"#,
        r#"{"author": "pat-author-token", "reviewer": "short"}"#,
        r#"{"author": "pat author token", "reviewer": "pat-reviewer-token"}"#,
    ] {
        assert!(CeTokens::parse(text).is_err(), "принято: {text}");
    }
    Ok(())
}

#[test]
fn host_file_round_trip_and_loopback_check() -> Result<(), String> {
    let roles = Roles::new(
        grant_ok(),
        root_ok("19.6.1"),
        author_ok(30),
        reviewer_ok(30),
    );
    let (host, _) =
        provision(URL, &pin(), password(), |c| roles.connect(c)).map_err(|e| e.to_string())?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = crate::results::write_json(dir.path(), HOST_FILE, &host)?;
    assert_eq!(CeHost::load(&path)?, host);
    assert_eq!(host.endpoints().rest, format!("{URL}/api/v4"));

    let mut foreign = host.clone();
    foreign.url = "https://gitlab.example".to_owned();
    let path = crate::results::write_json(dir.path(), "foreign.json", &foreign)?;
    assert!(CeHost::load(&path).is_err());
    assert!(CeHost::load(&dir.path().join("missing.json")).is_err());
    Ok(())
}

#[test]
fn web_authority_takes_host_from_web_url_forms() {
    use super::web_authority;
    assert_eq!(
        web_authority("http://95b8b759b74e/probe-author/shellio-review-sandbox").as_deref(),
        Some("95b8b759b74e")
    );
    assert_eq!(
        web_authority("http://127.0.0.1:55001/a/b").as_deref(),
        Some("127.0.0.1:55001")
    );
    assert_eq!(
        web_authority("git@95b8b759b74e:probe-author/shellio-review-sandbox.git").as_deref(),
        Some("95b8b759b74e")
    );
    assert_eq!(
        web_authority("ssh://git@95b8b759b74e:2222/a/b.git").as_deref(),
        Some("95b8b759b74e:2222")
    );
    assert_eq!(web_authority("no-authority"), None);
}
