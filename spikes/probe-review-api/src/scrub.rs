//! Очиститель следа и данных для `docs/probes/data/` (раздел плана «Живой контур»).
//!
//! Три слоя:
//! - известные значения: токены и пароли, логины ролей, пути репозиториев с их пространствами
//!   имён, адрес локального GitLab; значения добавляются и по ходу прогона (токен, выданный при
//!   настройке GitLab CE). Сравнение без учёта регистра ASCII, вместе с формами `%2F`, `%3A`,
//!   `%40` и `\/`;
//! - образцы: префиксы токенов GitHub и GitLab (в том числе `GR1348941` токена регистрации
//!   раннеров), адреса электронной почты, хеш почты в адресе Gravatar (`gravatar.com/avatar/<хеш>`);
//! - ключи: значение ключа `access_token`, `token`, `password` и подобных, а также любого ключа с
//!   окончанием `_token`, `_secret` или `_password` (`runners_token` в ответе `GET /projects/:id`
//!   владельцу) в JSON, строке запроса и теле формы заменяется целиком, даже если значение не
//!   известно заранее; значение ключа аватара (`avatar_url`, `avatarUrl`, `gravatar_id`) в JSON —
//!   тоже: адрес аватара содержит хеш почты или ID аккаунта, а его вид у хоста заранее не известен.
//!
//! Совпадения собираются за один проход по исходному тексту, поэтому метка не совпадает со
//! следующим правилом. При наложении побеждает более раннее начало, затем категория (токен,
//! почта, хеш аватара, репозиторий, адрес GitLab CE, логин, пространство имён), затем длина;
//! перекрытые совпадения поглощаются целиком, хвост секрета не остаётся. Публичные `github.com` и
//! `gitlab.com` не заменяются.

use serde_json::{Map, Value};

use crate::transport::is_secret_header;

pub const LABEL_TOKEN: &str = "<token>";
pub const LABEL_EMAIL: &str = "<email>";
pub const LABEL_AVATAR: &str = "<avatar>";
pub const LABEL_REPO: &str = "<repo>";
pub const LABEL_NAMESPACE: &str = "<namespace>";
pub const LABEL_GITLAB_CE: &str = "<gitlab-ce>";
pub const LABEL_AUTHOR: &str = "<author>";
pub const LABEL_REVIEWER: &str = "<reviewer>";

/// Префиксы токенов GitHub и GitLab. `GR1348941` — токен регистрации раннеров GitLab.
const TOKEN_PREFIXES: [&str; 14] = [
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "glpat-",
    "gloas-",
    "gldt-",
    "glrt-",
    "glptt-",
    "glft-",
    "glimt-",
    "GR1348941",
];

/// Ключи, значение которых — секрет, каким бы оно ни было.
const SECRET_KEYS: [&str; 8] = [
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "private_token",
    "password",
    "client_secret",
    "secret",
];

/// Окончания ключа, значение которого — секрет: `runners_token`, `feed_token`, `webhook_secret`.
/// Ключ, в котором слово стоит не в конце (`runner_token_expiration_interval`,
/// `ci_job_token_scope_enabled`), секретом не считается.
const SECRET_KEY_SUFFIXES: [&str; 3] = ["_token", "_secret", "_password"];

/// Ключи JSON с адресом аватара или хешем почты.
const AVATAR_KEYS: [&str; 3] = ["avatar_url", "avatarUrl", "gravatar_id"];

/// Начала адреса Gravatar перед хешем почты: как есть, с `\/` из JSON и в процентной кодировке.
const GRAVATAR_PREFIXES: [&str; 3] = [
    "gravatar.com/avatar/",
    "gravatar.com\\/avatar\\/",
    "gravatar.com%2favatar%2f",
];

/// Самый короткий хеш почты Gravatar — MD5, 32 шестнадцатеричных знака.
const AVATAR_HASH_MIN: usize = 32;

/// Порядок — приоритет при наложении совпадений.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Token,
    Email,
    Avatar,
    Repo,
    GitlabCe,
    Login,
    Namespace,
}

