// Глобальные команды. Меню, клавиша, палитра и кнопка вызывают один обработчик.
(() => {
  const St = S.st;
  const { icon } = S;
  const need = (id, fn) => (...args) => { if (!S.plugins[id]) { S.notify({ title: 'Панель пока недоступна', text: id, tone: 'wait' }); return; } fn(...args); };
  const anchor = (sel) => document.querySelector(sel);

  S.stateName = (s) => {
    if (s.host === 'lost') return 'хост потерян';
    if (s.host === 'finished') return 'завершена';
    if (s.host === 'reconnecting') return 'подключение';
    if (s.host === 'starting') return 'запуск';
    return { running: 'работает', waiting: s.waitText || 'ждёт подтверждения', idle: 'простаивает', error: 'ошибка', exited: 'завершена', starting: 'запуск' }[s.state] || s.state;
  };
  S.agentSt = (s) => {
    if (s.host === 'lost') return 'err';
    if (s.host === 'finished') return 'exit';
    if (s.host === 'reconnecting' || s.host === 'starting') return 'run';
    return { running: 'run', waiting: 'wait', idle: 'idle', error: 'err', exited: 'exit', starting: 'run' }[s.state] || 'idle';
  };

  S.openSession = (sid) => {
    const s = W.sessions[sid];
    const gone = S.removedOwner(s.wb);
    if (gone) { S.projRestorePrompt(gone.id, () => S.openSession(sid), s.title); return null; }
    const existing = Object.values(St.panels).find((p) => p.state.sid === sid);
    if (existing) { S.focus(existing.id); return existing; }
    s.background = false;
    return S.open(s.kind === 'editor' ? 'editor' : s.kind === 'agent' ? 'agent' : 'terminal', { sid, key: sid, wb: s.wb, zone: 'center' });
  };
  // Панель, которая показывает сессию: у каждого агента своя панель.
  S.panelOf = (s) => Object.values(St.panels).find((p) => p.state.sid === s.id) || null;

  S.openFile = (file, line) => { if (!S.plugins.editor) return; S.open('editor', { file, line, zone: 'center' }); };

  const C = (id, title, keys, run, extra = {}) => S.cmd({ id, title, keys, run, ...extra });
  // На WorkspaceHome нет рабочей области, и команды, которым она нужна, не существуют.
  const inWb = () => !!S.wb();

  C('palette', 'Поиск и команды', ['Space Space', 'Cmd-k'], () => S.palette(), { grp: 'Общие', icon: 'search' });
  C('help', 'Все клавиши', ['?'], () => S.help(), { grp: 'Общие', icon: 'keys' });
  C('hints', 'Метки перехода', ['f'], () => S.hintsOpen(), { grp: 'Общие', hidden: true });
  C('cmdline', 'Командная строка', [':'], () => S.cmdline(), { grp: 'Общие', hidden: true });
  C('filter', 'Фильтр в панели', ['/'], () => { const p = S.focused(); if (p) { p.filtering = true; S.render(); } }, { grp: 'Общие', hidden: true });
  C('back', 'Назад к предыдущему месту', ['Ctrl-o'], () => S.back(), { grp: 'Общие', hidden: true });

  const canType = (p) => { const def = p && S.plugins[p.plugin]; return !!def && !!def.inputDefault && !(def.surfaceless && def.surfaceless(p)); };
  C('mode.toggle', 'Переключить режим', [], () => {
    const p = S.focused();
    if (!canType(p)) { S.notify({ title: 'В этой панели нет режима ввода', text: 'Он есть у терминала, агентов и nvim.' }); return; }
    S.setMode(S.mode() === 'input' ? 'nav' : 'input');
  }, { grp: 'Общие' });
  C('mode.input', 'Режим ввода', ['i'], () => S.run('mode.toggle'), { hidden: true, pal: false, when: () => canType(S.focused()) && S.mode() === 'nav' });
  C('demo.welcome', 'Как пользоваться прототипом', [], () => S.welcome(), {});

  C('terminal.new', 'Новый терминал', ['Space t'], need('terminal', () => S.open('terminal', { zone: 'center' })), { grp: 'Запуск', icon: 'term', when: inWb });
  C('agent.new', 'Новый агент', ['Space a'], need('agent', () => S.newAgent()), { grp: 'Запуск', icon: 'agent', when: inWb });
  C('agents.find', 'Найти агента', [], () => S.palette('sess'), { grp: 'Агенты', icon: 'agent', when: inWb });
  C('editor.open', 'nvim', ['Space e'], need('editor', () => S.open('editor', { zone: 'center' })), { grp: 'Запуск', icon: 'file', when: inWb, sub: 'один на рабочую область' });
  C('ask', 'Спросить агента', ['Space q'], () => S.composer('ask'), { grp: 'Агенты', icon: 'help', when: inWb });
  C('task', 'Попросить агента', ['Space d'], () => S.composer('task'), { grp: 'Агенты', icon: 'agent', when: inWb });
  C('threads', 'Переписка', ['Space h'], need('threads', () => S.open('threads', {})), { grp: 'Агенты', icon: 'hub', when: inWb });

  C('changes', 'Изменения и коммит', ['Space g c'], need('changes', () => S.open('changes', {})), { grp: 'Git', icon: 'changes', when: inWb });
  // «История» (ADR-0033): граф рабочей области, история файла и история строк.
  // Файл, строка и ревизия под курсором: у nvim ревизия открытого буфера, у diff — ревизия его стороны.
  const here = () => {
    const p = S.focused(); const def = p && S.plugins[p.plugin];
    const at = (def && def.location ? def.location(p) : null) || {};
    return p && p.plugin === 'editor' ? { ...at, rev: p.state.rev || null } : at;
  };
  C('history.open', 'История рабочей области', ['Space g l'], need('history', () => S.open('history', { reset: true })), { grp: 'Git', icon: 'history', when: inWb });
  C('history.file', 'История файла', ['Space g h'], need('history', () => {
    const at = here();
    if (!at.file) { S.notify({ title: 'Нет файла для истории', text: 'Откройте файл в nvim или выберите файл в diff.' }); return; }
    S.open('history', { path: at.file, rev: at.rev || undefined });
  }), { grp: 'Git', icon: 'history', when: inWb });
  C('history.line', 'История строки или выделения', ['Space g L'], need('history', () => {
    const p = S.focused(); const at = here();
    if (!at.file || !at.line) { S.notify({ title: 'Нет строки для истории', text: 'Поставьте курсор на строку в nvim или в diff.' }); return; }
    const t = p && p.plugin === 'editor' ? p.state : null;
    const lines = t && t.anchor != null ? [Math.min(t.anchor, t.line) + 1, Math.max(t.anchor, t.line) + 1] : [at.line, at.line];
    S.open('history', { path: at.file, lines, rev: at.rev || undefined });
  }), { grp: 'Git', icon: 'history', when: inWb });

  C('review.open', 'Открыть ревью', ['Space r o'], need('review', () => S.open('review', { key: 'list' })), { grp: 'Ревью', icon: 'review', when: inWb });
  C('review.submit', 'Отправить ревью', ['Space r s'], need('review', () => {
    const p = S.focused();
    if (p && p.plugin === 'review') { S.plugins.review.actions.submit.run(p); return; }
    const wb = S.wb();
    const opened = Object.values(St.panels).filter((x) => x.plugin === 'review' && x.wb === wb.id && W.mrs[x.state.target]).map((x) => x.state.target);
    const onBranch = W.mrList.filter((id) => W.mrs[id] && W.mrs[id].repo === wb.repo && W.mrs[id].branch === wb.branch && W.mrs[id].state === 'opened');
    const ids = [...new Set([...opened, ...onBranch])];
    if (ids.length === 1) { S.plugins.review.actions.submit.run(S.open('review', { key: ids[0], target: ids[0] })); return; }
    const list = S.open('review', { key: 'list' });
    if (!ids.length) list.filter = wb.repo;
    S.notify({ title: ids.length ? 'Выберите ревью' : 'Для этой области ревью нет', text: ids.length ? 'К этой рабочей области относится несколько целей.' : `Ветка ${wb.branch} в ${wb.repo} не связана с MR или PR.`, tone: 'wait' });
  }), { grp: 'Ревью', icon: 'send', when: inWb });

  C('pipelines', 'Пайплайны', ['Space p'], need('pipelines', () => S.open('pipelines', {})), { grp: 'Проект', icon: 'pipeline', when: inWb });
  C('pipelines.failed', '', [], need('pipelines', () => {
    const f = W.pipelines.find((x) => x.state === 'failed' && S.project().repos.includes(x.repo));
    if (f) S.open('pipelines', { run: f.id, repo: f.repo, job: f.failed });
  }), { pal: false, hidden: true });
  C('pipelines.run', '', [], need('pipelines', (id) => { const r = W.pipelines.find((x) => x.id === +id); if (r && S.repoWs(r.repo) === St.route.ws) S.open('pipelines', { run: r.id, repo: r.repo }); }), { pal: false, hidden: true });
  C('resources', 'Ресурсы', ['Space m'], need('resources', () => S.open('resources', {})), { grp: 'Проект', icon: 'cpu' });
  C('settings', 'Настройки', ['Space ,', 'Cmd-,'], need('settings', () => S.open('settings', {})), { grp: 'Проект', icon: 'gear' });
  C('update', 'Обновление', ['Space u'], () => S.updateFlow(), { grp: 'Проект', icon: 'update' });

  C('wb.new', 'Новая рабочая область', ['Space w n'], () => S.newWorkbench(), { grp: 'Рабочая область', icon: 'grid', when: inWb });
  C('wb.switch', 'Переключить проект или рабочую область', ['Space w w'], () => S.switcher(), { grp: 'Рабочая область', icon: 'grid' });

  ['h', 'j', 'k', 'l'].forEach((d) => {
    C(`focus.${d}`, `Фокус ${{ h: 'влево', j: 'вниз', k: 'вверх', l: 'вправо' }[d]}`, [`Ctrl-w ${d}`, d], () => S.focusDir(d), { hidden: false, grp: 'Панели', pal: false });
    const zone = { H: 'left', J: 'bottom', K: 'center', L: 'right' }[d.toUpperCase()];
    C(`move.${d}`, `В зону: ${{ left: 'слева', bottom: 'снизу', center: 'центр', right: 'справа' }[zone]}`, [`Ctrl-w ${d.toUpperCase()}`], () => { const p = S.focused(); if (p) S.move(p.id, zone); }, { grp: 'Панели', pal: false });
  });
  // Клавиша работает везде и подсказывает ось зоны, палитра показывает только деление этой зоны.
  C('split.v', 'Разделить зону: части рядом', ['Ctrl-w v'], () => S.split('row'), { grp: 'Панели', icon: 'splitCols', when: inWb, palWhen: () => S.splitAxis() === 'row' });
  C('split.s', 'Разделить зону: части одна над другой', ['Ctrl-w s'], () => S.split('stack'), { grp: 'Панели', icon: 'splitRows', when: inWb, palWhen: () => S.splitAxis() === 'stack' });
  C('part.next', 'В соседнюю часть', [], () => { const p = S.focused(); const n = p && S.neighborPart(p); if (n) S.moveTo(p.id, { group: n.id }); }, { grp: 'Панели', when: () => { const p = S.focused(); return !!p && !!S.neighborPart(p); } });
  C('panel.close', 'Закрыть панель', ['Ctrl-w q'], () => { const p = S.focused(); if (p) S.close(p.id); }, { grp: 'Панели', icon: 'close' });
  C('panel.tocenter', '', [], () => { const p = S.focused(); if (p) S.move(p.id, 'center'); }, { pal: false, hidden: true });
  C('panel.only', 'Только эта панель', ['Ctrl-w o'], () => S.only(), { grp: 'Панели' });
  C('layout.equal', 'Выровнять размеры', ['Ctrl-w ='], () => S.equalize(), { grp: 'Панели' });
  C('size.w+', 'Шире', ['Ctrl-w >'], () => S.nudge('h', 40), { grp: 'Панели', pal: false, hidden: true });
  C('size.w-', 'Уже', ['Ctrl-w <'], () => S.nudge('h', -40), { grp: 'Панели', pal: false, hidden: true });
  C('size.h+', 'Выше', ['Ctrl-w +'], () => S.nudge('v', 40), { grp: 'Панели', pal: false, hidden: true });
  C('size.h-', 'Ниже', ['Ctrl-w -'], () => S.nudge('v', -40), { grp: 'Панели', pal: false, hidden: true });
  C('zoom', 'Развернуть панель', ['Ctrl-w z'], () => S.zoom(), { grp: 'Панели', icon: 'expand' });
  C('tab.prev', 'Предыдущая вкладка', ['H'], () => S.tabStep(-1), { grp: 'Панели', hidden: true, pal: false });
  C('tab.next', 'Следующая вкладка', ['L'], () => S.tabStep(1), { grp: 'Панели', hidden: true, pal: false });
  for (let i = 1; i <= 9; i++) C(`tab.${i}`, `Вкладка ${i}`, [String(i)], () => S.tabAt(i - 1), { hidden: true, pal: false });

  C('zone.left', 'Показать или скрыть навигацию', ['Space z h', 'Cmd-0'], () => S.toggleZone('left'), { grp: 'Зоны', icon: 'sideL' });
  C('zone.bottom', 'Показать или скрыть нижнюю зону', ['Space z j', 'Cmd-j'], () => S.toggleZone('bottom'), { grp: 'Зоны', icon: 'sideB' });
  C('zone.right', 'Показать или скрыть правую зону', ['Space z l', 'Cmd-Alt-0'], () => S.toggleZone(S.sideZone()), { grp: 'Зоны', icon: 'sideR' });
  C('zone.toggle', '', [], (z) => S.toggleZone(z), { pal: false, hidden: true });

  C('theme.toggle', 'Сменить тему', ['Cmd-Alt-t'], () => S.setTheme(St.prefs.theme === 'dark' ? 'light' : 'dark'), { icon: 'sun' });
  C('demo.drawer', 'Сценарии прототипа', [], () => S.palette('demo'), { icon: 'scenes', sub: 'потеря хоста, отказ хука, обновление и другие состояния' });
  C('app.quit', 'Выйти из shellio', [], () => S.quitFlow(), { icon: 'close' });
  S.native = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.native ? (a) => window.webkit.messageHandlers.native.postMessage(a) : null;
  C('win.close', '', [], () => S.quitFlow(), { pal: false, hidden: true });
  C('win.min', '', [], () => { if (S.native) S.native('min'); else S.notify({ title: 'Сворачивает браузер', text: 'На странице прототипа окном управляет браузер.' }); }, { pal: false, hidden: true });
  C('win.zoom', '', [], () => { if (S.native) S.native('zoom'); else if (document.fullscreenElement) document.exitFullscreen(); else if (document.documentElement.requestFullscreen) document.documentElement.requestFullscreen(); }, { pal: false, hidden: true });

  C('status.none', '', [], () => S.notify({ title: 'Откройте проект', text: 'Агенты живут в рабочей области.' }), { pal: false, hidden: true });
  C('agents.waiting', '', [], () => { const a = S.agents().find((x) => x.state === 'waiting'); if (a) S.openSession(a.id); }, { pal: false, hidden: true });

  C('zone.add', '', [], (zone) => {
    const list = Object.values(S.plugins).filter((d) => !d.chromeless && !d.hiddenFromAdd && (S.wb() || d.cardinality === 'app' || d.cardinality === 'workspace'));
    S.popover(document.querySelector('.panel.is-focus .ib.sm') || document.querySelector('.titlebar .seg'), {
      title: 'Открыть в этой зоне', width: 240,
      body: () => list.map((d) => K.row({ k: d.id, icon: d.icon, title: d.title, do: 'open', arg: d.id, quiet: true })).join(''),
      on: { open: (id, o) => { S.ov.pop(o); if (id === 'agent') S.newAgent(zone); else S.open(id, { zone }); } },
    });
  }, { pal: false, hidden: true });

  // Соседняя часть той же зоны: следующая, а у последней предыдущая.
  S.neighborPart = (p) => {
    const L = S.layout(p.wb); const g = S.groupOf(L, p.id);
    if (!g || g.nav) return null;
    const list = L.parts[g.zone]; const i = list.indexOf(g.id);
    if (list.length < 2) return null;
    return L.groups[list[i + 1] || list[i - 1]];
  };
  const PART_NAME = { row: ['левее', 'правее'], stack: ['выше', 'ниже'] };
  S.partName = (from, to) => { const L = S.layout(); const list = L.parts[from.zone]; return PART_NAME[S.AXIS[from.zone]][list.indexOf(to.id) > list.indexOf(from.id) ? 1 : 0]; };

  // Выбор для новой части, когда переносить нечего. Часть создаётся только после выбора.
  S.partPicker = (g) => {
    const list = Object.values(S.plugins).filter((d) => !d.chromeless && !d.hiddenFromAdd && (!d.placements || d.placements.includes(g.zone)) && (S.wb() || d.cardinality === 'app' || d.cardinality === 'workspace'));
    list.sort((a, b) => (a.id === 'terminal' ? -1 : b.id === 'terminal' ? 1 : 0));
    const at = { split: g.id, side: 'after' };
    S.popover(document.querySelector(`.group[data-g="${g.id}"] [data-cmd^="split."]`) || document.querySelector(`.group[data-g="${g.id}"] .ib.sm`) || document.querySelector('.titlebar .seg'), {
      title: 'Открыть в новой части', width: 260,
      body: () => list.map((d) => K.row({ k: d.id, icon: d.icon, title: d.title, do: 'open', arg: d.id, quiet: true })).join(''),
      on: { open: (id, o) => {
        S.ov.pop(o);
        const room = S.splitRoom(g, [g.active ? St.panels[g.active].plugin : 'terminal', id]);
        if (!room.ok) { S.notify({ title: 'Не хватает места для двух частей', text: `${S.plugins[id].title}: части нужно ${room.want} px, выйдет ${room.half}.`, tone: 'wait' }); return; }
        if (id === 'agent') S.newAgent(g.zone, at); else S.open(id, { zone: g.zone, at });
      } },
    });
  };

  const tabsOverflow = (p) => { const el = document.querySelector(`.group[data-g="${S.groupOf(S.layout(), p.id).id}"] .tabs`); return !!el && el.scrollWidth > el.clientWidth; };

  C('panel.menu', '', [], () => {
    const p = S.focused(); if (!p) return;
    const def = S.plugins[p.plugin];
    const acts = Object.entries(def.actions || {}).filter(([, a]) => a.title && a.menu !== false && (!a.when || a.when(p)));
    const L = S.layout(); const zone = S.groupOf(L, p.id).zone;
    const zones = [['left', 'Слева', 'Ctrl-w H'], ['center', 'В центр', 'Ctrl-w K'], ['bottom', 'Вниз', 'Ctrl-w J'], ['right', 'Справа', 'Ctrl-w L']].filter(([z]) => z !== zone);
    const here = S.groupOf(L, p.id);
    const others = here.nav ? [] : S.partsOf(L, zone).filter((g) => g !== here);
    const axis = S.AXIS[zone];
    const parts = `${others.map((g) => K.row({ k: `pt-${g.id}`, title: `В часть ${S.partName(here, g)}`, do: 'part', arg: g.id, quiet: true })).join('')}${here.nav ? '' : K.row({ k: 'split', title: axis === 'row' ? 'Разделить: части рядом' : 'Разделить: одна над другой', do: 'split', quiet: true, trail: K.kbd(axis === 'row' ? 'Ctrl-w v' : 'Ctrl-w s') })}`;
    S.popover(event.target.closest('button'), {
      title: def.title, width: 280,
      head: (() => { const m = def.meter ? def.meter(p) : null; return m ? `<span class="meter">${m.text}</span>` : ''; })(),
      body: () => `${tabsOverflow(p) ? `${K.sec('Вкладки')}${S.groupOf(L, p.id).tabs.map((id) => { const x = St.panels[id]; const d = S.plugins[x.plugin]; return K.row({ k: `t-${id}`, icon: d.icon, title: d.tabTitle ? d.tabTitle(x) : d.title, do: 'tab', arg: id, quiet: true }); }).join('')}${K.sec('Действия')}` : ''}${acts.map(([n, a]) => K.row({ k: n, title: a.title, do: 'act', arg: n, quiet: true, trail: a.key ? K.kbd([].concat(a.key)[0]) : '' })).join('')}${K.sec('Перенести')}${zones.map(([z, t, k]) => K.row({ k: z, title: t, do: 'mv', arg: z, quiet: true, trail: K.kbd(k) })).join('')}${parts ? `${K.sec('Части зоны')}${parts}` : ''}${K.sec('Панель')}${K.row({ k: 'add', title: 'Добавить панель в эту зону', do: 'add', quiet: true })}${K.row({ k: 'z', title: 'Развернуть', do: 'zoom', quiet: true, trail: K.kbd('Ctrl-w z') })}${K.row({ k: 'q', title: 'Закрыть', do: 'cl', quiet: true, trail: K.kbd('Ctrl-w q') })}`,
      on: {
        tab: (id, o) => { S.ov.pop(o); S.focus(id); },
        act: (n, o) => { S.ov.pop(o); def.actions[n].run(p); S.render(); },
        mv: (z, o) => { S.ov.pop(o); S.move(p.id, z); },
        part: (g, o) => { S.ov.pop(o); S.moveTo(p.id, { group: g }); },
        split: (_, o) => { S.ov.pop(o); S.split(axis); },
        add: (_, o) => { S.ov.pop(o); S.run('zone.add', zone); },
        zoom: (_, o) => { S.ov.pop(o); S.zoom(); },
        cl: (_, o) => { S.ov.pop(o); S.close(p.id); },
      },
    });
  }, { pal: false, hidden: true });

  C('status.branch', '', [], () => {
    const wb = S.wb(); const repo = S.repo();
    S.popover(event.target.closest('button'), {
      title: 'Ветка и worktree', width: 300,
      body: () => K.pad(K.kv([['Ветка', W.changes.detached ? 'detached HEAD' : wb.branch], ['HEAD', W.changes.head || repo.head], ['Worktree', wb.worktree], ['Репозиторий', `${repo.id}, ${repo.provider === 'gitlab' ? 'GitLab' : 'GitHub'}`]]) + `<div style="margin-top:10px">${K.btn('Изменения', { cmd: 'changes', key: 'Space g c', small: true })}</div>`),
    });
  }, { pal: false, hidden: true });

  C('status.resources', '', [], () => {
    const t = S.totals();
    const alive = Object.values(W.sessions).filter((s) => s.host !== 'finished');
    const rows = alive.filter((s) => S.wsOf(s.wb) === St.route.ws).sort((a, b) => b.mem - a.mem).slice(0, 5);
    const away = alive.filter((s) => S.wsOf(s.wb) !== St.route.ws);
    const awayCpu = Math.round(away.reduce((n, s) => n + s.cpu, 0) * 10) / 10;
    const awayMem = away.reduce((n, s) => n + s.mem, 0);
    S.popover(event.target.closest('button'), {
      title: 'Потребление', width: 320,
      head: `<span class="meter">100% — одно ядро</span>`,
      body: () => `${K.pad(K.kv([['Измерено, CPU', `${S.fmt.num(t.cpu)}%`], ['Измерено, память', S.fmt.mem(t.mem)]]))}${K.sec('Самое тяжёлое')}${rows.map((s) => K.row({ k: s.id, st: S.agentSt(s), title: s.title, trail: `${S.fmt.num(s.cpu)}% &nbsp; ${S.fmt.mem(s.mem)}`, quiet: true })).join('')}${away.length ? K.row({ k: 'other', icon: 'folder', title: 'Другие воркспейсы', trail: `${S.fmt.num(awayCpu)}% &nbsp; ${S.fmt.mem(awayMem)}`, quiet: true }) : ''}${K.row({ k: 'shared', icon: 'grid', title: 'Общее приложения', trail: `${S.fmt.num(W.resources.shared.cpu)}% &nbsp; ${W.resources.shared.mem} МБ`, quiet: true })}<div class="pad">${K.btn('Открыть ресурсы', { cmd: 'resources', key: 'Space m', small: true })}</div>`,
    });
  }, { pal: false, hidden: true });

  C('review.drafts', '', [], () => {
    S.popover(event.target.closest('button'), {
      title: 'Неотправленные черновики', width: 320,
      body: () => Object.values(W.mrs).filter((m) => m.drafts.length && S.repoWs(m.repo) === St.route.ws).map((m) => K.row({ k: m.id, icon: 'review', title: `${m.id} ${m.title}`, sub: S.fmt.plural(m.drafts.length, 'черновик', 'черновика', 'черновиков'), do: 'open', arg: m.id, quiet: true })).join(''),
      on: { open: (id, o) => { S.ov.pop(o); S.open('review', { key: id, target: id }); } },
    });
  }, { pal: false, hidden: true });

  S.newWorkbench = () => {
    S.sheet({
      icon: 'grid', title: 'Новая рабочая область', sub: `В проекте ${S.project().title}. Панели откроются сразу, настройки агента берутся из профиля проекта.`, width: 480,
      state: { name: '', tpl: 'empty', worktree: true },
      body: (o) => `<label class="form-l">Название</label>${K.field({ bind: 'name', value: o.state.name, placeholder: 'sso-refresh' }).replace('<input', '<input data-autofocus')}
        <label class="form-l">Начать с</label>${[['empty', 'Терминал', 'Один терминал в каталоге worktree. Остальные панели откроются по мере работы'], ['agent', 'Агент', 'Агент в центре и панель изменений справа'], ['review', 'Ревью', 'Список MR и PR проекта']].map(([id, t, s]) => `<div class="${S.cx('opt', o.state.tpl === id && 'on')}" data-do="tpl" data-arg="${id}">${K.radio(o.state.tpl === id)}<div><b>${t}</b><span class="s">${s}</span></div></div>`).join('')}
        <div class="${S.cx('opt')}" data-do="wt" style="margin-top:8px">${K.toggle(o.state.worktree)}<div><b>Новый worktree</b><span class="s">${o.state.worktree ? `Ветка <span class="mono">feature/${o.state.name || 'имя'}</span> от main, папка ~/src/${S.wb().repo}-${o.state.name || 'имя'}` : 'Работа в текущем каталоге ~/src/orbit-api'}</span></div></div>`,
      on: { tpl: (a, o) => { o.state.tpl = a; }, wt: (_, o) => { o.state.worktree = !o.state.worktree; } },
      onBind: () => S.render(),
      actions: [
        { label: 'Отменить', key: 'Esc' },
        { label: 'Создать', kind: 'primary', key: 'Enter', disabled: (o) => !(o.state.name || '').trim(), run: (o) => {
          const name = o.state.name.trim(); const id = `wb-${S.uid('x')}`;
          W.workbenches[id] = { id, title: name, project: S.project().id, repo: S.wb().repo, branch: o.state.worktree ? `feature/${name}` : S.wb().branch, worktree: o.state.worktree ? `~/src/${S.wb().repo}-${name}` : S.wb().worktree };
          S.project().workbenches.push(id);
          St.route.wb = id;
          S.seedLayout(id, o.state.tpl);
          S.render();
        } },
      ],
    });
  };

  S.newAgent = (zone = 'center', at) => {
    const pr = S.project().id;
    const st = { i: 0, task: '' };
    S.sheet({
      icon: 'agent', title: 'Новый агент', sub: `В каталоге ${S.wb().worktree}. Флаги берутся из настроек проекта ${pr}.`, width: 480, state: st,
      body: (o) => `${W.profiles.map((f, i) => `<div class="${S.cx('opt', o.state.i === i && 'on')}" data-do="pick" data-arg="${i}">${K.radio(o.state.i === i)}<div style="flex:1;min-width:0"><b>${f.title}${f.default ? ' <span style="color:var(--text-3);font-weight:400">по умолчанию</span>' : ''}</b><span class="s mono">${f.command} ${f.override[pr] || f.flags}</span></div>${K.kbd(String(i + 1))}</div>`).join('')}<label class="form-l">Задача при запуске, необязательно</label>${K.field({ bind: 'task', value: o.state.task, placeholder: 'Что нужно сделать?', rows: 2 })}`,
      on: { pick: (a, o) => { o.state.i = +a; } },
      onKey: (e, tok, o) => {
        if (tok === 'j' || tok === 'Down') { o.state.i = Math.min(W.profiles.length - 1, o.state.i + 1); S.render(); return true; }
        if (tok === 'k' || tok === 'Up') { o.state.i = Math.max(0, o.state.i - 1); S.render(); return true; }
        if (/^[1-9]$/.test(tok) && W.profiles[+tok - 1]) { o.state.i = +tok - 1; S.render(); return true; }
        return false;
      },
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Запустить', kind: 'primary', key: 'Enter', run: (o) => S.launchAgent(W.profiles[o.state.i], o.state.task, zone, { at }) }],
    });
  };

  // Сессии других воркспейсов показываются только числом.
  const liveHere = () => {
    const live = Object.values(W.sessions).filter((s) => s.host === 'ok');
    const here = live.filter((s) => S.wsOf(s.wb) === St.route.ws);
    const away = live.length - here.length;
    return { live: here, awayRow: away ? K.row({ k: '@away', title: 'Другие воркспейсы', trail: S.fmt.plural(away, 'сессия', 'сессии', 'сессий'), quiet: true }) : '' };
  };
  S.quitFlow = () => {
    if (St.prefs.quitSilent) { S.demoReconnect(); return; }
    const { live, awayRow } = liveHere();
    S.sheet({
      icon: 'close', title: 'Выйти из shellio?', sub: 'Агенты и терминалы продолжат работу. При следующем запуске приложение подключится к тем же сессиям.', width: 480, state: { remember: false },
      body: (o) => `${live.map((s) => K.row({ k: s.id, st: S.agentSt(s), title: s.title, trail: S.stateName(s), quiet: true })).join('')}${awayRow}<div class="opt" data-do="rem" style="margin-top:8px">${K.check(o.state.remember)}<div><b>Больше не спрашивать</b><span class="s">Выход всегда будет оставлять сессии работать</span></div></div>`,
      on: { rem: (_, o) => { o.state.remember = !o.state.remember; } },
      actions: [
        { label: 'Отменить', key: 'Esc' },
        { label: 'Завершить всё', kind: 'danger', key: 's', run: () => S.demoReset('finished') },
        { label: 'Выйти, оставив сессии', kind: 'primary', key: 'Enter', run: (o) => { if (o.state.remember) St.prefs.quitSilent = true; S.demoReconnect(); } },
      ],
    });
  };

  S.demoReconnect = () => {
    Object.values(W.sessions).forEach((s) => { if (s.host === 'ok') s.host = 'reconnecting'; });
    S.render();
    setTimeout(() => { Object.values(W.sessions).forEach((s) => { if (s.host === 'reconnecting') { s.host = 'ok'; if (s.kind === 'editor' && S.nvimDrain) S.nvimDrain(s); } }); S.notify({ title: 'Сессии подключены', tone: 'ok' }); S.render(); }, 2200);
  };

  S.updateFlow = () => {
    const u = W.update;
    if (u.state === 'none') { S.notify({ title: 'Установлена последняя версия', text: `shellio ${u.current}` }); return; }
    const from = document.querySelector('.status [data-cmd="update"]');
    if (u.state === 'blocked') {
      const { live, awayRow } = liveHere();
      S.popover(from, {
        title: `Обновление ${u.version} ждёт сессий`, width: 340,
        body: () => `<div class="pad" style="color:var(--text-2)">Эти сессии несовместимы с новой версией. Обновление установится после их завершения.</div>${live.map((s) => K.row({ k: s.id, st: S.agentSt(s), title: s.title, trail: S.stateName(s), do: 'open', arg: s.id, quiet: true })).join('')}${awayRow}`,
        on: { open: (id, o) => { S.ov.pop(o); S.openSession(id); } },
      });
      return;
    }
    const install = (o) => { S.ov.pop(o); u.state = 'none'; u.current = u.version; S.demoReconnect(); };
    S.popover(from, {
      title: `Обновление ${u.version} готово`, width: 340,
      body: () => `${K.pad(`${K.kv([['Сейчас', u.current], ['Размер', u.size], ['Подпись', '<span class="c-ok">проверена</span>']])}<div style="color:var(--text-2);margin:10px 0">Сессии продолжат работу, окно перезапустится.</div>${K.btn('Установить и перезапустить', { kind: 'primary', do: 'install', key: 'Enter', small: true })}`)}`,
      on: { install: (_, o) => install(o) },
      onKey: (e, tok, o) => { if (tok === 'Enter') { install(o); return true; } return false; },
    });
  };
})();
