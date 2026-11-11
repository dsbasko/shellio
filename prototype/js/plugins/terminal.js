// Терминал и общая логика сессий. Панель, сессия и программа остаются разными сущностями.
(() => {
  const St = S.st;
  const { esc } = S;
  const c = (cls, t) => `<span class="c-${cls}">${esc(t)}</span>`;

  S.session = (sid) => W.sessions[sid];

  const leftovers = (s) => s.procs || [['claude', 48216, '14:18:03'], ['cargo test auth::sso', 48702, '14:31:46']];

  const stopLeftovers = (p, relaunch) => {
    const s = S.session(p.state.sid);
    S.sheet({
      icon: 'stop', title: 'Остановить оставшиеся процессы?', sub: `Завершится процессов сессии ${s.title}: ${leftovers(s).length}. Их несохранённая работа пропадёт.`, width: 460,
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: relaunch ? 'Остановить и запустить' : 'Остановить', kind: 'danger', key: 'Enter', run: () => {
        s.procs = []; s.host = 'finished'; s.state = 'exited'; s.cpu = 0; s.mem = 0;
        S.notify({ ...S.owner(s.wb), title: 'Оставшиеся процессы остановлены', tone: 'ok' });
        if (relaunch) S.sessionActions.restart.run(p);
        S.render();
      } }],
    });
  };

  // Общие состояния сессии, которые заменяют поверхность терминала.
  S.sessionView = (p, s) => {
    const snap = (s.snapshot || []).slice(-7).filter((l) => !(l && l.raw)).map((l) => `<div class="t-l c-dim">${l && l.html != null ? l.html : l}</div>`).join('');
    if (s.host === 'reconnecting') return K.stack([K.ctx(`${K.status('run')}<span>Подключаемся к сессии ${s.title}</span>`, ''), K.scroll(`<div class="pad"><div style="color:var(--text-2);margin-bottom:8px">Процесс продолжает работу. Ввод станет доступен сразу после подключения.</div>${K.sec('Последний сохранённый вывод')}<div class="term" style="flex:none">${snap}</div></div>`)]);
    if (s.host === 'lost') {
      return K.stack([K.banner('err', `Связь с сессией ${s.title} потеряна`, 'Хост недоступен, но найдены процессы, которые продолжают работать. Подключиться к ним нельзя.', ''), K.scroll(`<div class="pad">${K.sec('Найденные процессы')}${K.table({ cols: [{ t: 'Процесс' }, { t: 'PID', w: '80px', num: true }, { t: 'Запущен', w: '100px', num: true }], rows: leftovers(s).map((r) => ({ k: r[1], cells: r })) })}<div style="display:flex;gap:6px;margin-top:12px">${K.btn('Только остановить', { do: 'stopRest', kind: 'danger', key: 'x' })}${K.btn('Остановить и запустить новую', { do: 'restart', key: 'Enter', kind: 'primary' })}</div><div style="color:var(--text-3);font-size:var(--t-sm);margin-top:8px">Два агента не должны менять один worktree, поэтому оставшиеся процессы завершаются до нового запуска.</div></div>`)]);
    }
    if (s.host === 'finished') {
      const resume = s.kind === 'agent';
      return K.stack([K.ctx(`${K.status('exit')}<span>Сессия ${s.title} завершена</span>`, ''), K.scroll(`<div class="pad"><div style="color:var(--text-2);margin-bottom:12px">Живых процессов не осталось. Вид и контекст сохранены.</div><div style="display:flex;gap:6px;margin-bottom:14px">${resume ? K.btn('Продолжить разговор', { do: 'resume', key: 'Enter', kind: 'primary' }) : ''}${K.btn('Запустить заново', { do: 'restart', key: resume ? 'r' : 'Enter', kind: resume ? '' : 'primary' })}</div>${resume ? `<div class="mono" style="color:var(--text-3);margin-bottom:12px">claude --resume 54b918a6-3e42-4f16-9abc-61d26b70d839</div>` : ''}${K.sec('Последний сохранённый вывод')}<div class="term" style="flex:none">${snap}</div></div>`)]);
    }
    if (s.host === 'starting') return K.stack([K.ctx(`${K.status('run')}<span>Запускаем ${s.title}</span>`, ''), K.pad(`<div style="color:var(--text-2)">Каталог ${s.cwd}. Переход к файлу выполнится, как только ${s.title} будет готов.</div>`)]);
    if (s.host === 'missing') return K.empty({ icon: 'warn', title: `${s.program || 'nvim'} не установлен`, text: 'Установите программу и повторите запуск. Файл и позиция сохранятся.', actions: `${K.btn('brew install neovim', { do: 'noop', icon: 'link' })}${K.btn('Повторить запуск', { do: 'restart', key: 'Enter', kind: 'primary' })}` });
    return null;
  };

  S.sessionActions = {
    stopRest: { title: 'Остановить оставшиеся', key: 'x', when: (p) => S.session(p.state.sid).host === 'lost', run: (p) => stopLeftovers(p, false) },
    // Порядок важен: у завершённого агента Enter принадлежит resume.
    resume: { title: 'Продолжить разговор', key: 'Enter', when: (p) => { const s = S.session(p.state.sid); return s.host === 'finished' && s.kind === 'agent'; }, run: (p) => { const s = S.session(p.state.sid); s.host = 'ok'; s.state = 'idle'; s.gen += 1; s.cpu = 3; s.mem = 200; p.mode = 'input'; if (S.plugins[p.plugin].onRestart) S.plugins[p.plugin].onRestart(p, s, true); S.render(); } },
    restart: { title: 'Запустить заново', key: ['r', 'Enter'], when: (p) => ['lost', 'finished', 'missing'].includes(S.session(p.state.sid).host), run: (p) => { const s = S.session(p.state.sid); if (s.host === 'lost') { stopLeftovers(p, true); return; } s.host = 'ok'; s.state = 'idle'; s.gen += 1; s.cpu = s.kind === 'agent' ? 3 : 0.1; s.mem = s.kind === 'agent' ? 180 : 26; p.mode = 'input'; if (S.plugins[p.plugin].onRestart) S.plugins[p.plugin].onRestart(p, s); S.render(); } },
    stop: { title: 'Остановить сессию', key: 'X', when: (p) => { const s = S.session(p.state.sid); return s.host === 'ok' && s.state !== 'exited'; }, run: (p) => S.stopFlow(p) },
    noop: { title: '', menu: false, run: () => {} },
  };

  S.stopFlow = (p) => {
    const s = S.session(p.state.sid);
    const stop = { label: 'Остановить', kind: 'danger', key: 'Enter', run: () => { s.snapshot = s.snapshot || (s.tx || []).slice(-7); s.host = 'finished'; s.state = 'exited'; s.cpu = 0; s.mem = 0; S.render(); } };
    // Цель фиксируется при вызове: имя и поколение не меняются, пока открыто подтверждение.
    if (p.plugin === 'agent') {
      const gen = s.gen;
      S.sheet({ icon: 'stop', title: `Остановить ${s.title}, поколение ${gen}?`, sub: 'Выполняемая работа будет прервана, несохранённое состояние агента пропадёт. Панель останется, разговор можно будет продолжить. Если нужно только убрать панель, закройте вид.', width: 460, actions: [{ label: 'Закрыть вид', key: 'c', run: () => S.close(p.id) }, { label: 'Отменить', key: 'Esc' }, { ...stop, run: () => { if (W.sessions[s.id] !== s || s.gen !== gen) { S.notify({ title: 'Агент уже другой', text: `${s.title} перезапущен, остановка отменена.`, tone: 'wait' }); return; } stop.run(); } }] });
      return;
    }
    if (p.plugin === 'editor' && S.plugins.editor.stop) { S.plugins.editor.stop(p); return; }
    S.sheet({
      icon: 'stop', title: `Остановить ${s.title}?`, sub: 'Программа и её дочерние процессы завершатся. Если нужно только убрать панель, закройте вид.', width: 460,
      actions: [{ label: 'Закрыть вид', key: 'c', run: () => S.close(p.id) }, { label: 'Отменить', key: 'Esc' }, stop],
    });
  };

  // Закрытие вида обратимо, поэтому без диалога: сессия уходит в «Фоновые».
  S.sessionDestroy = (p, o = {}) => {
    const s = S.session(p.state.sid); if (!s) return;
    if (s.host === 'finished' || s.state === 'exited') { delete W.sessions[s.id]; return; }
    s.background = true;
    if (!o.silent) S.notify({ ...S.owner(s.wb), title: `${s.title} продолжает работу в фоне`, text: 'Вид закрыт, процесс не остановлен.', action: { label: 'Открыть снова', run: () => S.openSession(s.id) } });
  };

  const prompt = (s) => `${c('acc', s.cwd || '~')} ${c('ok', '❯')} `;

  const OUT = {
    'git status --short': [' M src/auth/sso.rs', ' M src/auth/session.rs', ' M src/routes/login.rs', 'A  migrations/0042_sso.sql', ' M README.md'],
    'git status': [' M src/auth/sso.rs', ' M src/auth/session.rs', ' M src/routes/login.rs', 'A  migrations/0042_sso.sql', ' M README.md'],
    'git log --oneline -3': ['a3f9c21 Добавить обмен кода на токен', '7d10e4b Вынести проверку состояния', '19cd5e0 Миграция таблицы sso'],
    ls: ['Cargo.lock  Cargo.toml  README.md  migrations  src  tests'],
    pwd: ['/Users/dev/src/orbit-api'],
    'shellioctl session list': ['ID            ВИД        СОСТОЯНИЕ', 'claude-sso    агент      работает', 'codex-tests   агент      ждёт подтверждения', 'zsh           терминал   простаивает', 'make-test     терминал   работает'],
    'shellioctl ask claude-docs "что делает verify_state?"': ['поставлено в очередь: msg-9127'],
  };
  const TEST = [`   ${c('ok', 'Compiling')} orbit-api v0.8.0`, `    ${c('ok', 'Finished')} test profile in 2.14s`, 'running 3 tests', `test sso::valid_state ... ${c('ok', 'ok')}`, `test sso::reject_expired_state ... ${c('ok', 'ok')}`, `test sso::reject_reused_state ... ${c('ok', 'ok')}`, '', `test result: ${c('ok', 'ok')}. 3 passed; 0 failed; 24 filtered out`];

  function exec(p, s) {
    const t = p.state; const cmd = t.input.trim();
    s.tx.push(`${prompt(s)}${esc(t.input)}`);
    t.input = '';
    if (!cmd) { S.render(); return; }
    t.hist.push(cmd); t.hi = t.hist.length;
    if (cmd === 'clear') { s.tx = []; S.render(); return; }
    if (cmd === 'help') { s.tx.push(c('dim', 'Попробуйте: git status --short, git log --oneline -3, ls, pwd, cargo test, make test, shellioctl session list, clear')); S.render(); return; }
    if (cmd === 'cargo test' || cmd === 'make test') {
      s.state = 'running'; s.cpu = 14; s.mem = 340;
      let i = 0; const lines = TEST;
      const timer = setInterval(() => {
        s.tx.push(lines[i++]);
        if (i >= lines.length) { clearInterval(timer); s.state = 'idle'; s.cpu = 0.1; s.mem = 26; }
        S.render();
      }, 320);
      p.state.timer = timer;
      S.render();
      return;
    }
    const out = OUT[cmd];
    if (out) out.forEach((l) => s.tx.push(esc(l)));
    else s.tx.push(`zsh: command not found: ${esc(cmd.split(' ')[0])}`);
    S.render();
  }

  S.plugin({
    id: 'terminal', title: 'Терминал', icon: 'term', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'multi', minSize: { w: 300, h: 90 }, inputDefault: true,
    init(p, o) {
      let s = o.sid && W.sessions[o.sid];
      if (!s) { const id = S.uid('zsh'); s = W.sessions[id] = { id, kind: 'term', title: `zsh ${Object.values(W.sessions).filter((x) => x.kind === 'term').length + 1}`, wb: p.wb, state: 'idle', host: 'ok', gen: 1, cpu: 0.1, mem: 26, cwd: W.workbenches[p.wb].worktree, tx: [] }; }
      if (!s.tx) s.tx = [];
      s.background = false;
      Object.assign(p.state, { sid: s.id, input: '', hist: [], hi: 0 });
    },
    surfaceless: (p) => S.session(p.state.sid).host !== 'ok',
    tabTitle: (p) => S.session(p.state.sid).title,
    tabStatus: (p) => { const s = S.session(p.state.sid); return s.host === 'ok' ? (s.state === 'running' ? 'run' : null) : S.agentSt(s); },
    meter: (p) => { const s = S.session(p.state.sid); return s.host === 'finished' ? null : { kind: 'measured', text: `${S.fmt.num(s.cpu)}% CPU · ${S.fmt.mem(s.mem)}` }; },
    context: (p) => [{ label: `Терминал ${S.session(p.state.sid).title}`, icon: 'term' }],
    view(p, geo) {
      const s = S.session(p.state.sid);
      const st = S.sessionView(p, s);
      if (st) return st;
      return K.term(s.tx, { input: s.state === 'running' ? null : p.state.input, prompt: prompt(s) });
    },
    actions: { ...S.sessionActions },
    onKey(p, e, tok) {
      const s = S.session(p.state.sid);
      if (s.host !== 'ok') return;
      const t = p.state;
      if (tok === 'Ctrl-\\') { s.tx.push(c('dim', '^\\')); S.render(); return; }
      if (tok === 'Ctrl-c') { if (t.timer) { clearInterval(t.timer); t.timer = null; s.state = 'idle'; s.tx.push(c('dim', '^C')); } else { s.tx.push(`${prompt(s)}${esc(t.input)}^C`); t.input = ''; } S.render(); return; }
      if (tok === 'Ctrl-l') { s.tx = []; S.render(); return; }
      if (tok === 'Enter') { exec(p, s); return; }
      if (tok === 'Backspace') { t.input = t.input.slice(0, -1); S.render(); return; }
      if (tok === 'Up' && t.hist.length) { t.hi = Math.max(0, t.hi - 1); t.input = t.hist[t.hi] || ''; S.render(); return; }
      if (tok === 'Down' && t.hist.length) { t.hi = Math.min(t.hist.length, t.hi + 1); t.input = t.hist[t.hi] || ''; S.render(); return; }
      if (e.key.length === 1 && !e.metaKey && !e.ctrlKey) { t.input += e.key; S.render(); }
    },
    destroy: (p, o) => S.sessionDestroy(p, o),
    onRestart: (p, s) => { s.tx = [c('dim', `Сессия ${s.title} запущена заново`)]; },
  });

  const changed = (mark, path) => `${mark === 'A' ? `${c('ok', 'A')} ` : ` ${c('err', mark)}`} ${path}`;
  W.sessions.zsh.tx = [`${c('acc', '~/src/orbit-api')} ${c('ok', '❯')} git status --short`, changed('M', 'src/auth/sso.rs'), changed('M', 'src/auth/session.rs'), changed('M', 'src/routes/login.rs'), changed('A', 'migrations/0042_sso.sql'), changed('M', 'README.md')];
  W.sessions['make-test'].tx = ['cargo test --workspace --all-targets', `   ${c('ok', 'Compiling')} orbit-api v0.8.0`, 'running 12 tests', `test rejects_expired_state ... ${c('ok', 'ok')}`, `test creates_session_cookie ... ${c('ok', 'ok')}`, 'test concurrent_callback ... выполняется'];
})();