impl Category {
    fn describe(self) -> &'static str {
        match self {
            Category::Token => "токен",
            Category::Email => "адрес электронной почты",
            Category::Avatar => "хеш почты в адресе аватара",
            Category::Repo => "путь репозитория",
            Category::GitlabCe => "адрес локального GitLab",
            Category::Login => "логин",
            Category::Namespace => "пространство имён",
        }
    }
}

#[derive(Clone, Debug)]
struct Rule {
    category: Category,
    label: &'static str,
    /// Формы значения в нижнем регистре ASCII.
    needles: Vec<String>,
    /// Совпадение только целым словом: логин внутри другого слова не заменяется.
    bounded: bool,
}

#[derive(Clone, Copy, Debug)]
struct Span {
    start: usize,
    end: usize,
    category: Category,
    label: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct Scrubber {
    rules: Vec<Rule>,
}

impl Scrubber {
    pub fn new() -> Scrubber {
        Scrubber::default()
    }

    fn add(&mut self, category: Category, label: &'static str, value: &str, bounded: bool) {
        let needles = forms(value);
        if !needles.is_empty() {
            self.rules.push(Rule {
                category,
                label,
                needles,
                bounded,
            });
        }
    }

    /// Токен или пароль: заменяется везде, где встретится.
    pub fn add_token(&mut self, value: &str) -> &mut Scrubber {
        self.add(Category::Token, LABEL_TOKEN, value, false);
        self
    }

    /// Логин роли, например [`LABEL_AUTHOR`].
    pub fn add_login(&mut self, login: &str, label: &'static str) -> &mut Scrubber {
        self.add(Category::Login, label, login, true);
        self
    }

    /// Путь репозитория `владелец/имя` (у GitLab — с подгруппами) и каждое его пространство
    /// имён.
    pub fn add_repo(&mut self, path: &str) -> &mut Scrubber {
        let path = path.trim_matches('/');
        self.add(Category::Repo, LABEL_REPO, path, true);
        let segments: Vec<&str> = path.split('/').collect();
        for end in 1..segments.len() {
            self.add(
                Category::Namespace,
                LABEL_NAMESPACE,
                &segments[..end].join("/"),
                true,
            );
        }
        self
    }

    /// Адрес локального GitLab CE: `127.0.0.1:<порт>`, имя хоста контейнера.
    pub fn add_gitlab_ce(&mut self, address: &str) -> &mut Scrubber {
        self.add(Category::GitlabCe, LABEL_GITLAB_CE, address, true);
        self
    }

