// Панель агента: родной TUI в терминале и тонкая строка состояния. У каждого агента своя панель (ADR-0030).
(() => {
  const St = S.st;
  const { esc, cx } = S;
  const c = (cls, t) => `<span class="c-${cls}">${esc(t)}</span>`;

  const bullet = (t) => `  ${c('ok', '●')} ${t}`;
  const scripts = {
    'claude-sso': [
      bullet(`${c('b', 'Чтение')} src/routes/login.rs`),
      { cls: 'dim', html: '    Обработчик сохраняет state в сессию до редиректа.' },
      '',
      bullet(`${c('b', 'Изменение')} src/auth/sso.rs ${c('ok', '+6')} ${c('err', '−1')}`),
      { cls: 'add', html: '  + let expected_state = session.take_sso_state().await?;' },
      { cls: 'add', html: '  + verify_state(&amp;expected_state, &amp;query.state)?;' },
      '',
      bullet(`${c('b', 'Выполняю')} cargo test auth::sso`),
      { cls: 'dim', html: `      test result: ${c('ok', 'ok')}. 12 passed; 0 failed` },
      '',
      '  Готово. Проверка state работает, обмен кода покрыт тестами. Осталась обработка таймаута провайдера.',
    ],
  };

  const claude = W.sessions['claude-sso'];
  claude.tx = [
    bullet(`${c('b', 'Изменение')} src/auth/sso.rs ${c('ok', '+12')} ${c('err', '−1')}`),
    { cls: 'dim', html: '        user_id: claims.sub,' },
    { cls: 'del', html: '  - expires_at,' },
    { cls: 'add', html: '  + expires_at: expires_at.min(Utc::now() + SESSION_TTL),' },
    { cls: 'dim', html: '        created_at: Utc::now(),' },
    '',
    bullet(`${c('b', 'Изменение')} migrations/0042_sso.sql ${c('ok', '+18')}`),
    '    Таблица sso_sessions, индексы по пользователю и сроку действия.',
    '',
    bullet(`${c('b', 'Выполняю')} cargo check`),
    { cls: 'dim', html: '      Checking orbit-api v0.8.0' },
    { cls: 'dim', html: `      ${c('ok', 'Finished')} dev profile in 1.82s` },
    '',
    '  Сессия создаётся только после проверки. Перехожу к о',
    '',
  ];
  // Последняя строка ещё печатается: первый такт дописывает её целиком.
  claude.partial = { index: 13, text: '  Сессия создаётся только после проверки. Перехожу к обработчику login и тестам.' };
  claude.script = scripts['claude-sso'].slice();
  claude.activity = 'Чтение';
  claude.elapsed = 1;

  const codex = W.sessions['codex-tests'];
  codex.tx = [
    { cls: 'dim', html: '  codex · ~/src/orbit-api · feature/sso-login' },
    '',
    `  ${c('dim', '›')} Допиши интеграционные тесты для входа через SSO`,
    '',
    `  ${c('ok', '•')} Обновил tests/sso_login.rs`,
    '',
  ];
  codex.approval = 'cargo test --test sso_login';

  W.sessions['claude-docs'].tx = [
    `${c('b', 'Claude Code')} ${c('dim', '~/src/orbit-api')}`,
    '',
    `${c('dim', '>')} Обнови README с примером настройки SSO.`,
    '',
    bullet(`${c('b', 'Изменение')} README.md ${c('ok', '+6')}`),
    'Готово. Раздел про SSO добавлен.',
  ];

  // Поля, которые объявил адаптер. Нет поля — нет строки в сведениях.
  Object.assign(claude, { fields: { tool: 'Изменение src/auth/sso.rs', tokens: { last: 18400, total: 214000 }, context: 41 } });
  Object.assign(codex, { fields: { tool: 'cargo test --test sso_login', tokens: { last: 9100, total: 61800 } } });
  Object.assign(W.sessions['claude-docs'], { fields: { tool: 'Изменение README.md', tokens: { last: 2300, total: 40100 }, context: 12 } });

  // Время в состоянии считается от известного перехода.
  const now = () => Date.now();
  [['claude-sso', 134], ['codex-tests', 41], ['claude-docs', 760]].forEach(([id, sec]) => { const s = W.sessions[id]; s.since = now() - sec * 1000; s.seen = s.state; });

  const REPLY = ['Смотрю, что уже сделано.', 'Понял. Изменений в коде это не требует, добавлю проверку в тест.', 'Готово.'];
  const replyLines = () => REPLY.map((r, i) => (i === REPLY.length - 1 ? `  ${r}` : bullet(r)));

  // Единый такт сессий: выводит следующую строку сценария, двигает индикатор и отмечает переходы.
  S.tickAgents = () => {
    let changed = false;
    Object.values(W.sessions).forEach((s) => {
      if (s.kind !== 'agent') return;
      if (s.seen !== s.state) { s.seen = s.state; s.since = now(); changed = true; }
      if (s.host !== 'ok' || s.state !== 'running') return;
      s.elapsed = (s.elapsed || 0) + 1;
      changed = true;
      if (s.elapsed % 3 === 0 && s.partial) { s.tx[s.partial.index] = s.partial.text; delete s.partial; return; }
      if (s.elapsed % 3 === 0 && s.script && s.script.length) {
        s.tx.push(s.script.shift());
        if (!s.script.length) {
          s.state = 'idle'; s.cpu = 0.4; s.activity = null;
          S.notify({ ws: S.wsOf(s.wb), project: W.workbenches[s.wb].project, title: `${s.title} закончил`, text: 'Агент ждёт следующей задачи.', tone: 'ok', action: { label: 'Открыть', run: () => S.openSession(s.id) } });
        }
      }
    });
    return changed;
  };

  const boxOf = (s) => {
    const running = s.state === 'running';
    const ph = s.approval ? '1–3 или Enter' : s.agent !== 'claude' || s.input ? '' : running ? 'Подождите окончания шага или нажмите Esc' : 'Опишите следующий шаг';
    return `${c('dim', '> ')}${esc(s.input || '')}<span class="cur"></span>${ph ? `<span class="ph">${ph}</span>` : ''}`;
  };

  function surface(s) {
    const lines = s.tx.slice();
    if (s.state === 'waiting' && s.approval) {
      lines.push(`  ${c('wait', 'Разрешить выполнение команды?')}`);
      lines.push({ cls: 'b', html: `    $ ${esc(s.approval)}` });
      lines.push('');
      lines.push({ raw: '<button type="button" class="term-opt first" data-do="approve" data-arg="1" data-k="o1">  › 1. Да</button>' });
      lines.push({ raw: `<button type="button" class="term-opt" data-do="approve" data-arg="2" data-k="o2">    2. Да, и больше не спрашивать для ${esc(s.approval.split(' ').slice(0, 2).join(' '))}</button>` });
      lines.push({ raw: `<button type="button" class="term-opt" data-do="approve" data-arg="3" data-k="o3">    3. Нет, сказать, что сделать иначе   ${c('dim', 'esc')}</button>` });
    } else if (s.state === 'running') {
      const elapsed = s.agent === 'claude' ? `${s.elapsed || 0} с, Esc прервать` : 'Esc прервать';
      lines.push(`  ${c('wait', `${s.agent === 'claude' ? '✻' : '⠋'}${s.agent === 'claude' ? ` ${s.activity || 'Думаю'}…` : ''}`)}${s.agent === 'claude' ? '' : ` ${s.activity || 'Думаю'}…`} ${c('dim', elapsed)}`);
    }
    return K.term(lines, { anchored: true, box: boxOf(s) });
  }

  function decide(s, allow) {
    if (allow) {
      s.tx.push({ cls: 'b', html: `  $ ${esc(s.approval)}` }, { cls: 'dim', html: '     Compiling orbit-api v0.8.0' });
      s.approval = null; s.state = 'running'; s.elapsed = 0; s.cpu = 18; s.activity = 'Выполняю тесты';
      s.script = [{ cls: 'dim', html: `      test concurrent_callback ... ${c('ok', 'ok')}` }, { cls: 'dim', html: `      test result: ${c('ok', 'ok')}. 1 passed` }, '', '  Тест проходит. Один callback принят, второй отклонён.'];
    } else {
      s.tx.push({ cls: 'dim', html: '  ⎿ Команда отклонена.' }, '  Что сделать иначе?');
      s.approval = null; s.waitText = 'ждёт ввода'; s.cpu = 0.4;
    }
    S.render();
  }

  function submit(s) {
    const text = (s.input || '').trim(); s.input = '';
    if (!text) { S.render(); return; }
    s.tx.push('', `  ${c('dim', '›')} ${esc(text)}`, '');
    s.state = 'running'; s.elapsed = 0; s.cpu = 14; s.activity = 'Думаю'; s.waitText = null;
    s.script = replyLines();
    S.render();
  }

  const sel = (p) => W.sessions[p.state.sid] || null;
  const live = (s) => s.host !== 'finished' && s.state !== 'exited';
  const stOf = (s) => (s.stale ? 'exit' : S.agentSt(s));
  const age = (s) => {
    if (!s.since || s.stale || !live(s)) return '';
    const sec = Math.max(0, Math.round((now() - s.since) / 1000));
    return sec < 60 ? `${sec} с` : sec < 3600 ? `${Math.floor(sec / 60)} мин` : `${Math.floor(sec / 3600)} ч`;
  };
  const stateLine = (s) => {
    if (s.stale) return 'статус устарел';
    if (s.host === 'ok' && s.state === 'running') return s.activity ? `работает, ${s.activity.toLowerCase()}` : 'работает';
    return S.stateName(s);
  };
  const stateHtml = (s) => { const t = esc(stateLine(s)); return !s.stale && s.host === 'ok' && s.state === 'waiting' ? `<span class="c-wait">${t}</span>` : s.host === 'lost' || s.state === 'error' ? `<span class="c-err">${t}</span>` : t; };
  const tok = (n) => (n >= 1000 ? `${S.fmt.num(Math.round(n / 100) / 10)} тыс.` : String(n));
  const short = (p) => !!p.geo && p.geo.size === 'ShortBar';

  function details(p, s) {
    const f = s.fields || {};
    const wb = W.workbenches[s.wb];
    const queue = (W.threads || []).filter((t) => S.threadTo && S.threadTo(t) === s.id && ['queued', 'delivered'].includes(t.state)).length;
    const rows = [
      ['Профиль', esc(s.profile)],
      ['Каталог', esc(s.cwd)],
      ['Ветка', esc(wb.branch)],
      f.tool && ['Последнее', esc(f.tool)],
      f.tokens && ['Токены', `ход ${tok(f.tokens.last)}, всего ${tok(f.tokens.total)}`],
      f.context != null && ['Контекст', `${f.context}%`],
      ['Очередь', queue ? S.fmt.plural(queue, 'сообщение', 'сообщения', 'сообщений') : 'пусто'],
      live(s) && ['Процесс', `${S.fmt.num(s.cpu)}% CPU, ${S.fmt.mem(s.mem)}`],
    ];
    return `<div class="${cx('ag-det', short(p) && 'full')}" data-k="ag-det">${K.sec('Сведения', K.iconBtn('close', { do: 'details', tip: 'Скрыть сведения (s)' }))}<div class="pad">${K.kv(rows)}</div></div>`;
  }

  // Имя агента во вкладке, поэтому строка над терминалом называет только состояние и время в нём.
  const head = (p, s) => K.ctx(`<span class="ag-st">${stateHtml(s)}</span>${age(s) ? `<span class="ag-age">${age(s)}</span>` : ''}`, K.iconBtn('info', { do: 'details', tip: 'Сведения об агенте (s)', kind: p.state.details ? 'on' : '' }));

  function view(p) {
    const s = sel(p);
    if (!s) return K.empty({ icon: 'agent', title: 'Сессия агента недоступна', text: 'Агент убран вместе с сессией.', actions: K.btn('Новый агент', { cmd: 'agent.new', key: 'Space a', kind: 'primary' }) });
    const st = S.sessionView(p, s);
    if (st) return st;
    // В низкой панели сведения стоят на месте терминала, а не сжимают его.
    if (p.state.details && short(p)) return `${head(p, s)}${details(p, s)}`;
    return `${head(p, s)}${p.state.details ? details(p, s) : ''}<div class="ag-pane">${surface(s)}</div>`;
  }

  // Имя сессии не переиспользуется: завершённая сессия не отдаёт своё имя следующей.
  const usedIds = new Set(Object.keys(W.sessions));
  const freshId = (command) => { let n = 1; while (usedIds.has(`${command}-${n}`)) n++; const id = `${command}-${n}`; usedIds.add(id); return id; };

  // Поручение новому агенту: своя рабочая область и свой каталог, фокус остаётся у источника.
  S.launchTaskAgent = (task, ctx) => {
    const src = S.wb();
    const profile = W.profiles.find((f) => f.default) || W.profiles[0];
    const id = freshId(profile.command);
    const wb = `task-${id}`;
    W.workbenches[wb] = { id: wb, title: id, project: src.project, repo: src.repo, branch: `agent/${id}`, worktree: `${src.worktree}-${id}` };
    S.project().workbenches.push(wb);
    S.seedLayout(wb, 'task');
    S.launchAgent(profile, task, 'center', { id, wb, ctx, focus: false });
    return W.sessions[id];
  };

  S.launchAgent = (profile, task, zone, opts = {}) => {
    const id = opts.id || freshId(profile.command);
    const wb = opts.wb ? W.workbenches[opts.wb] : S.wb();
    const flags = profile.override[S.project().id] || profile.flags;
    const cwd = wb.worktree;
    const ctxLines = opts.ctx && opts.ctx.length ? [`${c('dim', 'Контекст:')} ${esc(opts.ctx.join(', '))}`, ''] : [];
    W.sessions[id] = { id, kind: 'agent', agent: profile.command, profile: profile.title, title: id, wb: wb.id, state: task ? 'running' : 'idle', host: 'ok', gen: 1, cpu: task ? 12 : 1, mem: 180, cwd, elapsed: 0, since: now(), activity: task ? 'Изучаю проект' : null, fields: {}, tx: [`${c('b', profile.title)} ${c('dim', cwd)} ${c('dim', flags)}`, ''].concat(ctxLines, task ? [`${c('dim', '>')} ${esc(task)}`, ''] : []), script: task ? replyLines() : [] };
    W.sessions[id].seen = W.sessions[id].state;
    const panel = S.open('agent', { sid: id, key: id, zone: zone || 'center', wb: opts.wb, focus: opts.focus, at: opts.at });
    if (panel && opts.wb) { const L = S.layout(opts.wb); if (!L.focus) L.focus = panel.id; }
    return panel;
  };

  // Переписка открывается в другой зоне, чтобы терминал агента оставался виден.
  function openThreads(p) {
    const L = S.layout(p.wb);
    const here = S.groupOf(L, p.id);
    const zone = here && here.zone === 'center' ? 'right' : 'center';
    const t = S.open('threads', { agent: p.state.sid, zone });
    if (t && S.groupOf(L, t.id) === S.groupOf(L, p.id)) S.move(t.id, zone);
    const g = S.groupOf(L, p.id);
    if (g && g.active !== p.id) { g.active = p.id; S.render(); }
  }

  S.plugin({
    id: 'agent', title: 'Агент', icon: 'agent', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'multi', minSize: { w: 320, h: 120 }, inputDefault: true,
    init(p, o) { Object.assign(p.state, { sid: o.sid, details: false }); if (W.sessions[o.sid]) W.sessions[o.sid].background = false; },
    reopen(p) { const s = sel(p); if (s) s.background = false; },
    // Сведения в низкой панели стоят на месте терминала: ввод в невидимый терминал не уходит.
    surfaceless: (p) => { const s = sel(p); return !s || s.host !== 'ok' || (p.state.details && short(p)); },
    tabTitle: (p) => { const s = sel(p); return s ? esc(s.title) : 'Агент'; },
    tabStatus: (p) => { const s = sel(p); return s ? stOf(s) : null; },
    meter: (p) => { const s = sel(p); return !s || !live(s) ? null : { kind: 'measured', text: `${S.fmt.num(s.cpu)}% CPU · ${S.fmt.mem(s.mem)}`, tip: `Измерено: сессия ${s.title}, хост и все её процессы` }; },
    context: (p) => { const s = sel(p); return s ? [{ label: `Вывод ${s.title}`, icon: 'agent' }] : []; },
    inputTarget: (p) => { const s = sel(p); return s ? s.title : 'агента'; },
    view: (p, geo) => K.adapt(geo, { left: () => view(p), right: () => view(p), center: () => view(p), bottom: () => view(p) }),
    inputHints: (p) => { const s = sel(p); return s && s.state === 'waiting' && s.approval ? [['1', 'да'], ['3', 'нет']] : []; },
    actions: {
      details: { title: 'Сведения об агенте', key: 's', hint: 'Сведения', when: (p) => !!sel(p), run: (p) => { p.state.details = !p.state.details; S.render(); } },
      threads: { title: 'Переписка агента', key: 'm', hint: 'Переписка', when: (p) => !!sel(p), run: openThreads },
      ask: { title: 'Спросить этого агента', key: 'q', hint: 'Спросить', when: (p) => !!sel(p) && sel(p).host === 'ok', run: (p) => S.composer('ask', { to: p.state.sid }) },
      approve: { title: '', menu: false, run: (p, a) => { const s = sel(p); if (s && s.state === 'waiting' && s.approval) decide(s, a !== '3'); } },
      ...Object.fromEntries(Object.entries(S.sessionActions).map(([k, a]) => [k, { ...a, when: (p) => !!sel(p) && (!a.when || a.when(p)) }])),
    },
    onKey(p, e, tok) {
      const s = sel(p);
      if (!s || s.host !== 'ok') return;
      if (tok === 'Ctrl-\\') { s.tx.push(c('dim', '^\\')); S.render(); return; }
      if (s.state === 'waiting' && s.approval) {
        if (tok === '1' || tok === '2' || tok === 'Enter' || tok === 'y') decide(s, true);
        else if (tok === '3' || tok === 'Esc' || tok === 'n') decide(s, false);
        return;
      }
      if (s.state === 'running') {
        if (tok === 'Esc') { s.tx.push({ cls: 'dim', html: '  ⎿ Прервано. Что сделать дальше?' }); s.state = 'waiting'; s.approval = null; s.waitText = 'ждёт ввода'; s.script = []; delete s.partial; s.cpu = 0.4; s.activity = null; S.render(); }
        return;
      }
      if (tok === 'Enter') { submit(s); return; }
      if (tok === 'Backspace') { s.input = (s.input || '').slice(0, -1); S.render(); return; }
      if (e.key.length === 1 && !e.metaKey && !e.ctrlKey) { s.input = (s.input || '') + e.key; S.render(); }
    },
    // Закрытие вида обратимо: агент продолжает работу и виден в «В фоне».
    destroy: (p, o) => S.sessionDestroy(p, o),
    onRestart: (p, s, resumed) => { s.tx = [`${c('b', s.profile)} ${c('dim', s.cwd)}`, c('dim', resumed ? 'Разговор продолжен.' : 'Новая сессия.'), '']; s.stale = false; },
  });

  const G = 'Агенты и сессии';
  const ensureWb = () => { if (!S.wb()) S.go('feature-sso'); };
  const panelFor = (sid) => S.openSession(sid);
  S.scenario({ id: 'agents-many', group: G, title: 'Пять агентов: пять вкладок', run: () => {
    ensureWb();
    const mine = () => Object.values(W.sessions).filter((s) => s.kind === 'agent' && s.wb === S.wb().id && live(s));
    const gem = W.profiles.find((f) => f.id === 'gemini');
    if (gem && !mine().some((s) => s.agent === 'gemini')) S.launchAgent(gem, 'Найди места, где не обработан таймаут провайдера', 'center', { focus: false });
    while (mine().length < 5) S.launchAgent(W.profiles[0], '', 'center', { focus: false });
    mine().forEach((s) => { if (!S.panelOf(s)) S.open('agent', { sid: s.id, key: s.id, zone: 'center', focus: false }); });
    panelFor('claude-sso');
  } });
  S.scenario({ id: 'agents-split', group: G, title: 'Два агента рядом', run: () => {
    ensureWb();
    const a = panelFor('claude-sso'); const b = panelFor('codex-tests');
    const L = S.layout();
    if (S.groupOf(L, a.id) === S.groupOf(L, b.id)) S.moveTo(b.id, { split: S.groupOf(L, b.id).id, side: 'after' });
    S.focus(a.id);
  } });
  S.scenario({ id: 'agents-stale', group: G, title: 'Статус агента не подтверждён', run: () => {
    const s = W.sessions['claude-sso']; s.stale = true;
    S.openSession('claude-sso');
    S.notify({ title: 'Нет свежего статуса claude-sso', text: 'Последнее состояние помечено устаревшим, пока адаптер не пришлёт новое.', tone: 'wait' });
  } });
  S.scenario({ id: 'agents-hooked', group: G, title: 'Агент запущен вручную в терминале', run: () => {
    ensureWb();
    const id = 'zsh-claude';
    if (!W.sessions[id]) W.sessions[id] = { id, kind: 'term', title: 'zsh 2', wb: S.wb().id, state: 'running', host: 'ok', gen: 1, cpu: 9, mem: 260, cwd: S.wb().worktree, hooked: { title: 'claude', profile: 'Claude Code', state: 'running', host: 'ok' }, tx: [`${c('acc', S.wb().worktree)} ${c('ok', '❯')} claude`, '', bullet(`${c('b', 'Чтение')} src/auth/session.rs`)] };
    S.open('terminal', { sid: id, zone: 'bottom' });
    S.notify({ title: 'claude из терминала zsh 2 зарегистрирован хуком', text: 'Он остаётся во вкладке терминала, в «Сессиях» виден как агент.' });
  } });
})();
