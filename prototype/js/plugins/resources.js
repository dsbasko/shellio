(() => {
  const { esc } = S;
  const zoneNames = { left: 'Слева', right: 'Справа', center: 'Центр', bottom: 'Снизу' };
  const sortNames = { mem: 'Память', cpu: 'CPU', title: 'Название' };
  const order = ['mem', 'cpu', 'title'];
  W.resources.processes = W.resources.processes || {};
  W.resources.nextPid = W.resources.nextPid || 48200;
  W.resources.sample = W.resources.sample || { missing: {}, when: 'Текущий снимок' };
  W.resources.shared.processes = W.resources.shared.processes || [{ name: 'shellio', pid: 48190, started: 'Сегодня, 14:10:00', depth: 0 }];
  W.resources.service.processes = W.resources.service.processes || [
    { name: 'shellio-service', pid: 48191, started: 'Сегодня, 14:10:00', depth: 0 },
    { name: 'git', pid: 48192, started: 'Сегодня, 14:10:01', depth: 1 },
  ];
  function ensureProcesses(s) {
    const found = W.resources.processes[s.id];
    if (found && found.gen === s.gen && found.rows.length) return found.rows;
    const pid = W.resources.nextPid; W.resources.nextPid += 4;
    const started = `Запущен в ${new Date().toLocaleTimeString('ru-RU')}`;
    const rows = [
      { name: 'shellio-host', pid, started, depth: 0 },
      { name: s.kind === 'agent' ? s.agent || s.profile : s.title, pid: pid + 1, started, depth: 1 },
      { name: 'shellio-attach', pid: pid + 2, started, depth: 0 },
    ];
    W.resources.processes[s.id] = { gen: s.gen, rows };
    return rows;
  }
  Object.values(W.sessions).forEach(ensureProcesses);
  const sample = () => W.resources.sample;
  const alive = (s) => s.host !== 'finished' && s.state !== 'exited';
  const cpu = (n) => `${S.fmt.num(Math.round(n * 10) / 10)}%`;
  const mem = (n) => S.fmt.mem(n);
  function panelsOf(s) {
    return [S.panelOf(s)].filter(Boolean).map((p) => ({ panel: p, group: S.groupOf(S.layout(p.wb), p.id) })).filter((x) => x.group);
  }
  function placeOf(s) {
    const list = panelsOf(s);
    return list.length ? list.map(({ group }) => zoneNames[group.zone]).join(', ') : 'В фоне';
  }
  function rows() {
    const here = S.st.route.ws;
    const live = Object.values(W.sessions).filter(alive);
    const away = live.filter((s) => S.wsOf(s.wb) !== here);
    const whatOf = (s) => { const w = W.workbenches[s.wb]; return W.projects[w.project].removedAt ? `${W.projects[w.project].title}, убран из каталога` : w.title; };
    return [
      ...live.filter((s) => S.wsOf(s.wb) === here).map((s) => ({ id: s.id, title: s.title, cpu: s.cpu, mem: s.mem, session: s, place: placeOf(s), what: whatOf(s), missing: sample().missing[s.id] })),
      ...(away.length ? [{ id: '@other', title: 'Другие воркспейсы', cpu: Math.round(away.reduce((n, s) => n + s.cpu, 0) * 10) / 10, mem: away.reduce((n, s) => n + s.mem, 0), what: S.fmt.plural(away.length, 'сессия', 'сессии', 'сессий'), place: 'Приложение' }] : []),
      { id: '@shared', title: 'Общее приложения', ...W.resources.shared, what: 'GUI, рендер, шрифты, Metal', place: 'Приложение' },
      { id: '@service', title: 'Служебные процессы', ...W.resources.service, what: 'Git и служебная работа', place: 'Приложение' },
    ];
  }
  const filtered = (p) => rows().filter((r) => S.match(`${r.title} ${r.what} ${r.place}`, p.filter || '') > 0);
  const sorted = (list, by) => [...list].sort((a, b) => {
    if (by === 'title') return a.title.localeCompare(b.title, 'ru');
    if (!!a.missing !== !!b.missing) return a.missing ? 1 : -1;
    return b[by] - a[by] || a.title.localeCompare(b.title, 'ru');
  });
  function visibleRows(p) {
    const list = filtered(p);
    if (p.state.presentation === 'side') {
      const seen = new Map();
      [...sorted(list, p.state.sort).slice(0, 4), ...sorted(list, p.state.sort === 'cpu' ? 'mem' : 'cpu').slice(0, 4), ...list.filter((r) => r.missing)].forEach((r) => seen.set(r.id, r));
      return [...seen.values()];
    }
    return p.state.presentation === 'bottom' ? [...new Map([...sorted(list, p.state.sort).slice(0, 5), ...list.filter((r) => r.missing)].map((r) => [r.id, r])).values()] : sorted(list, p.state.sort);
  }
  const selected = (p) => {
    const list = p.state.area === 'detail' ? filtered(p) : visibleRows(p);
    return list.find((r) => r.id === p.state.selected) || list[0];
  };
  function measured() {
    const total = S.totals();
    const missing = rows().filter((r) => r.missing);
    return { cpu: total.cpu - missing.reduce((sum, r) => sum + r.cpu, 0), mem: total.mem - missing.reduce((sum, r) => sum + r.mem, 0), missing };
  }
  function shownEstimates() {
    const here = S.st.route.ws;
    const known = W.estimates.filter((e) => W.projects[e.project]);
    const mine = known.filter((e) => S.projectWs(e.project) === here);
    const away = known.filter((e) => S.projectWs(e.project) !== here);
    return [...mine, ...(away.length ? [{ id: '@other', title: 'Другие воркспейсы', what: 'Оценка одной суммой', cpu: Math.round(away.reduce((n, e) => n + e.cpu, 0) * 10) / 10, mem: Math.round(away.reduce((n, e) => n + e.mem, 0) * 10) / 10 }] : [])];
  }
  function estimated() { return shownEstimates().reduce((sum, r) => ({ cpu: sum.cpu + r.cpu, mem: sum.mem + r.mem }), { cpu: 0, mem: 0 }); }
  const totalText = (t) => `${t.missing.length ? '≥ ' : ''}${cpu(t.cpu)} CPU &nbsp; ${t.missing.length ? '≥ ' : ''}${mem(t.mem)}`;
  const estimateText = () => { const t = estimated(); return `≈ ${cpu(t.cpu)} CPU &nbsp; ≈ ${mem(t.mem)}`; };
  function partialBanner() {
    const missing = measured().missing;
    return missing.length ? K.banner('wait', 'Частичный замер', `Нет замера: ${missing.map((r) => esc(r.title)).join(', ')}. Эти сессии не входят в доступную часть итога. Повторите замер.`, K.btn('Повторить', { do: 'refresh', small: true })) : '';
  }
  function header(p, back = false) {
    return K.ctx(`${back ? K.iconBtn('chevL', { do: 'back', tip: 'К сводке' }) : ''}${K.crumbs(back ? ['Приложение', 'Процессы'] : ['Приложение'])}`, K.btn(sortNames[p.state.sort], { do: 'sort', small: true, key: 's', tip: 'Изменить сортировку' }));
  }
  function empty(p) { return K.empty({ icon: 'cpu', title: 'Строки не найдены', text: 'Измените фильтр или покажите все процессы.', actions: K.btn('Сбросить фильтр', { do: 'clearFilter' }) }); }
  function measuredSummary(compact = false) {
    const t = measured();
    const title = t.missing.length ? 'Измерено: доступная часть' : 'Измерено: процессы';
    return `${K.sec(title, compact ? '' : totalText(t))}${compact ? K.row({ title: totalText(t), sub: '100% CPU = одно логическое ядро', quiet: true }) : K.pad(`<div class="row-s">100% CPU — одно логическое ядро. Доступно ядер: ${W.resources.cores}. Итог включает GUI, сессии и служебные процессы.</div>`)}`;
  }
  function estimates(compact = false) {
    const heading = `${K.sec('Оценка: данные приложения', compact ? '' : estimateText())}${compact ? K.row({ title: estimateText(), quiet: true }) : ''}`;
    const explanation = K.pad('<div class="row-s">Оценка не складывается с измеренным и не вычитается из него. Это учтённые данные и время работы внутри приложения.</div>');
    return `${heading}${compact ? '' : K.table({ cols: [{ t: 'Данные' }, { t: 'CPU', w: '85px', num: true }, { t: 'Память', w: '100px', num: true }], rows: shownEstimates().map((r) => ({ k: `estimate-${r.id}`, cells: [`${esc(r.title)}<div class="row-s">${esc(r.what)}</div>`, `≈ ${cpu(r.cpu)}`, `≈ ${mem(r.mem)}`] })) })}${explanation}`;
  }
  function table(p, list = visibleRows(p), compact = false) {
    if (!list.length) return empty(p);
    const current = selected(p);
    return K.table({
      cols: [{ t: 'Панель / сессия' }, ...(!compact ? [{ t: 'Рабочая область / зона', w: '175px' }] : []), { t: 'CPU', w: '75px', num: true }, { t: 'Память', w: '90px', num: true }],
      rows: list.map((r) => ({
        k: r.id, on: current && current.id === r.id, do: 'select', arg: r.id,
        cells: [`${r.missing || r.session ? K.status(r.missing ? 'wait' : S.agentSt(r.session)) : ''} ${esc(r.title)}${r.missing ? '<div class="row-s">Нет замера</div>' : ''}`, ...(!compact ? [`${esc(r.what)}<div class="row-s">${esc(r.place)}</div>`] : []), r.missing ? '—' : cpu(r.cpu), r.missing ? '—' : mem(r.mem)],
      })),
    });
  }
  function top(p, metric, omitCursor = false) {
    const current = selected(p); const list = sorted(filtered(p), metric).slice(0, 4);
    return `${K.sec(metric === 'mem' ? 'Топ по памяти' : metric === 'cpu' ? 'Топ по CPU' : 'По названию')}${list.map((r) => K.row({ k: `${metric}-${r.id}`, on: !omitCursor && current && current.id === r.id, title: esc(r.title), sub: `${esc(r.place)}${r.missing ? ', нет замера' : ''}`, do: 'select', arg: r.id, st: r.missing ? 'wait' : r.session ? S.agentSt(r.session) : 'idle', trail: r.missing ? '—' : metric === 'cpu' ? cpu(r.cpu) : mem(r.mem) })).join('')}`;
  }
  function processList(row) {
    const s = row.session;
    return s ? (s.host === 'lost' && s.procs ? s.procs.map(([name, pid, started]) => ({ name, pid, started, depth: 0 })) : ensureProcesses(s)) : row.processes || [];
  }
  function tree(p, row) {
    const list = processList(row);
    return `${K.sec('Дерево процессов', list.length)}${list.map((proc) => K.row({ k: `${proc.pid}-${proc.started}`, indent: proc.depth + 1, icon: 'term', title: esc(proc.name), sub: `PID ${proc.pid}, ${esc(proc.started)}`, quiet: true, st: row.missing && row.missing.pid === proc.pid ? 'wait' : undefined })).join('')}${K.pad('<div class="row-s">Процесс учитывается по PID и времени старта, один раз за снимок. Общий рендер терминала относится к GUI.</div>')}`;
  }
  function detail(p) {
    const r = selected(p); if (!r) return empty(p);
    const s = r.session;
    return `${K.ctx(esc(r.title), s ? K.btn('К панели', { do: 'open', small: true, key: 'Enter' }) : '')}${K.scroll(`${r.missing ? K.banner('wait', 'Нет полного замера сессии', `${esc(r.missing.reason)}. Предыдущие значения: ${cpu(r.cpu)} CPU, ${mem(r.mem)}. Они не включены в текущий итог.`, K.btn('Повторить', { do: 'refresh', small: true })) : ''}${K.pad(K.kv([
      ['CPU', r.missing ? 'Нет замера' : cpu(r.cpu)], ['Память', r.missing ? 'Нет замера' : mem(r.mem)],
      ['Измерение', r.missing ? 'Частичное' : esc(sample().when)],
      s && ['Сессия', esc(s.id)], s && ['Состояние', esc(S.stateName(s))],
      s && ['Рабочая область', esc(W.workbenches[s.wb].title)], s && ['Каталог', esc(s.cwd)], ['Панель', esc(r.place)],
    ]))}${s ? K.pad(K.btn('Остановить сессию', { do: 'stop', key: 'X', kind: 'danger', disabled: !alive(s) })) : K.pad(`<div class="row-s">${r.id === '@other' ? 'Измеренные процессы других воркспейсов одной суммой. Из этого воркспейса подробности не видны.' : r.id === '@shared' ? 'GUI, рендер терминалов, scrollback, шрифты и Metal принадлежат общей части приложения. Они не распределяются поровну между панелями.' : 'Общая работа Git учитывается здесь. Дочерние git-процессы не прибавляются повторно к запросившей панели.'}</div>`)}${tree(p, r)}`)}`;
  }
  function select(p, id) {
    if (!rows().some((r) => r.id === id)) return;
    if (p.state.area !== 'detail') {
      const saved = { ...p.state }; const filter = p.filter;
      S.mark(p, () => { Object.assign(p.state, saved); p.filter = filter; });
    }
    p.state.selected = id; p.state.area = 'detail'; S.render();
  }
  function back(p) {
    const last = S.st.history[S.st.history.length - 1];
    if (last && last.panel === p.id && last.restore) { S.back(); return; }
    p.state.area = 'list'; S.render();
  }
  function stop(p) {
    const row = selected(p); const s = row && row.session; if (!s || !alive(s)) return;
    const generation = s.gen;
    const processes = processList(row).map((proc) => ({ ...proc }));
    const changed = () => W.sessions[s.id] !== s || s.gen !== generation || !alive(s);
    S.sheet({
      icon: 'stop', title: `Остановить ${esc(s.title)}?`, width: 500,
      sub: 'Программа и дочерние процессы завершатся. Выполняемая работа будет прервана, несохранённые данные могут пропасть.',
      state: { error: '' },
      body: (o) => `${K.kv([['Сессия', esc(s.id)], ['Поколение', generation], ['Рабочая область', esc(W.workbenches[s.wb].title)], ['Каталог', esc(s.cwd)], ['Панель', esc(row.place)]])}${K.sec('Будут остановлены')}${processes.map((proc) => K.row({ title: esc(proc.name), sub: `PID ${proc.pid}, ${esc(proc.started)}`, indent: proc.depth, quiet: true })).join('')}${changed() || o.state.error ? K.banner('err', o.state.error || 'Сессия изменилась', 'Закройте подтверждение и выберите сессию заново.') : ''}`,
      actions: [{ label: 'Отменить', key: 'Esc' }, {
        label: 'Остановить', kind: 'danger', key: 'Enter', close: false, disabled: changed,
        run: (o) => {
          if (changed()) { o.state.error = 'Сессия изменилась'; S.render(); return; }
          s.snapshot = s.snapshot || (s.tx || []).slice(-7); s.host = 'finished'; s.state = 'exited'; s.cpu = 0; s.mem = 0; s.procs = [];
          W.resources.processes[s.id] = { gen: s.gen, rows: [] }; delete sample().missing[s.id];
          p.state.area = 'list'; p.state.selected = null; S.ov.pop(o);
          S.notify({ ...S.owner(s.wb), title: `Сессия ${esc(s.title)} остановлена`, text: 'Процессы завершены. Панель и сохранённый вывод доступны в рабочей области.', tone: 'ok' }); S.render();
        },
      }],
    });
  }
  function side(p) {
    p.state.presentation = 'side';
    if (p.state.area === 'detail') return `${header(p, true)}${detail(p)}`;
    const list = visibleRows(p); const current = selected(p); const firstMetric = p.state.sort;
    const first = sorted(filtered(p), firstMetric).slice(0, 4);
    return `${header(p)}${K.scroll(`${partialBanner()}${measuredSummary(true)}${list.length ? `${top(p, firstMetric)}${top(p, firstMetric === 'cpu' ? 'mem' : 'cpu', first.some((r) => current && r.id === current.id))}${filtered(p).filter((r) => r.missing).map((r) => K.row({ title: esc(r.title), st: 'wait', sub: 'Нет замера, не включено в итог', do: 'select', arg: r.id })).join('')}` : empty(p)}${estimates(true)}`)}`;
  }
  function center(p, geo) {
    if (geo.size === 'Narrow') return side(p);
    p.state.presentation = 'center';
    const summary = `${partialBanner()}${measuredSummary()}${table(p, sorted(filtered(p), p.state.sort), geo.size !== 'Wide')}${estimates()}`;
    return `${header(p)}${geo.size === 'Wide' ? K.cols([{ html: K.scroll(summary) }, { w: '35%', html: detail(p) }]) : K.stack([K.scroll(summary), K.stack([detail(p)])])}`;
  }
  function bottom(p, geo) {
    if (geo.width < 620) return side(p);
    p.state.presentation = 'bottom';
    return `${geo.size === 'ShortBar' ? '' : header(p, p.state.area === 'detail')}${K.ctx(`<span>Измерено${measured().missing.length ? ', частично' : ''}: ${totalText(measured())}</span>`, `<span>Оценка: ${estimateText()}</span>`)}${K.cols([{ html: K.scroll(`${partialBanner()}${table(p, visibleRows(p), true)}${geo.size !== 'ShortBar' ? estimates(true) : K.pad('<div class="row-s">Оценка не складывается с измеренным. 100% CPU = одно ядро.</div>')}`) }, p.state.area === 'detail' ? { w: '40%', html: detail(p) } : null])}`;
  }
  function move(p, delta) {
    const list = visibleRows(p); const current = selected(p); const index = list.findIndex((r) => current && r.id === current.id);
    p.state.selected = (list[S.clamp(index + delta, 0, list.length - 1)] || {}).id || null; S.render();
  }
  function reopen(p, opts = {}) {
    if (opts.sid && W.sessions[opts.sid]) { p.filter = ''; select(p, opts.sid); }
  }
  S.plugin({
    id: 'resources', title: 'Ресурсы', icon: 'cpu', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'app', minSize: { w: 230, h: 90 }, inputDefault: false,
    init(p, opts) { Object.assign(p.state, { area: 'list', selected: null, sort: 'mem', presentation: 'center' }); reopen(p, opts); }, reopen,
    meter: () => ({ kind: 'shared', text: 'общее' }),
    tabStatus: () => measured().missing.length ? 'wait' : null,
    context(p) { const r = selected(p); return r ? [{ label: `${r.title}, ${r.missing ? 'нет замера' : `${cpu(r.cpu)} CPU, ${mem(r.mem)}`}`, icon: 'cpu' }] : []; },
    view: (p, geo) => K.adapt(geo, { left: () => side(p), right: () => side(p), center: () => center(p, geo), bottom: () => bottom(p, geo) }),
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
      open: { title: 'Перейти к панели', key: 'Enter', hint: 'К панели', when: (p) => !!(selected(p) || {}).session, run: (p) => { const row = selected(p); if (row && row.session) { p.state.selected = row.id; S.openSession(row.session.id); } } },
      sort: { title: 'Изменить сортировку', key: 's', hint: 'Сортировка', run: (p) => { const row = selected(p); p.state.sort = order[(order.indexOf(p.state.sort) + 1) % order.length]; if (row) p.state.selected = row.id; S.render(); } },
      stop: { title: 'Остановить выбранную сессию', key: 'X', hint: 'Остановить', when: (p) => !!(selected(p) || {}).session, run: stop },
      detail: { title: 'Показать дерево процессов', key: 'l', run: (p) => { const row = selected(p); if (row) select(p, row.id); } },
      back: { title: 'К сводке', key: 'h', when: (p) => p.state.area === 'detail', menu: false, run: back },
      refresh: { title: 'Повторить замер', key: 'u', run: () => { sample().missing = {}; sample().when = `Обновлено в ${new Date().toLocaleTimeString('ru-RU')}`; Object.values(W.sessions).filter(alive).forEach(ensureProcesses); S.render(); } },
      clearFilter: { title: 'Сбросить фильтр', when: (p) => !!p.filter, run: (p) => { p.filter = ''; S.render(); } },
      select: { title: '', menu: false, run: select },
    },
  });
  S.scenario({ id: 'resources.partial', group: 'Ресурсы', title: 'Частичный замер показан явно', run() {
    const s = Object.values(W.sessions).find((session) => alive(session) && session.id === 'make-test') || Object.values(W.sessions).find(alive);
    if (!s) { S.open('resources'); S.notify({ title: 'Нет работающих сессий', text: 'Запустите терминал, затем повторите сценарий.', tone: 'wait' }); return; }
    const proc = ensureProcesses(s).find((row) => row.depth === 1);
    sample().missing[s.id] = { pid: proc.pid, reason: `Не удалось измерить PID ${proc.pid}: нет доступа к процессу` };
    const p = S.open('resources'); p.filter = ''; p.state.area = 'list'; p.state.selected = s.id; S.render();
  } });
})();