    /// Совпадения после слияния. `whole_words = false` ищет и логин внутри другого слова: так
    /// [`Scrubber::leaks`] видит то, что замена по границам слова оставила.
    fn spans(&self, text: &str, whole_words: bool) -> Vec<Span> {
        let lower = text.to_ascii_lowercase();
        let bytes = text.as_bytes();
        let mut spans = Vec::new();
        for rule in &self.rules {
            for needle in &rule.needles {
                for (start, _) in lower.match_indices(needle.as_str()) {
                    let end = start + needle.len();
                    if whole_words
                        && rule.bounded
                        && !(left_boundary(bytes, start) && right_boundary(bytes, end))
                    {
                        continue;
                    }
                    spans.push(Span {
                        start,
                        end,
                        category: rule.category,
                        label: rule.label,
                    });
                }
            }
        }
        token_spans(text, &mut spans);
        email_spans(bytes, &mut spans);
        avatar_spans(&lower, &mut spans);
        spans.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then(a.category.cmp(&b.category))
                .then((b.end - b.start).cmp(&(a.end - a.start)))
        });
        let mut merged: Vec<Span> = Vec::new();
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start < last.end => last.end = last.end.max(span.end),
                _ => merged.push(span),
            }
        }
        merged
    }

    pub fn scrub_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut pos = 0;
        for span in self.spans(text, true) {
            out.push_str(&text[pos..span.start]);
            out.push_str(span.label);
            pos = span.end;
        }
        out.push_str(&text[pos..]);
        out
    }

    /// Строка запроса или тело формы: значения секретных ключей заменяются целиком.
    pub fn scrub_params(&self, text: &str) -> String {
        text.split('&')
            .map(|param| match param.split_once('=') {
                Some((key, _)) if is_secret_key(key) => {
                    format!("{}={LABEL_TOKEN}", self.scrub_text(key))
                }
                Some((key, value)) => {
                    format!("{}={}", self.scrub_text(key), self.scrub_text(value))
                }
                None => self.scrub_text(param),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    pub fn scrub_url(&self, url: &str) -> String {
        match url.split_once('?') {
            Some((path, query)) => {
                format!("{}?{}", self.scrub_text(path), self.scrub_params(query))
            }
            None => self.scrub_text(url),
        }
    }

    pub fn scrub_header(&self, name: &str, value: &str) -> String {
        if is_secret_header(name) {
            LABEL_TOKEN.to_owned()
        } else {
            self.scrub_text(value)
        }
    }

    pub fn scrub_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.scrub_text(text)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.scrub_json(v)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, item) in map {
                    let item = match item {
                        Value::String(text) if is_secret_key(key) && !text.is_empty() => {
                            Value::String(LABEL_TOKEN.to_owned())
                        }
                        Value::String(text) if is_avatar_key(key) && !text.is_empty() => {
                            Value::String(LABEL_AVATAR.to_owned())
                        }
                        other => self.scrub_json(other),
                    };
                    out.insert(self.scrub_text(key), item);
                }
                Value::Object(out)
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        }
    }

    /// Тело запроса или ответа: JSON очищается по значениям, остальное — как строка формы.
    pub fn scrub_body(&self, body: &[u8]) -> Value {
        if body.is_empty() {
            return Value::Null;
        }
        match serde_json::from_slice::<Value>(body) {
            Ok(value) => self.scrub_json(&value),
            Err(_) => Value::String(self.scrub_params(&String::from_utf8_lossy(body))),
        }
    }

    /// Что из известного и образцов осталось в тексте: категории без самих значений. Для
    /// проверки каталога данных перед коммитом. Известные значения ищутся и внутри слов,
    /// поэтому проверка строже замены.
    pub fn leaks(&self, text: &str) -> Vec<&'static str> {
        let mut found: Vec<&'static str> = self
            .spans(text, false)
            .iter()
            .map(|span| span.category.describe())
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEYS.contains(&lower.as_str())
        || SECRET_KEY_SUFFIXES
            .iter()
            .any(|suffix| lower.len() > suffix.len() && lower.ends_with(suffix))
}

fn is_avatar_key(key: &str) -> bool {
    AVATAR_KEYS.contains(&key)
}

/// Формы значения в тексте следа: как есть, в процентной кодировке и с `\/` из JSON.
fn forms(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    let mut encoded = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '/' => encoded.push_str("%2F"),
            ':' => encoded.push_str("%3A"),
            '@' => encoded.push_str("%40"),
            '+' => encoded.push_str("%2B"),
            ' ' => encoded.push_str("%20"),
            other => encoded.push(other),
        }
    }
    let mut forms = vec![
        value.to_ascii_lowercase(),
        encoded.to_ascii_lowercase(),
        value.replace('/', "\\/").to_ascii_lowercase(),
    ];
    forms.sort();
    forms.dedup();
    forms
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn left_boundary(bytes: &[u8], start: usize) -> bool {
    if start == 0 || !is_word(bytes[start - 1]) {
        return true;
    }
    // `%2F` перед значением — граница, хотя `F` — буква.
    start >= 3
        && bytes[start - 3] == b'%'
        && bytes[start - 2].is_ascii_hexdigit()
        && bytes[start - 1].is_ascii_hexdigit()
}

