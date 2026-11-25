// Сценарии прототипа: любое состояние продукта вызывается отсюда.
(() => {
  const St = S.st;
  const G = { agents: 'Агенты и сессии', git: 'Изменения', app: 'Приложение' };
  const sess = (id) => W.sessions[id];
  const focusS = (id) => S.openSession(id);
  const snapOf = (s) => { s.snapshot = (s.tx || []).slice(-7); };

  S.scenario({ id: 'wait', group: G.agents, title: 'Агент ждёт подтверждения', run: () => { const s = sess('codex-tests'); s.state = 'waiting'; s.approval = s.approval || 'cargo test'; S.notify({ title: 'codex-tests ждёт подтверждения', text: 'Фокус остаётся у вас.', tone: 'wait', action: { label: 'Перейти', run: () => focusS('codex-tests') } }); S.render(); } });
  S.scenario({ id: 'lost', group: G.agents, title: 'Хост потерян, процессы живы', run: () => { const s = sess('claude-sso'); snapOf(s); s.host = 'lost'; s.procs = [['claude', 48216, '14:18:03'], ['cargo test auth::sso', 48702, '14:31:46'], ['orbit_api-6c5f', 48731, '14:31:51']]; focusS('claude-sso'); } });
  S.scenario({ id: 'finished', group: G.agents, title: 'Сессия завершена', run: () => { const s = sess('claude-sso'); snapOf(s); s.host = 'finished'; s.state = 'exited'; s.cpu = 0; s.mem = 0; focusS('claude-sso'); } });
  S.scenario({ id: 'reconnect', group: G.agents, title: 'Подключение после перезапуска', run: () => { const s = sess('claude-sso'); snapOf(s); S.demoReconnect(); focusS('claude-sso'); } });
  S.scenario({ id: 'missing', group: G.agents, title: 'Программа не установлена', run: () => { const s = sess('zsh'); s.host = 'missing'; s.program = 'nvim'; focusS('zsh'); } });
  S.scenario({ id: 'restore', group: G.agents, title: 'Вернуть все сессии в норму', run: () => { Object.values(W.sessions).forEach((s) => { s.host = 'ok'; if (s.state === 'exited') s.state = 'idle'; if (!s.mem) { s.mem = 180; s.cpu = 1; } }); S.render(); } });

  const C = W.changes;
  const addFile = (f) => { if (!C.files.find((x) => x.path === f.path)) C.files.push(f); };
  S.scenario({ id: 'stale', group: G.git, title: 'Сообщение устарело', run: () => { S.go('feature-sso'); C.msg = { text: 'Добавить вход через SSO', state: 'stale', indexAt: 0 }; S.open('changes', {}); } });
  S.scenario({ id: 'hook', group: G.git, title: 'Хук упадёт при коммите', run: () => { S.go('feature-sso'); C.hookFails = true; C.msg = { text: 'Добавить вход через SSO', state: 'manual', indexAt: 0 }; S.open('changes', {}); S.notify({ title: 'Нажмите ⌘↩ в панели изменений', text: 'Хук завершится с ошибкой.' }); } });
  S.scenario({ id: 'divergence', group: G.git, title: 'В коммит попал лишний файл', run: () => { S.go('feature-sso'); C.hookFails = false; C.hookError = false; C.addedAfterCheck = 'migrations/0042_sso.sql'; C.msg = { text: 'Добавить вход через SSO', state: 'manual', indexAt: 0 }; S.open('changes', {}); S.notify({ title: 'Нажмите ⌘↩ в панели изменений', text: 'Агент добавит файл между проверкой и коммитом.' }); } });
  S.scenario({ id: 'rename', group: G.git, title: 'Переименованный файл', run: () => { S.go('feature-sso'); addFile({ path: 'src/auth/oidc.rs', from: 'src/auth/sso_legacy.rs', status: 'R', sim: 92, staged: true }); S.open('changes', {}); S.render(); } });
  S.scenario({ id: 'binary', group: G.git, title: 'Бинарный файл', run: () => { S.go('feature-sso'); addFile({ path: 'assets/logo.png', status: 'B', staged: false, sizes: '184 КБ, было 92 КБ' }); W.diffs['assets/logo.png'] = []; S.open('changes', {}); S.render(); } });
  S.scenario({ id: 'detached', group: G.git, title: 'Detached HEAD', run: () => { S.go('feature-sso'); C.detached = !C.detached; S.open('changes', {}); S.render(); } });
  S.scenario({ id: 'conflict', group: G.git, title: 'Конфликт слияния', run: () => { S.go('feature-sso'); C.conflict = true; const f = C.files.find((x) => x.path === 'src/auth/session.rs'); if (f) { f.conflict = true; f.staged = false; } S.open('changes', {}); S.render(); } });
  S.scenario({ id: 'clean', group: G.git, title: 'Нет изменений', run: () => { S.go('feature-sso'); C.files = []; C.conflict = false; C.detached = false; S.open('changes', {}); S.render(); } });

  S.scenario({ id: 'upd', group: G.app, title: 'Обновление готово', run: () => { W.update.state = 'ready'; S.render(); } });
  S.scenario({ id: 'updb', group: G.app, title: 'Обновление ждёт сессий', run: () => { W.update.state = 'blocked'; S.render(); } });
  S.scenario({ id: 'quit', group: G.app, title: 'Выход из приложения', run: () => S.quitFlow() });
  S.scenario({ id: 'pipefail', group: G.app, title: 'Уведомление о сбое пайплайна', run: () => S.notify({ title: 'Запуск 8841 упал', text: 'test:integration, orbit-api', tone: 'err', action: { label: 'Открыть', run: () => S.run('pipelines') } }) });
  S.scenario({ id: 'narrow', group: G.app, title: 'Панель в тесной зоне', run: () => { const L = S.layout(); L.size.right = 200; S.render(); } });
  S.scenario({ id: 'layout', group: G.app, title: 'Сбросить размеры зон', run: () => S.equalize() });
  // Деление зон: правая делится одна над другой, нижняя рядом (ADR-0031).
  const splitOff = (id) => { const L = S.layout(); const g = S.groupOf(L, id); if (g && g.tabs.length > 1) S.moveTo(id, { split: g.id, side: 'after' }); };
  S.scenario({ id: 'split-right', group: G.app, title: 'Правая зона: история над изменениями', run: () => { if (!S.wb()) S.go('feature-sso'); const p = S.open('changes', { zone: 'right' }); if (p) splitOff(p.id); } });
  S.scenario({ id: 'split-bottom', group: G.app, title: 'Нижняя зона: пайплайны рядом с терминалом', run: () => { if (!S.wb()) S.go('feature-sso'); const p = S.open('pipelines', { zone: 'bottom' }); if (p) splitOff(p.id); } });
  S.scenario({ id: 'light', group: G.app, title: 'Сменить тему', run: () => S.setAppearance(St.prefs.theme === 'dark' ? 'light' : 'dark') });
  S.scenario({ id: 'contrast', group: G.app, title: 'Повышенный контраст', run: () => S.setPref('contrast', !St.prefs.contrast) });
  S.scenario({ id: 'motion', group: G.app, title: 'Без анимации', run: () => S.setPref('motion', !St.prefs.motion) });

  S.firstRun = () => {
    const open = (o) => {
      if (!W.projects['my-app']) {
        W.repos['my-app'] = { id: 'my-app', provider: 'github', host: 'github.com', path: '~/src/my-app', branch: 'main', head: '1f2e3d4' };
        W.workbenches['my-app-main'] = { id: 'my-app-main', title: 'main', project: 'my-app', repo: 'my-app', branch: 'main', worktree: '~/src/my-app' };
        W.projects['my-app'] = W.newProject('my-app', 'personal', { repos: ['my-app'], roots: ['~/src/my-app'], workbenches: ['my-app-main'] });
      }
      S.ov.pop(o);
      S.go('my-app-main');
    };
    const o = {
      type: 'firstrun',
      on: { open: (_, x) => open(x) },
      render: () => `<div class="firstrun"><div class="firstrun-c"><h1>Откройте папку проекта</h1><p>shellio определит репозитории и запустит терминал в этой папке.</p><div class="firstrun-drop">Перетащите папку сюда</div>${K.btn('Открыть папку', { kind: 'primary', do: 'open', key: 'Enter' })}<p class="mono" style="color:var(--text-3)">shellio ~/project</p></div></div>`,
      onKey: (e, tok) => { if (tok === 'Enter') open(o); else if (tok === 'Esc') S.ov.pop(o); return true; },
    };
    S.ov.push(o);
  };
  S.scenario({ id: 'firstrun', group: G.app, title: 'Первый запуск', run: () => S.firstRun() });

  const WS = 'Воркспейсы';
  S.scenario({ id: 'ws-empty', group: WS, title: 'Пустой воркспейс', run: () => { if (!W.workspaces.some((w) => w.id === 'sandbox')) W.workspaces.push({ id: 'sandbox', title: 'Песочница', position: W.workspaces.length, deletedAt: null }); S.goWs('sandbox'); } });
  S.scenario({ id: 'ws-300', group: WS, title: 'Триста проектов в воркспейсе', run: () => {
    const ws = St.route.ws;
    for (let i = 1; i <= 300; i++) {
      const id = `service-${String(i).padStart(3, '0')}`;
      if (W.projects[id]) continue;
      W.repos[id] = { id, provider: 'gitlab', host: 'gitlab.com', path: `~/src/${id}`, branch: 'main', head: 'a1b2c3d' };
      W.projects[id] = W.newProject(id, ws, { repos: [id], roots: [`~/src/${id}`] });
    }
    S.notify({ title: 'Добавлено 300 проектов', text: 'Опроса и наблюдения за ними нет, пока они не открыты.', tone: 'ok' });
    S.render();
  } });
  S.scenario({ id: 'ws-other-wait', group: WS, title: 'Агент ждёт в другом воркспейсе', run: () => { const s = sess('claude-notes') || (W.sessions['claude-notes'] = { id: 'claude-notes', kind: 'agent', agent: 'claude', profile: 'Claude Code', title: 'claude-notes', wb: 'dotfiles-main', state: 'idle', host: 'ok', gen: 1, cpu: 0.2, mem: 148, cwd: '~/dotfiles', background: true, tx: [] }); s.state = 'waiting'; s.approval = s.approval || 'git push'; S.notify({ ws: 'personal', title: 'claude-notes ждёт подтверждения', tone: 'wait', action: { label: 'Перейти', run: () => S.openSession('claude-notes') } }); if (!St.prefs.crossAttention) S.notify({ title: 'Сигнал скрыт', text: 'Он виден только в «Личное». Показать счётчик можно в настройках.' }); S.render(); } });
  S.scenario({ id: 'ws-cli-move', group: WS, title: 'Перенос текущего проекта из CLI', run: () => { const pr = S.project(); if (!pr) return; const to = S.workspaces().find((w) => w.id !== pr.ws); if (to) S.projMoveDo([pr.id], to.id, { cli: true }); } });
  S.scenario({ id: 'ws-move-block', group: WS, title: 'Перенос с препятствиями', run: () => S.projMove(['payments-gateway']) });

  S.demoReset = (mode) => {
    if (mode === 'finished') Object.values(W.sessions).forEach((s) => { s.snapshot = (s.tx || []).slice(-7); s.host = 'finished'; s.state = 'exited'; s.cpu = 0; s.mem = 0; });
    S.render();
  };
})();
