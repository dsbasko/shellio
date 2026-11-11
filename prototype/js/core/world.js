// Единый мир данных прототипа. Все панели читают и меняют только его.
window.W = {};
(() => {
  W.workspaces = [
    { id: 'work', title: 'Работа', position: 0, deletedAt: null },
    { id: 'personal', title: 'Личное', position: 1, deletedAt: null },
  ];
  // Проект принадлежит одному воркспейсу, корень одному проекту.
  W.newProject = (id, ws, over = {}) => ({ id, title: id, ws, repos: [], roots: [], workbenches: [], listPos: null, removedAt: null, lastOpenedAt: 0, lastWb: null, ...over });
  W.projects = {
    orbit: W.newProject('orbit', 'work', { repos: ['orbit-api', 'orbit-web'], roots: ['~/src/orbit-api', '~/src/orbit-web'], workbenches: ['feature-sso', 'review', 'infra'], listPos: 0, lastOpenedAt: 9000, lastWb: 'feature-sso' }),
    atlas: W.newProject('atlas', 'work', { repos: ['atlas'], roots: ['~/src/atlas'], workbenches: ['atlas-main'], listPos: 3, lastOpenedAt: 8000, lastWb: 'atlas-main' }),
    dotfiles: W.newProject('dotfiles', 'personal', { repos: ['dotfiles'], roots: ['~/dotfiles'], workbenches: ['dotfiles-main'], listPos: 0, lastOpenedAt: 9000, lastWb: 'dotfiles-main' }),
  };
  W.repos = {
    'orbit-api': { id: 'orbit-api', provider: 'gitlab', host: 'gitlab.com', path: '~/src/orbit-api', branch: 'feature/sso-login', head: 'a3f9c21' },
    'orbit-web': { id: 'orbit-web', provider: 'github', host: 'github.com', path: '~/src/orbit-web', branch: 'perf/session-cache', head: 'd8a20f1' },
    atlas: { id: 'atlas', provider: 'gitlab', host: 'git.atlas.dev', path: '~/src/atlas', branch: 'main', head: '4de2a10' },
    dotfiles: { id: 'dotfiles', provider: 'github', host: 'github.com', path: '~/dotfiles', branch: 'main', head: '90aa1f3' },
  };
  W.workbenches = {
    'feature-sso': { id: 'feature-sso', title: 'feature-sso', project: 'orbit', repo: 'orbit-api', branch: 'feature/sso-login', worktree: '~/src/orbit-api' },
    review: { id: 'review', title: 'review', project: 'orbit', repo: 'orbit-api', branch: 'main', worktree: '~/src/orbit-api-review' },
    infra: { id: 'infra', title: 'infra', project: 'orbit', repo: 'orbit-web', branch: 'main', worktree: '~/src/orbit-web' },
    'atlas-main': { id: 'atlas-main', title: 'main', project: 'atlas', repo: 'atlas', branch: 'main', worktree: '~/src/atlas' },
    'dotfiles-main': { id: 'dotfiles-main', title: 'main', project: 'dotfiles', repo: 'dotfiles', branch: 'main', worktree: '~/dotfiles' },
  };

  const coldProjects = (ws, names) => names.forEach((name, i) => {
    const gh = i % 3 === 0;
    W.repos[name] = { id: name, provider: gh ? 'github' : 'gitlab', host: gh ? 'github.com' : 'gitlab.com', path: `~/src/${name}`, branch: 'main', head: (0x1a2b3c0 + i * 4099).toString(16).slice(0, 7) };
    W.projects[name] = W.newProject(name, ws, { repos: [name], roots: [`~/src/${name}`] });
  });
  coldProjects('work', ['analytics-pipeline', 'api-gateway', 'auth-service', 'billing-core', 'catalog-sync', 'checkout-web', 'ci-templates', 'cms-admin', 'cms-admin-old', 'data-warehouse', 'design-tokens', 'docs-portal', 'email-worker', 'event-bus', 'experiments-2023', 'feature-flags', 'fraud-rules', 'helm-charts', 'infra-terraform', 'inventory-api', 'k8s-operators', 'ledger', 'legacy-billing', 'loadtest', 'logging-stack', 'mobile-android', 'mobile-ios', 'notifications', 'old-admin', 'onboarding-flow', 'partner-portal', 'payments-gateway', 'pricing-engine', 'recommendations', 'reporting', 'search-index', 'session-store', 'shipping-api', 'sso-proxy', 'status-page', 'support-desk', 'tax-rules', 'user-profile', 'webhooks']);
  coldProjects('personal', ['blog', 'budget-tracker', 'home-lab', 'recipes', 'sketchbook']);
  Object.assign(W.projects['payments-gateway'], { listPos: 1, lastOpenedAt: 7000, blockers: [
    { id: 'pay-task', kind: 'Поручение', title: 'Поручение проекту billing-core', sub: 'Агент claude-pay ждёт ответа', peer: 'billing-core' },
    { id: 'pay-mr', kind: 'Публикация', title: 'MR !318: отправка ревью', sub: 'Запрос выполняется', peer: null },
  ] });
  Object.assign(W.projects['api-gateway'], { listPos: 2, lastOpenedAt: 6500 });
  W.projects['billing-core'].lastOpenedAt = 5000;
  W.projects['inventory-api'].lastOpenedAt = 4000;
  W.projects['search-index'].lastOpenedAt = 3000;
  W.projects['cms-admin-old'].removedAt = 1;
  W.projects['status-page'].missing = true;

  W.sessions = {
    'claude-sso': { id: 'claude-sso', kind: 'agent', agent: 'claude', profile: 'Claude Code', title: 'claude-sso', wb: 'feature-sso', state: 'running', host: 'ok', gen: 1, cpu: 18, mem: 430, cwd: '~/src/orbit-api' },
    'codex-tests': { id: 'codex-tests', kind: 'agent', agent: 'codex', profile: 'Codex', title: 'codex-tests', wb: 'feature-sso', state: 'waiting', host: 'ok', gen: 1, cpu: 6, mem: 310, cwd: '~/src/orbit-api' },
    'claude-docs': { id: 'claude-docs', kind: 'agent', agent: 'claude', profile: 'Claude Code', title: 'claude-docs', wb: 'feature-sso', state: 'idle', host: 'ok', gen: 1, cpu: 0.1, mem: 122, cwd: '~/src/orbit-api' },
    zsh: { id: 'zsh', kind: 'term', title: 'zsh', wb: 'feature-sso', state: 'idle', host: 'ok', gen: 1, cpu: 0.1, mem: 26, cwd: '~/src/orbit-api' },
    'make-test': { id: 'make-test', kind: 'term', title: 'make test', wb: 'feature-sso', state: 'running', host: 'ok', gen: 1, cpu: 22.1, mem: 1020, cwd: '~/src/orbit-api', background: true },
    nvim: { id: 'nvim', kind: 'editor', title: 'nvim', wb: 'feature-sso', state: 'idle', host: 'ok', gen: 1, cpu: 0.4, mem: 92, cwd: '~/src/orbit-api' },
  };

  W.resources = { shared: { cpu: 1.6, mem: 240 }, service: { cpu: 0.6, mem: 160 }, cores: 16 };
  W.estimates = [
    { project: 'orbit', id: 'git', title: 'Git', what: 'Кэши двух репозиториев', cpu: 0.3, mem: 12 },
    { project: 'orbit', id: 'review', title: 'Ревью !142', what: 'Diff и треды', cpu: 0.1, mem: 8.6 },
    { project: 'orbit', id: 'pipelines', title: 'Пайплайны', what: 'Снимки запусков и логи', cpu: 0.1, mem: 6.8 },
    { project: 'orbit', id: 'changes', title: 'Изменения', what: 'Index и hunks', cpu: 0.1, mem: 4.2 },
    { project: 'orbit', id: 'history', title: 'История', what: 'Граф и коммиты', cpu: 0.1, mem: 2.1 },
  ];

  W.people = ['m.ivanova', 'a.petrov', 's.kim'];

  W.commits = {
    a3f9c21: { sha: 'a3f9c21', title: 'Добавить обмен кода на токен', author: 'Дмитрий', age: 'сегодня', when: 'Сегодня, 11:42', parent: '7d10e4b', parents: ['7d10e4b'], body: 'Обмен кода провайдера на токен, проверка claims, создание сессии и cookie состояния после проверки.', files: ['src/auth/sso.rs', 'src/auth/session.rs', 'src/routes/login.rs'] },
    '7d10e4b': { sha: '7d10e4b', title: 'Вынести проверку состояния', author: 'Дмитрий', age: 'вчера', when: 'Вчера, 18:06', parent: '19cd5e0', parents: ['19cd5e0'], body: 'Callback получает сессию, у сессии появляется провайдер входа. Проверка state будет опираться на неё.', files: ['src/auth/sso.rs', 'src/auth/session.rs'] },
    '19cd5e0': { sha: '19cd5e0', title: 'Миграция таблицы sso', author: 's.kim', age: '4 дня', when: '7 ноября, 16:20', parent: '5c2d110', parents: ['5c2d110'], body: 'Таблица sso_sessions, индексы по пользователю и сроку действия.', files: ['migrations/0042_sso.sql'] },
  };

  W.commits['0000000'] = { sha: '0000000', title: 'Не закоммичено', author: 'claude-sso', age: 'сейчас', when: 'Сейчас', parent: 'a3f9c21', body: 'Строка изменена в рабочей копии и ещё не вошла в коммит.', files: ['src/auth/sso.rs'], uncommitted: true };

  // История репозиториев для панели «История»: feature/sso-login ответвилась от v0.4.0, main ушла вперёд со слиянием.
  const commit = (sha, title, author, age, when, parents, files, body) => ({ sha, title, author, age, when, parent: parents[0] || null, parents, files, body });
  Object.assign(W.commits, {
    '8e6d3f4': commit('8e6d3f4', 'Обновить axum до 0.8', 's.kim', 'сегодня', 'Сегодня, 09:15', ['b72c0e9'], ['Cargo.toml', 'src/routes/login.rs'], 'Новая версия axum и правки извлечения параметров запроса.'),
    b72c0e9: commit('b72c0e9', "Merge branch 'fix/cookie-samesite'", 'm.ivanova', 'вчера', 'Вчера, 12:30', ['5c2d110', '4a91d07'], ['src/auth/session.rs'], 'Слияние исправления cookie сессии.'),
    '4a91d07': commit('4a91d07', 'Cookie сессии с SameSite=Lax', 'a.petrov', '3 дня', '8 ноября, 10:02', ['5c2d110'], ['src/auth/session.rs'], 'Cookie сессии больше не уходит в сторонние запросы. Вход через провайдера по-прежнему работает: возврат идёт верхнеуровневым переходом.'),
    '5c2d110': commit('5c2d110', 'Выпуск 0.4.0', 'm.ivanova', '5 дней', '6 ноября, 17:40', ['2f8a6e1'], ['CHANGELOG.md', 'Cargo.toml'], 'Версия 0.4.0 и список изменений.'),
    '2f8a6e1': commit('2f8a6e1', 'Логировать ошибки провайдера', 'Дмитрий', 'неделю назад', '4 ноября, 13:12', ['9b0c3d5'], ['src/auth/oidc.rs'], 'Модуль provider переименован в oidc. Ошибки провайдера пишутся в журнал.'),
    '9b0c3d5': commit('9b0c3d5', 'Каркас модуля auth', 'Дмитрий', '2 недели назад', '28 октября, 11:05', [], ['src/auth/provider.rs', 'src/auth/session.rs', 'src/auth/sso.rs', 'src/routes/login.rs'], 'Модуль auth: клиент провайдера, сессия и маршрут входа.'),
    d8a20f1: commit('d8a20f1', 'Кэшировать сессии в памяти', 's.kim', 'вчера', 'Вчера, 15:48', ['ae1102c'], ['src/cache/session.ts', 'README.md'], 'Повторное чтение сессии берётся из памяти процесса.'),
    ae1102c: commit('ae1102c', 'Выпуск 2.3.0', 'a.petrov', '4 дня', '7 ноября, 12:00', [], ['CHANGELOG.md'], 'Версия 2.3.0.'),
  });
  // Переименования: новый путь → прежний путь в родителе.
  W.commitRenames = { '2f8a6e1': { 'src/auth/oidc.rs': 'src/auth/provider.rs' } };
  W.graphs = {
    'orbit-api': {
      order: ['a3f9c21', '8e6d3f4', '7d10e4b', 'b72c0e9', '4a91d07', '19cd5e0', '5c2d110', '2f8a6e1', '9b0c3d5'],
      branches: { 'feature/sso-login': 'a3f9c21', main: '8e6d3f4' },
      remotes: { 'origin/feature/sso-login': '7d10e4b', 'origin/main': '8e6d3f4' },
      tags: { 'v0.4.0': '5c2d110' },
    },
    'orbit-web': { order: ['d8a20f1', 'ae1102c'], branches: { 'perf/session-cache': 'd8a20f1', main: 'ae1102c' }, remotes: { 'origin/main': 'ae1102c' }, tags: { 'v2.3.0': 'ae1102c' } },
  };
  // Остальные репозитории получают короткую прямую историю от своего HEAD.
  W.graphOf = (repoId) => {
    if (W.graphs[repoId]) return W.graphs[repoId];
    const repo = W.repos[repoId]; if (!repo) return { order: [], branches: {}, remotes: {}, tags: {} };
    const titles = ['Обновить зависимости', 'Исправить сборку на macOS', 'Добавить тесты конфигурации', 'Описать установку в README', 'Начальная структура'];
    const order = titles.map((_, i) => (i ? (parseInt(repo.head, 16) ^ (i * 0x9e3779)).toString(16).padStart(7, '0').slice(-7) : repo.head));
    order.forEach((sha, i) => { if (!W.commits[sha]) W.commits[sha] = commit(sha, titles[i], W.people[i % W.people.length], `${i + 1} дн.`, `${11 - i} ноября`, order[i + 1] ? [order[i + 1]] : [], ['README.md'], `${titles[i]}.`); });
    W.changesets = W.changesets || {};
    order.slice().reverse().forEach((sha, k) => { if (!W.changesets[sha]) W.changesets[sha] = { 'README.md': [`# ${repoId}`, '', ...titles.slice().reverse().slice(0, k + 1)] }; });
    return (W.graphs[repoId] = { order, branches: { [repo.branch]: repo.head }, remotes: { [`origin/${repo.branch}`]: repo.head }, tags: {} });
  };

  W.orbitBuffers = {
    'src/auth/sso.rs': { lang: 'rust', lines: `use axum::{extract::Query, response::Redirect};
use crate::auth::{Session, SsoClient, SsoError};
use crate::config::SsoConfig;

#[derive(serde::Deserialize)]
pub struct SsoCallback {
    pub code: String,
    pub state: String,
}

pub async fn callback(
    session: Session,
    client: SsoClient,
    Query(query): Query<SsoCallback>,
) -> Result<Redirect, SsoError> {
    let expected_state = session.take_sso_state().await?;
    verify_state(&expected_state, &query.state)?;

    let token = client.exchange_code(&query.code).await?;
    let identity = client.fetch_identity(&token).await?;
    session.sign_in(identity).await?;

    Ok(Redirect::to("/dashboard"))
}

fn verify_state(expected: &str, received: &str) -> Result<(), SsoError> {
    use subtle::ConstantTimeEq;

    if expected.is_empty() || expected.as_bytes().ct_eq(received.as_bytes()).unwrap_u8() != 1 {
        return Err(SsoError::InvalidState);
    }

    Ok(())
}

pub async fn complete_login(
    state: &AppState,
    claims: IdTokenClaims,
) -> Result<Session, AuthError> {
    verify_claims(&claims, &state.sso)?;
    let expires_at = claims.expires_at();
    let session = Session {
        user_id: claims.sub,
        expires_at: expires_at.min(Utc::now() + SESSION_TTL),
        created_at: Utc::now(),
    };
    state.sessions.insert(&session).await?;
    Ok(session)
}`.split('\n'), origin: [0, 0, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 2, 0, 0, 0, 2, 0, 0, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 2, 0, 0, 0, 0, 0, 0, 1, 1, 3, 1, 1, 1, 1, 1] },
    'src/auth/session.rs': { lang: 'rust', lines: `use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

const SESSION_TTL: Duration = Duration::hours(8);

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub provider: Option<String>,
}

impl Session {
    pub fn from_sso(user_id: Uuid, expires_at: DateTime<Utc>) -> Self {
        Self {
            expires_at: expires_at.min(Utc::now() + SESSION_TTL),
            provider: Some("sso".into()),
            ..Self::new(user_id)
        }
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }
}`.split('\n'), origin: [] },
    'src/routes/login.rs': { lang: 'rust', lines: `pub async fn login(session: Session, client: SsoClient) -> Result<Redirect, AppError> {
    let (authorize_url, csrf_token) = client.authorize_url();
    session.insert("sso_state", csrf_token.secret()).await?;
    session.save().await?;

    Ok(Redirect::to(authorize_url.as_str()))
}`.split('\n'), origin: [] },
    'migrations/0042_sso.sql': { lang: 'sql', lines: `CREATE TABLE sso_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id),
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sso_provider_subject_key UNIQUE (provider, subject)
);

CREATE INDEX sso_sessions_user_id_idx ON sso_sessions(user_id);
CREATE INDEX sso_sessions_expires_at_idx ON sso_sessions(expires_at);`.split('\n'), origin: [] },
    'README.md': { lang: null, lines: `# orbit-api

Сервис авторизации и сессий.

## Вход через SSO

1. Настройте провайдера в \`config/sso.toml\`.
2. Запустите \`make dev\`.
3. Откройте \`/login\`.`.split('\n'), origin: [] },
  };

  const L = (t, o, n, s) => ({ t, o, n, s });
  W.diffs = {
    'src/cache/session.ts': [
      { header: '@@ -8,4 +8,6 @@ export function readSession(', lines: [
        L(' ', 8, 8, '  const key = sessionKey(userId);'),
        L('-', 9, null, '  return store.get(key);'),
        L('+', null, 9, '  const cached = memo.get(key);'),
        L('+', null, 10, '  if (cached) return cached;'),
        L('+', null, 11, '  return store.get(key).then((v) => memo.set(key, v));'),
        L(' ', 10, 12, '}'),
      ] },
    ],
    'src/auth/sso.rs': [],
    'src/auth/session.rs': [
      { header: '@@ -12,6 +12,14 @@ impl Session', lines: [
        L(' ', 12, 12, ''),
        L(' ', 13, 13, 'impl Session {'),
        L('+', null, 14, '    pub fn from_sso(user_id: Uuid, expires_at: DateTime<Utc>) -> Self {'),
        L('+', null, 15, '        Self {'),
        L('+', null, 16, '            expires_at: expires_at.min(Utc::now() + SESSION_TTL),'),
        L('+', null, 17, '            provider: Some("sso".into()),'),
        L('+', null, 18, '            ..Self::new(user_id)'),
        L('+', null, 19, '        }'),
        L('+', null, 20, '    }'),
        L('-', 14, null, '    // TODO: срок действия для внешних провайдеров'),
        L(' ', 15, 21, ''),
        L(' ', 16, 22, '    pub fn is_expired(&self) -> bool {'),
      ] },
    ],
    'src/routes/login.rs': [
      { header: '@@ -1,5 +1,4 @@', lines: [
        L(' ', 1, 1, 'pub async fn login(session: Session, client: SsoClient) -> Result<Redirect, AppError> {'),
        L('-', 2, null, '    tracing::debug!("начало входа");'),
        L(' ', 3, 2, '    let (authorize_url, csrf_token) = client.authorize_url();'),
        L(' ', 4, 3, '    session.insert("sso_state", csrf_token.secret()).await?;'),
        L(' ', 5, 4, '    session.save().await?;'),
      ] },
    ],
    'migrations/0042_sso.sql': [
      { header: '@@ -0,0 +1,12 @@', lines: W.orbitBuffers['migrations/0042_sso.sql'].lines.map((s, i) => L('+', null, i + 1, s)) },
    ],
    'README.md': [
      { header: '@@ -3,3 +3,9 @@', lines: [
        L(' ', 3, 3, 'Сервис авторизации и сессий.'),
        L('+', null, 4, ''),
        L('+', null, 5, '## Вход через SSO'),
        L('+', null, 6, ''),
        L('+', null, 7, '1. Настройте провайдера в `config/sso.toml`.'),
        L('+', null, 8, '2. Запустите `make dev`.'),
        L('+', null, 9, '3. Откройте `/login`.'),
      ] },
    ],
    'src/auth/oidc.rs': [
      { header: '@@ -1,4 +1,4 @@', lines: [
        L('-', 1, null, 'pub mod sso_legacy;'),
        L('+', null, 1, 'pub mod oidc_client;'),
        L(' ', 2, 2, ''),
        L(' ', 3, 3, 'pub use oidc_client::OidcClient;'),
      ] },
    ],
  };

  // Версии sso.rs для «Истории»: рабочая копия → HEAD (a3f9c21) → 7d10e4b → 9b0c3d5. Ханки между ними согласованы с буфером.
  {
    const B = W.orbitBuffers['src/auth/sso.rs'].lines;
    const head = [...B.slice(0, 15), B[18], ...B.slice(19, 25), ...B.slice(35, 49)];
    head[30] = '        expires_at,';
    const v7 = [head[0], head[1], ...head.slice(3, 21)];
    const v9 = v7.filter((_, i) => i !== 10 && i !== 16);
    const add = (from, list, n0) => list.map((x, i) => L('+', null, n0 + i, x));
    W.diffs['src/auth/sso.rs'] = [
      { header: '@@ -14,5 +14,8 @@ pub async fn callback(', lines: [L(' ', 14, 14, B[13]), L(' ', 15, 15, B[14]), L('-', 16, null, head[15]), ...add(0, B.slice(15, 19), 16), L(' ', 17, 20, B[19]), L(' ', 18, 21, B[20])] },
      { header: '@@ -21,3 +24,13 @@', lines: [L(' ', 21, 24, B[23]), L(' ', 22, 25, B[24]), ...add(0, B.slice(25, 35), 26), L(' ', 23, 36, B[35])] },
      { header: '@@ -30,3 +43,3 @@ pub async fn complete_login(', lines: [L(' ', 30, 43, B[42]), L('-', 31, null, head[30]), L('+', null, 44, B[43]), L(' ', 32, 45, B[44])] },
    ];
    // Снимки файлов по коммитам. Изменения коммита заданы относительно первого родителя, у слияния тоже.
    const E9 = ['use chrono::{DateTime, Utc};', 'use uuid::Uuid;', '', '#[derive(Debug, Clone)]', 'pub struct Session {', '    pub id: Uuid,', '    pub user_id: Uuid,', '    pub expires_at: DateTime<Utc>,', '}', '', 'impl Session {', '    pub fn is_expired(&self) -> bool {', '        Utc::now() >= self.expires_at', '    }', '}'];
    const E4 = [...E9.slice(0, 11), "    pub fn cookie(&self) -> Cookie<'static> {", '        Cookie::build(("sid", self.id.to_string())).same_site(SameSite::Lax).build()', '    }', '', ...E9.slice(11)];
    const E7 = [...E9.slice(0, 8), '    pub provider: Option<String>,', ...E9.slice(8)];
    const SB = W.orbitBuffers['src/auth/session.rs'].lines;
    const EH = [...SB.slice(0, 14), '    // TODO: срок действия для внешних провайдеров', ...SB.slice(21)];
    const L9 = ['pub async fn login(session: Session, client: SsoClient) -> Result<Redirect, AppError> {', '    tracing::debug!("начало входа");', '    let authorize_url = client.authorize_url();', '    Ok(Redirect::to(authorize_url.as_str()))', '}'];
    const LB = W.orbitBuffers['src/routes/login.rs'].lines;
    const LH = [LB[0], L9[1], ...LB.slice(1)];
    const L8 = ['pub async fn login(State(client): State<SsoClient>, session: Session) -> Result<Redirect, AppError> {', ...L9.slice(1)];
    const C9 = ['[package]', 'name = "orbit-api"', 'version = "0.3.2"', 'edition = "2021"', '', '[dependencies]', 'axum = "0.7"', 'tokio = { version = "1", features = ["full"] }'];
    const C5 = C9.map((x) => (x.startsWith('version') ? 'version = "0.4.0"' : x));
    const C8 = C5.map((x) => (x.startsWith('axum') ? 'axum = "0.8"' : x));
    const K9 = ['# Изменения', ''];
    const K5 = ['# Изменения', '', '## 0.4.0', '', '- Журнал ошибок провайдера.', '- Модуль provider переименован в oidc.', ''];
    const P9 = ['pub mod sso_legacy;', '', 'pub use oidc_client::OidcClient;'];
    const RB = W.orbitBuffers['README.md'].lines;
    const T1 = ["import { store } from './store';", '', 'export function readSession(userId: string) {', '  const key = sessionKey(userId);', '  return store.get(key);', '}'];
    W.changesets = Object.assign(W.changesets || {}, {
      '9b0c3d5': { 'Cargo.toml': C9, 'CHANGELOG.md': K9, 'README.md': RB.slice(0, 3), 'src/auth/provider.rs': P9, 'src/auth/session.rs': E9, 'src/auth/sso.rs': v9, 'src/routes/login.rs': L9 },
      '2f8a6e1': { 'src/auth/provider.rs': null, 'src/auth/oidc.rs': [...P9, '', 'pub fn log_provider_error(err: &ProviderError) {', '    tracing::warn!(error = %err, "ошибка провайдера");', '}'] },
      '5c2d110': { 'Cargo.toml': C5, 'CHANGELOG.md': K5 },
      '19cd5e0': { 'migrations/0042_sso.sql': W.orbitBuffers['migrations/0042_sso.sql'].lines },
      '7d10e4b': { 'src/auth/sso.rs': v7, 'src/auth/session.rs': E7 },
      a3f9c21: { 'src/auth/sso.rs': head, 'src/auth/session.rs': EH, 'src/routes/login.rs': LH },
      '4a91d07': { 'src/auth/session.rs': E4 },
      b72c0e9: { 'src/auth/session.rs': E4 },
      '8e6d3f4': { 'Cargo.toml': C8, 'src/routes/login.rs': L8 },
      ae1102c: { 'CHANGELOG.md': ['# Изменения', '', '## 2.3.0'], 'README.md': ['# orbit-web', '', 'Кэш сессий веб-клиента.'], 'src/cache/session.ts': T1 },
      d8a20f1: { 'README.md': ['# orbit-web', '', 'Кэш сессий веб-клиента.', '', 'Повторное чтение сессии берётся из памяти процесса.'], 'src/cache/session.ts': ["import { memo } from './memo';", ...T1] },
    });
  }
  // Дерево ревизии: дерево первого родителя с изменениями коммита.
  const trees = new Map();
  W.treeOf = (sha) => {
    if (!sha) return {};
    if (trees.has(sha)) return trees.get(sha);
    const c = W.commits[sha], first = c && (c.parents || [c.parent])[0];
    const t = { ...W.treeOf(first) };
    Object.entries(W.changesets[sha] || {}).forEach(([k, v]) => { if (v == null) delete t[k]; else t[k] = v; });
    trees.set(sha, t); return t;
  };
  // Файлы коммита следуют из его изменений; старый путь переименования отдельно не показывается.
  Object.entries(W.changesets).forEach(([sha, ch]) => {
    const from = new Set(Object.values(W.commitRenames[sha] || {}));
    if (W.commits[sha]) W.commits[sha].files = Object.keys(ch).filter((k) => !(ch[k] == null && from.has(k)));
  });
  // nvim открывает историческую ревизию из того же снимка.
  Object.entries(W.orbitBuffers).forEach(([path, b]) => {
    b.revisions = {};
    W.graphs['orbit-api'].order.forEach((sha) => { const v = W.treeOf(sha)[path]; if (v) b.revisions[sha] = v; });
  });

  const changesSeed = (over = {}) => ({ detached: false, conflict: false, files: [], msg: { text: '', state: 'empty', streaming: false, indexAt: 0 }, indexRev: 1, head: null, lastCommit: null, hookFails: false, ...over });
  // Index и рабочая копия принадлежат каталогу: области одного worktree видят одно состояние.
  W.worktrees = {
    '~/src/orbit-api': changesSeed({ head: 'a3f9c21', files: [
      { path: 'src/auth/sso.rs', status: 'M', staged: true },
      { path: 'src/auth/session.rs', status: 'M', staged: true },
      { path: 'migrations/0042_sso.sql', status: 'A', staged: false, by: 'claude-sso' },
      { path: 'src/routes/login.rs', status: 'M', staged: false },
      { path: 'README.md', status: 'M', staged: false },
    ] }),
    '~/src/orbit-web': changesSeed({ head: 'd8a20f1', files: [{ path: 'src/cache/session.ts', status: 'M', staged: false }] }),
  };
  W.changesOf = (wb) => { if (!W.workbenches[wb]) return changesSeed(); const t = W.workbenches[wb].worktree; return W.worktrees[t] || (W.worktrees[t] = changesSeed()); };
  W.changes = new Proxy({}, { get: (_, k) => W.changesOf(S.st.route.wb)[k], set: (_, k, v) => { W.changesOf(S.st.route.wb)[k] = v; return true; } });

  W.mrs = {
    '!142': {
      id: '!142', provider: 'gitlab', repo: 'orbit-api', title: 'Вход через SSO', author: 'm.ivanova', branch: 'feature/sso-login', target: 'main', state: 'opened', draft: false,
      head: 'a3f9c21', base: '5c2d110', commits: ['a3f9c21', '7d10e4b', '19cd5e0'], reviewers: [{ who: 'a.petrov', state: 'you' }, { who: 's.kim', state: 'approved' }], assignees: ['m.ivanova'], labels: ['auth', 'backend'],
      description: 'Добавляет вход через корпоративный OIDC-провайдер. После callback создаём сессию и возвращаем пользователя на исходную страницу.',
      pipeline: 8842, url: 'https://gitlab.com/orbit/orbit-api/-/merge_requests/142',
      files: ['src/auth/sso.rs', 'src/auth/session.rs', 'src/routes/login.rs', 'migrations/0042_sso.sql', 'README.md'],
      viewed: { 'src/routes/login.rs': true, 'migrations/0042_sso.sql': true },
      threads: [
        { id: 't1', file: 'src/auth/sso.rs', line: 17, who: 's.kim', when: 'Сегодня, 16:28', resolved: false, body: 'Проверяем state до создания сессии? Нужен тест на повторный callback.', replies: [{ who: 'm.ivanova', when: '16:34', body: 'Да, сессия создаётся после verify_state. Добавлю sso_callback_rejects_reused_state.' }] },
        { id: 't2', file: 'src/auth/session.rs', line: 16, who: 'a.petrov', when: 'Вчера, 19:02', resolved: true, body: 'Ограничиваем срок действия TTL сервера? Иначе провайдер задаёт слишком длинную сессию.', replies: [] },
      ],
      drafts: [
        { id: 'd1', file: 'src/auth/sso.rs', line: 19, range: '19', body: 'Ограничим время проверки токена, чтобы callback не зависал.', by: null },
        { id: 'd2', file: 'src/auth/session.rs', line: 16, range: '16', body: 'Удалим state до запроса к провайдеру. Сейчас два параллельных callback могут создать две сессии.', by: 'claude-sso' },
      ],
    },
    '#87': {
      id: '#87', provider: 'github', repo: 'orbit-web', title: 'Кэш сессий', author: 's.kim', branch: 'perf/session-cache', target: 'main', state: 'opened', draft: false,
      head: 'd8a20f1', base: 'ae1102c', commits: ['d8a20f1'], reviewers: [{ who: 'a.petrov', state: 'you' }], assignees: ['s.kim'], labels: ['performance'],
      description: 'Кэширует профиль на время жизни токена.', pipeline: 512, url: 'https://github.com/orbit/orbit-web/pull/87',
      files: ['README.md'], viewed: {}, threads: [], drafts: [],
    },
  };
  W.mrList = ['!142', '#87'];

  W.pipelines = [
    { id: 8842, repo: 'orbit-api', provider: 'gitlab', state: 'running', ref: 'feature/sso-login', sha: 'a3f9c21', title: 'Добавить обмен кода на токен', who: 'm.ivanova', dur: '2 мин 18 с', attempt: 1, when: 'Сейчас' },
    { id: 8841, repo: 'orbit-api', provider: 'gitlab', state: 'failed', ref: 'feature/sso-login', sha: '7d10e4b', title: 'Вынести проверку состояния', who: 'm.ivanova', dur: '4 мин 12 с', attempt: 1, when: '12:36', failed: 'test:integration' },
    { id: 8840, repo: 'orbit-api', provider: 'gitlab', state: 'ok', ref: 'main', sha: '19cd5e0', title: 'Миграция таблицы sso', who: 'a.petrov', dur: '3 мин 48 с', attempt: 1, when: '12:20' },
    { id: 512, repo: 'orbit-web', provider: 'github', state: 'ok', ref: 'perf/session-cache', sha: 'd8a20f1', title: 'Кэш сессий', who: 'a.petrov', dur: '2 мин 41 с', attempt: 1, when: '12:31' },
  ];
  W.jobs = {
    8841: [
      { stage: 'build', jobs: [{ name: 'build', state: 'ok', dur: '1:08' }] },
      { stage: 'test', jobs: [{ name: 'test:unit', state: 'ok', dur: '52 с' }, { name: 'test:integration', state: 'failed', dur: '1:16', code: 101 }] },
    ],
    8842: [
      { stage: 'build', jobs: [{ name: 'build', state: 'ok', dur: '1:02' }] },
      { stage: 'test', jobs: [{ name: 'test:unit', state: 'ok', dur: '50 с' }, { name: 'test:integration', state: 'running', dur: '0:42' }] },
    ],
    8840: [
      { stage: 'test', jobs: [{ name: 'test:unit', state: 'ok', dur: '50 с' }, { name: 'test:integration', state: 'ok', dur: '1:02' }] },
      { stage: 'deploy', jobs: [{ name: 'review:deploy', state: 'skipped', dur: '' }, { name: 'release:manual', state: 'manual', dur: '' }] },
    ],
    512: [{ stage: 'ci', jobs: [{ name: 'lint', state: 'ok', dur: '40 с' }, { name: 'test', state: 'ok', dur: '1:52' }] }],
  };
  W.log = [
    ['', '$ cargo test --locked --test auth_sso -- --nocapture'],
    ['', '   Compiling orbit-api v0.8.0 (/builds/orbit/orbit-api)'],
    ['', '    Finished `test` profile [unoptimized + debuginfo] in 42.11s'],
    ['', '     Running tests/auth_sso.rs (target/debug/deps/auth_sso)'],
    ['', 'test sso_rejects_invalid_state ... ok'],
    ['', 'test sso_exchanges_code ... ok'],
    ['err', "thread 'callback_preserves_state' panicked at tests/auth_sso.rs:184:5:"],
    ['err', "assertion `left == right` failed: callback must redirect"],
    ['err', '  left: 400'],
    ['err', ' right: 303'],
    ['', 'test callback_preserves_state ... FAILED'],
    ['', ''],
    ['err', 'test result: FAILED. 5 passed; 1 failed; finished in 0.24s'],
    ['err', 'error: test failed, to rerun pass `--test auth_sso`'],
    ['err', 'ERROR: Job failed: exit status 101'],
  ];

  W.threads = [
    { id: 'th1', kind: 'task', title: 'Защитить повторный вход', to: 'claude-sso', state: 'running', when: '14:26', ctx: ['src/auth/session.rs:41-43'], messages: [
      { from: 'Вы', text: 'Запрети повторное использование state и добавь тест конкурентного callback.', state: 'accepted', when: '14:26' },
      { from: 'claude-sso', text: 'Взял в работу. Проверка и удаление state теперь в одной транзакции, запускаю тесты.', when: '14:28' },
    ] },
    { id: 'th2', kind: 'ask', title: 'Проверить concurrent_callback', to: 'codex-tests', state: 'unknown', when: '14:31', ctx: ['tests/sso_login.rs', 'test:integration / 8841'], messages: [
      { from: 'Вы', text: 'Проверь, что два callback с одним state не создают две сессии.', state: 'unknown', when: '14:31' },
    ] },
    { id: 'th3', kind: 'ask', title: 'Почему истекает state?', to: 'claude-docs', state: 'replied', when: '14:24', ctx: ['src/auth/sso.rs:16-17'], messages: [
      { from: 'Вы', text: 'Почему state удаляется до создания сессии?', state: 'delivered', when: '14:24' },
      { from: 'claude-docs', text: 'Чтобы повторный callback не мог использовать тот же state. Удаление идёт раньше обмена кода, иначе возможна гонка.', when: '14:25' },
    ] },
    // Агенты пишут друг другу сами: отправитель здесь агент, а не человек.
    { id: 'th4', kind: 'ask', title: 'Какие тесты ловят повторный callback?', from: 'claude-sso', to: 'codex-tests', state: 'replied', when: '14:29', ctx: ['tests/sso_login.rs'], messages: [
      { from: 'claude-sso', text: 'Меняю проверку state в callback. Какие тесты ловят повторный вызов?', state: 'delivered', when: '14:29' },
      { from: 'codex-tests', text: 'concurrent_callback и reject_reused_state в tests/sso_login.rs. Первый сейчас ждёт подтверждения запуска.', when: '14:30' },
    ] },
  ];

  W.update = { state: 'none', version: '0.4.2', current: '0.4.1', size: '38,6 МБ' };

  W.profiles = [
    { id: 'claude', title: 'Claude Code', command: 'claude', flags: '--model opus', default: true, override: { orbit: '--model sonnet' } },
    { id: 'codex', title: 'Codex', command: 'codex', flags: '', default: false, override: {} },
    { id: 'claude-review', title: 'Claude Code, ревью', command: 'claude', flags: '--model opus --permission-mode plan', default: false, override: {} },
    { id: 'gemini', title: 'Gemini CLI', command: 'gemini', flags: '', default: false, override: {} },
  ];

  W.accounts = [
    { id: 'gh', ws: 'work', provider: 'github', host: 'github.com', user: 'm.ivanova', state: 'ok', note: 'Проверен сегодня в 14:32' },
    { id: 'gl', ws: 'work', provider: 'gitlab', host: 'gitlab.com', user: 'm.ivanova', state: 'ok', note: 'До 28 февраля 2027' },
    { id: 'atlas', ws: 'work', provider: 'gitlab', host: 'git.atlas.dev', user: 'm.ivanova', state: 'err', note: 'Токен истёк 8 ноября' },
    { id: 'gh-home', ws: 'personal', provider: 'github', host: 'github.com', user: 'mi-home', state: 'ok', note: 'Проверен вчера' },
  ];

  W.commitPrompt = `Составь сообщение коммита на русском языке по diff из index.

Начни с короткого заголовка в повелительном наклонении. Укажи, что изменилось и зачем.

- Длина заголовка до 72 символов.
- После заголовка оставь пустую строку.
- Не придумывай изменения, которых нет в diff.

Верни только готовое сообщение коммита.`;

  // Файлы принадлежат репозиторию: другая рабочая копия не видит чужие буферы.
  const bufferSets = { 'orbit-api': W.orbitBuffers };
  W.buffersOf = (repo) => bufferSets[repo] || (bufferSets[repo] = { 'README.md': { lang: null, lines: [`# ${repo}`, '', 'Файлы проекта появятся здесь при открытии.'], origin: [] } });
  const currentBuffers = () => W.buffersOf(window.S && S.repo && S.repo() ? S.repo().id : 'none');
  W.buffers = new Proxy({}, {
    get: (_, k) => currentBuffers()[k],
    set: (_, k, v) => { currentBuffers()[k] = v; return true; },
    has: (_, k) => k in currentBuffers(),
    ownKeys: () => Reflect.ownKeys(currentBuffers()),
    getOwnPropertyDescriptor: (_, k) => { const d = Object.getOwnPropertyDescriptor(currentBuffers(), k); return d && { ...d, configurable: true }; },
  });
  W.dirtyBuffers = (repo) => Object.values(bufferSets[repo] || {}).filter((b) => b.modified).length;
  W.dropBuffers = (repo) => { delete bufferSets[repo]; };
  W.allFiles = () => Object.keys(W.buffers);
})();