fn right_boundary(bytes: &[u8], end: usize) -> bool {
    end == bytes.len() || !is_word(bytes[end])
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn token_spans(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    for prefix in TOKEN_PREFIXES {
        for (start, _) in text.match_indices(prefix) {
            let mut end = start + prefix.len();
            while end < bytes.len() && is_token_char(bytes[end]) {
                end += 1;
            }
            spans.push(Span {
                start,
                end,
                category: Category::Token,
                label: LABEL_TOKEN,
            });
        }
    }
}

/// Хеш почты после `gravatar.com/avatar/` в любой форме из [`GRAVATAR_PREFIXES`]; `lower` — текст
/// в нижнем регистре ASCII.
fn avatar_spans(lower: &str, spans: &mut Vec<Span>) {
    let bytes = lower.as_bytes();
    for prefix in GRAVATAR_PREFIXES {
        for (at, _) in lower.match_indices(prefix) {
            let start = at + prefix.len();
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end - start >= AVATAR_HASH_MIN {
                spans.push(Span {
                    start,
                    end,
                    category: Category::Avatar,
                    label: LABEL_AVATAR,
                });
            }
        }
    }
}

fn is_local_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

/// Адреса `local@domain.tld`, в том числе с `%40`. `git@host` (адрес SSH удалённого
/// репозитория) почтой не считается.
fn email_spans(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut at = 0;
    while at < bytes.len() {
        let separator = if bytes[at] == b'@' {
            1
        } else if bytes[at..].starts_with(b"%40") {
            3
        } else {
            at += 1;
            continue;
        };
        let mut start = at;
        while start > 0 && is_local_char(bytes[start - 1]) {
            start -= 1;
        }
        while start < at && bytes[start] == b'.' {
            start += 1;
        }
        let domain_start = at + separator;
        let mut end = domain_start;
        while end < bytes.len() && is_domain_char(bytes[end]) {
            end += 1;
        }
        while end > domain_start && matches!(bytes[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let local = &bytes[start..at];
        let domain = &bytes[domain_start..end];
        let valid_domain = domain.first().is_some_and(u8::is_ascii_alphanumeric)
            && domain.iter().rposition(|b| *b == b'.').is_some_and(|dot| {
                let tld = &domain[dot + 1..];
                tld.len() >= 2 && tld.iter().all(u8::is_ascii_alphabetic)
            });
        if !local.is_empty() && valid_domain && !local.eq_ignore_ascii_case(b"git") {
            spans.push(Span {
                start,
                end,
                category: Category::Email,
                label: LABEL_EMAIL,
            });
            at = end;
        } else {
            at += separator;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const AUTHOR: &str = "Alice-Dev";
    const REVIEWER: &str = "alice-dev-review";
    const REPO: &str = "Alice-Dev/shellio-review-sandbox";
    const GROUP_REPO: &str = "team-x/sub/shellio-review-sandbox";
    const TOKEN: &str = "ghp_AbCdEf0123456789AbCdEf0123456789AbCd";
    const PASSWORD: &str = "s3cret-Root-Pass";
    const CE: &str = "127.0.0.1:8929";

    fn scrubber() -> Scrubber {
        let mut scrubber = Scrubber::new();
        scrubber
            .add_token(TOKEN)
            .add_token(PASSWORD)
            .add_login(AUTHOR, LABEL_AUTHOR)
            .add_login(REVIEWER, LABEL_REVIEWER)
            .add_repo(REPO)
            .add_repo(GROUP_REPO)
            .add_gitlab_ce(CE);
        scrubber
    }

    /// Оракул задачи 16: исходных значений нет ни в какой форме и регистре.
    fn assert_clean(output: &str) {
        let lower = output.to_ascii_lowercase();
        for original in [
            AUTHOR,
            REVIEWER,
            REPO,
            "alice-dev%2fshellio",
            "team-x",
            TOKEN,
            PASSWORD,
            CE,
            "127.0.0.1%3a8929",
            "alice@example.com",
            "ghp_",
            "github_pat_",
            "glpat-",
            "GR1348941",
        ] {
            assert!(
                !lower.contains(&original.to_ascii_lowercase()),
                "осталось «{original}» в {output}"
            );
        }
    }

    #[test]
    fn tokens_known_and_by_prefix() {
        let s = scrubber();
        let text = format!(
            "Bearer {TOKEN}; new glpat-Xy_z9-AbC.01.def and github_pat_11ABC_def then ghp_ alone"
        );
        let out = s.scrub_text(&text);
        assert_eq!(
            out,
            "Bearer <token>; new <token> and <token> then <token> alone"
        );
        assert_clean(&out);
        assert_eq!(s.scrub_text(&format!("pw={PASSWORD}.")), "pw=<token>.");
    }

    #[test]
    fn logins_case_insensitive_whole_word_longest_first() {
        let s = scrubber();
        let out = s.scrub_text(
            "by alice-dev, reviewed by ALICE-DEV-REVIEW; @Alice-Dev. not alice-developer",
        );
        assert_eq!(
            out,
            "by <author>, reviewed by <reviewer>; @<author>. not alice-developer"
        );
    }

    #[test]
    fn repo_paths_plain_encoded_escaped_and_namespaces() {
        let s = scrubber();
        let out = s.scrub_text(
            "https://github.com/alice-dev/shellio-review-sandbox/pull/1 \
             /api/v4/projects/team-x%2Fsub%2Fshellio-review-sandbox/merge_requests \
             git@github.com:Alice-Dev/shellio-review-sandbox.git \
             https:\\/\\/gitlab.com\\/team-x\\/sub\\/shellio-review-sandbox \
             group team-x and team-x/sub",
        );
        assert_eq!(
            out,
            "https://github.com/<repo>/pull/1 \
             /api/v4/projects/<repo>/merge_requests \
             git@github.com:<repo>.git \
             https:\\/\\/gitlab.com\\/<repo> \
             group <namespace> and <namespace>"
        );
        assert!(out.contains("github.com") && out.contains("gitlab.com"));
        assert_clean(&out);
    }

    #[test]
    fn emails_plain_encoded_and_noreply() {
        let s = scrubber();
        let out = s.scrub_text(
            "alice@example.com, 1234+alice-dev@users.noreply.github.com, \
             ?author_email=alice%40example.com&x=1, user@localhost, @mention",
        );
        assert_eq!(
            out,
            "<email>, <email>, ?author_email=<email>&x=1, user@localhost, @mention"
        );
        assert_clean(&out);
    }

    #[test]
    fn gitlab_ce_address_in_url_and_encoded() {
        let s = scrubber();
        let out = s.scrub_url(
            "http://127.0.0.1:8929/api/v4/projects?redirect_uri=http%3A%2F%2F127.0.0.1%3A8929%2Fcb",
        );
        assert_eq!(
            out,
            "http://<gitlab-ce>/api/v4/projects?redirect_uri=http%3A%2F%2F<gitlab-ce>%2Fcb"
        );
        assert_eq!(
            s.scrub_text("http://127.0.0.1:89290/"),
            "http://127.0.0.1:89290/"
        );
        assert_clean(&out);
    }

    #[test]
    fn url_query_secret_keys_and_values() {
        let s = scrubber();
        let out = s.scrub_url(
            "/repos/alice-dev/shellio-review-sandbox/pulls?private_token=unknownvalue&state=open&author=Alice-Dev",
        );
        assert_eq!(
            out,
            "/repos/<repo>/pulls?private_token=<token>&state=open&author=<author>"
        );
        assert_clean(&out);
    }

    #[test]
    fn headers_secret_names_and_values() {
        let s = scrubber();
        assert_eq!(
            s.scrub_header("Authorization", "Bearer opaque"),
            LABEL_TOKEN
        );
        assert_eq!(s.scrub_header("PRIVATE-TOKEN", "opaque"), LABEL_TOKEN);
        assert_eq!(
            s.scrub_header(
                "link",
                "<https://api.github.com/repos/alice-dev/shellio-review-sandbox/pulls?page=2>; rel=\"next\""
            ),
            "<https://api.github.com/repos/<repo>/pulls?page=2>; rel=\"next\""
        );
        assert_eq!(s.scrub_header("x-ratelimit-remaining", "4999"), "4999");
    }

    #[test]
    fn json_values_keys_and_secret_keys() {
        let s = scrubber();
        let input = json!({
            "user": {"login": "alice-dev", "email": "alice@example.com", "id": 42},
            "full_name": REPO,
            "web_url": "http://127.0.0.1:8929/team-x/sub/shellio-review-sandbox",
            "access_token": "6f1e0b9c2d8a7f3e",
            "token": "glpat-zzzzzzzzzzzzzzzzzzzz",
            "password": PASSWORD,
            "body": "LGTM, cc @ALICE-DEV-REVIEW",
            "alice-dev": true,
            "count": 3,
            "nested": [{"refresh_token": "abc"}, "Alice-Dev"]
        });
        let out = s.scrub_json(&input);
        assert_eq!(
            out,
            json!({
                "user": {"login": "<author>", "email": "<email>", "id": 42},
                "full_name": "<repo>",
                "web_url": "http://<gitlab-ce>/<repo>",
                "access_token": "<token>",
                "token": "<token>",
                "password": "<token>",
                "body": "LGTM, cc @<reviewer>",
                "<author>": true,
                "count": 3,
                "nested": [{"refresh_token": "<token>"}, "<author>"]
            })
        );
        let text = out.to_string();
        assert_clean(&text);
        assert!(!text.contains("6f1e0b9c2d8a7f3e"));
    }

    /// Выдуманное значение в форме токена регистрации раннеров GitLab.
    const RUNNERS_TOKEN: &str = "GR1348941Zx9-AbCdEfGhIjKlMnOp";

    #[test]
    fn token_suffix_keys_and_runner_registration_prefix() {
        let s = scrubber();
        // `runners_token` и четыре ключа со словом не в конце — из ответа `GET /projects/:id`
        // владельцу в серии `t15-20261121`, остальные ключи придуманы; значения выдуманы.
        let body = json!({
            "id": 7,
            "path_with_namespace": GROUP_REPO,
            "runners_token": RUNNERS_TOKEN,
            "runner_token_expiration_interval": null,
            "ci_job_token_scope_enabled": false,
            "ci_push_repository_for_job_token_allowed": true,
            "ci_id_token_sub_claim_components": ["project_path", "ref_type", "ref"],
            "token_type": "bearer",
            "feed_token": "opaque-feed",
            "Webhook_Secret": "opaque-hook",
            "encrypted_password": "opaque-pw",
            "empty_token": "",
            "nested": [{"incoming_email_token": "opaque-mail"}],
            "description": format!("runner {RUNNERS_TOKEN} ok"),
        });
        let out = s.scrub_body(body.to_string().as_bytes());
        assert_eq!(
            out,
            json!({
                "id": 7,
                "path_with_namespace": "<repo>",
                "runners_token": "<token>",
                "runner_token_expiration_interval": null,
                "ci_job_token_scope_enabled": false,
                "ci_push_repository_for_job_token_allowed": true,
                "ci_id_token_sub_claim_components": ["project_path", "ref_type", "ref"],
                "token_type": "bearer",
                "feed_token": "<token>",
                "Webhook_Secret": "<token>",
                "encrypted_password": "<token>",
                "empty_token": "",
                "nested": [{"incoming_email_token": "<token>"}],
                "description": "runner <token> ok",
            })
        );
        let text = out.to_string();
        assert_clean(&text);
        assert!(!text.contains("opaque"), "{text}");
        assert!(s.leaks(&text).is_empty());
        assert_eq!(s.leaks(&format!("x {RUNNERS_TOKEN} y")), vec!["токен"]);
        assert_eq!(
            s.scrub_url(
                "/api/v4/projects/7?runners_token=opaque&per_page=100&ci_job_token_scope_enabled=1"
            ),
            "/api/v4/projects/7?runners_token=<token>&per_page=100&ci_job_token_scope_enabled=1"
        );
        assert_eq!(
            s.scrub_body(b"feed_token=opaque&token_type=bearer"),
            Value::String("feed_token=<token>&token_type=bearer".to_owned())
        );
    }

    #[test]
    fn form_body_secret_keys() {
        let s = scrubber();
        let out = s.scrub_body(b"grant_type=password&username=root&password=unseen-value");
        assert_eq!(
            out,
            Value::String("grant_type=password&username=root&password=<token>".to_owned())
        );
        assert_eq!(s.scrub_body(b""), Value::Null);
        assert_eq!(
            s.scrub_body(br#"{"login":"alice-dev"}"#),
            json!({"login": "<author>"})
        );
    }

    #[test]
    fn overlapping_matches_leave_no_tail() {
        let mut s = Scrubber::new();
        s.add_login("alice", LABEL_AUTHOR).add_token("ce.secret");
        // Логин начинается раньше и побеждает, но покрывает и хвост токена.
        assert_eq!(s.scrub_text("x alice.secret y"), "x <author> y");
        let mut s = Scrubber::new();
        s.add_login("author", LABEL_AUTHOR)
            .add_login("reviewer", LABEL_REVIEWER);
        // Метка не проходит повторно через правила.
        assert_eq!(s.scrub_text("author reviewer"), "<author> <reviewer>");
    }

    #[test]
    fn public_hosts_and_unicode_survive() {
        let s = scrubber();
        let text = "https://github.com/ и https://gitlab.com/ — комментарий alice-dev ✓";
        assert_eq!(
            s.scrub_text(text),
            "https://github.com/ и https://gitlab.com/ — комментарий <author> ✓"
        );
    }

    #[test]
    fn leaks_reports_categories_without_values() {
        let s = scrubber();
        let dirty = format!("{TOKEN} alice@example.com {REPO} {CE} alice-dev");
        let leaks = s.leaks(&dirty);
        assert_eq!(leaks.len(), 5, "{leaks:?}");
        assert!(leaks.iter().all(|leak| !leak.contains("alice")));
        assert!(s.leaks(&s.scrub_text(&dirty)).is_empty());
    }

    #[test]
    fn leaks_find_login_inside_word_that_replacement_keeps() {
        let s = scrubber();
        let text = "bot alice-dev_bot and alice-developer";
        assert_eq!(s.scrub_text(text), text);
        assert_eq!(s.leaks(text), vec!["логин"]);
    }

    const SHA256_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const MD5_HASH: &str = "FEDCBA9876543210FEDCBA9876543210";

    #[test]
    fn gravatar_hash_plain_escaped_encoded_and_md5() {
        let s = Scrubber::new();
        let out = s.scrub_text(&format!(
            "https://www.gravatar.com/avatar/{SHA256_HASH}?s=80&d=identicon \
             https:\\/\\/secure.gravatar.com\\/avatar\\/{MD5_HASH} \
             ?u=https%3A%2F%2Fwww.gravatar.com%2Favatar%2F{SHA256_HASH}%3Fs%3D80 \
             gravatar.com/avatar/abc123 https://example.com/avatar/{SHA256_HASH}"
        ));
        assert_eq!(
            out,
            format!(
                "https://www.gravatar.com/avatar/<avatar>?s=80&d=identicon \
                 https:\\/\\/secure.gravatar.com\\/avatar\\/<avatar> \
                 ?u=https%3A%2F%2Fwww.gravatar.com%2Favatar%2F<avatar>%3Fs%3D80 \
                 gravatar.com/avatar/abc123 https://example.com/avatar/{SHA256_HASH}"
            )
        );
        assert_eq!(s.leaks(&out), Vec::<&str>::new());
        assert_eq!(
            s.leaks(&format!("gravatar.com/avatar/{MD5_HASH}")),
            vec!["хеш почты в адресе аватара"]
        );
    }

    #[test]
    fn avatar_keys_replaced_whole_and_hash_elsewhere() {
        let s = scrubber();
        let input = json!({
            "author": {
                "username": "alice-dev",
                "avatar_url": format!("https://www.gravatar.com/avatar/{SHA256_HASH}?s=80&d=identicon"),
            },
            "user": {"avatar_url": "https://avatars.githubusercontent.com/u/123456?v=4", "gravatar_id": ""},
            "viewer": {"avatarUrl": "https://avatars.githubusercontent.com/u/123456?v=4"},
            "owner": {"gravatar_id": MD5_HASH, "avatar_url": null},
            "body": format!("see https://www.gravatar.com/avatar/{SHA256_HASH}"),
        });
        let out = s.scrub_json(&input);
        assert_eq!(
            out,
            json!({
                "author": {"username": "<author>", "avatar_url": "<avatar>"},
                "user": {"avatar_url": "<avatar>", "gravatar_id": ""},
                "viewer": {"avatarUrl": "<avatar>"},
                "owner": {"gravatar_id": "<avatar>", "avatar_url": null},
                "body": "see https://www.gravatar.com/avatar/<avatar>",
            })
        );
        let text = out.to_string().to_ascii_lowercase();
        assert!(!text.contains(SHA256_HASH) && !text.contains(&MD5_HASH.to_ascii_lowercase()));
        assert!(!text.contains("123456"));
        assert_clean(&text);
    }

    #[test]
    fn empty_values_are_ignored() {
        let mut s = Scrubber::new();
        s.add_token("").add_login("  ", LABEL_AUTHOR).add_repo("");
        assert_eq!(s.scrub_text("text stays"), "text stays");
    }
}
