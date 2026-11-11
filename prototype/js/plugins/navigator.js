// Навигатор: проекты активного воркспейса, области текущего проекта и сессии текущей области.
(() => {
  const St = S.st;
  const { esc } = S;
  const open = {};
  const toggle = (id) => { open[id] = !open[id]; };

  function wbSummary(id) {
    const wb = W.workbenches[id];
    const waiting = Object.values(W.sessions).some((s) => s.wb === id && s.kind === 'agent' && s.host !== 'finished' && s.state === 'waiting');
    const failed = W.pipelines.some((r) => r.state === 'failed' && r.repo === wb.repo && r.ref === wb.branch);
    if (waiting || failed) return `<span class="flags">${waiting ? '<span class="dot wait" aria-label="агент ждёт"></span>' : ''}${failed ? '<span class="f-err" aria-label="CI упал">✕</span>' : ''}</span>`;
    const drafts = id === 'review' ? W.mrs['!142'].drafts.length : 0;
    return drafts ? `<span class="meta">${S.fmt.plural(drafts, 'черновик', 'черновика', 'черновиков')}</span>` : '';
  }

  const keyOf = (it) => `${it.t}-${it.id}`;
  const selectable = (it) => it.t === 'project' || it.t === 'wb' || it.t === 'session';
  const LIMIT = 100;

  function items(p) {
    const cur = S.project();
    if (p.state.route !== S.lkey()) { p.state.route = S.lkey(); if (cur) open[cur.id] = true; }
    const g = S.projectGroups(St.route.ws, cur && cur.id);
    const q = (p.filter || '').trim();
    const out = [];
    const pushProject = (pr, extra) => {
      out.push({ t: 'project', id: pr.id, title: pr.title, pr, ...extra });
      if (q || !open[pr.id]) return;
      pr.workbenches.forEach((wid) => out.push({ t: 'wb', id: wid, title: W.workbenches[wid].title, parent: pr.id }));
    };
    if (q) {
      const hits = (list) => list.map((pr) => ({ pr, s: S.projScore(pr, q) })).filter((x) => x.s > 0).sort((a, b) => b.s - a.s).map((x) => x.pr);
      const inside = hits(g.listed); const outside = hits(g.outside);
      inside.forEach((pr) => pushProject(pr));
      if (outside.length) {
        out.push({ t: 'sect', id: 'outside', title: 'Не в списке', sub: outside.length });
        outside.slice(0, LIMIT).forEach((pr) => pushProject(pr));
        if (outside.length > LIMIT) out.push({ t: 'cap', id: 'cap', total: outside.length });
      }
      if (!inside.length && !outside.length) out.push({ t: 'none', id: 'nohit' });
    } else {
      out.push({ t: 'sect', id: 'projects', title: 'Проекты', sub: g.total ? `${g.listed.length} из ${g.total}` : '', add: true });
      g.listed.forEach((pr) => pushProject(pr));
      if (g.here) pushProject(g.here, { temp: true });
      if (!g.listed.length && !g.here) out.push({ t: 'empty', id: 'empty', any: g.total > 0, removed: S.projectsIn(St.route.ws).filter((x) => x.removedAt).length });
    }
    const here = St.route.wb;
    if (here) {
      const live = Object.values(W.sessions).filter((s) => s.wb === here && !s.forgotten);
      out.push({ t: 'sect', id: 'sessions', title: 'Сессии', sub: W.workbenches[here].title });
      live.filter((s) => !s.background).forEach((s) => out.push({ t: 'session', id: s.id, s }));
      const bg = live.filter((s) => s.background);
      if (bg.length) { out.push({ t: 'sect', id: 'bg', title: 'В фоне' }); bg.forEach((s) => out.push({ t: 'session', id: s.id, s, bg: true })); }
    }
    return out;
  }

  // Курсор привязан к строке, а не к номеру: показ, скрытие и порядок переставляют список.
  function resolve(p, list) {
    let i = list.findIndex((it) => keyOf(it) === p.state.curKey);
    if (i < 0) i = S.clamp(p.state.cur || 0, 0, Math.max(0, list.length - 1));
    if (list[i] && !selectable(list[i])) {
      const next = list.findIndex((it, n) => n > i && selectable(it));
      i = next >= 0 ? next : list.reduce((last, it, n) => (selectable(it) ? n : last), i);
    }
    p.state.cur = i;
    p.state.curKey = list[i] ? keyOf(list[i]) : null;
    return i;
  }

  function step(p, d) {
    const list = items(p);
    let i = resolve(p, list) + d;
    while (list[i] && !selectable(list[i])) i += d;
    if (list[i]) { p.state.cur = i; p.state.curKey = keyOf(list[i]); }
  }

  const sessStatus = (s) => {
    const st = S.agentSt(s);
    if (s.host !== 'ok') return { status: S.stateName(s), dot: st, stCls: st === 'err' ? 'err' : '' };
    if (s.kind === 'editor') return { status: s.editorState ? s.editorState.file.split('/').pop() : '', dot: 'none', stCls: '' };
    // Агент, которого хук нашёл в обычном терминале, виден с состоянием агента.
    if (s.hooked) return { status: `${s.hooked.title}: ${S.stateName(s.hooked)}`, dot: S.agentSt(s.hooked), stCls: s.hooked.state === 'waiting' ? 'wait' : '' };
    if (s.kind === 'term') return s.state === 'running' ? { status: 'выполняется', dot: 'run', stCls: '' } : { status: s.cwd, dot: 'none', stCls: '' };
    return { status: S.stateName(s), dot: st, stCls: s.state === 'waiting' ? 'wait' : '' };
  };

  const sessActions = () => `${K.iconBtn('agent', { cmd: 'agent.new', tip: 'Новый агент' })}${K.iconBtn('term', { cmd: 'terminal.new', tip: 'Новый терминал' })}`;
  const addAction = () => `<span class="nav-add">${K.iconBtn('plus', { do: 'add', tip: 'Выбрать проекты для списка (a)' })}</span>`;

  function projectTrail(it) {
    const pr = it.pr;
    if (it.temp) return `<span class="meta">${pr.removedAt ? 'Убран' : 'Вне списка'}</span>`;
    if (open[pr.id] && pr.workbenches.length) return '';
    return pr.workbenches.map(wbSummary).find((x) => x.includes('flags')) || '';
  }

  const rowOf = (p, it, isCur, focused) => {
    const kb = focused && isCur;
    const k = keyOf(it);
    switch (it.t) {
      case 'project':
        if (p.state.renaming === it.id) return `<div class="tree tree-edit" data-k="${esc(k)}">${S.icon('folder', 14)}<input class="tree-rn" data-bind="renameVal" data-rename data-own-esc value="${esc(p.state.renameVal || '')}" spellcheck="false" autocomplete="off" aria-label="Имя проекта"></div>`;
        return K.navTree({ k, kb, chev: !!open[it.id], chevDo: 'toggle', more: 'more', quick: it.pr.removedAt ? { do: 'quick', icon: 'undo', tip: 'Вернуть проект (u)' } : S.listed(it.pr) ? { do: 'quick', icon: 'minus', tip: 'Скрыть из списка (x)' } : { do: 'quick', icon: 'plus', tip: 'Показать в списке (x)' }, icon: 'folder', title: esc(it.title), trail: projectTrail(it), do: 'pick', arg: k, reveal: !focused && S.project() === it.pr && !open[it.id] });
      case 'wb': return K.navTree({ k, kb, level: 2, sel: St.route.wb === it.id, reveal: !focused && St.route.wb === it.id, icon: 'branch', title: esc(it.title), trail: wbSummary(it.id), do: 'pick', arg: k });
      case 'sect': return K.navSect(it.title, it.sub, it.id === 'sessions' ? (S.wb() ? sessActions() : '') : it.add ? addAction() : '', !!it.add);
      case 'none': return `<div class="nav-none" data-k="${k}">Ничего не найдено</div>`;
      case 'cap': return `<div class="nav-none" data-k="${k}">Показано ${LIMIT} из ${it.total}<button type="button" class="lnk" data-do="catalog">Открыть каталог</button></div>`;
      case 'empty':
        if (it.any) return `<div class="nav-none" data-k="${k}">В списке пока нет проектов.<br>${K.btn('Выбрать проекты', { do: 'add', small: true, key: 'a' })}</div>`;
        return `<div class="nav-none" data-k="${k}">В этом воркспейсе пока нет проектов.<br>${K.btn('Добавить проект', { do: 'add', small: true, key: 'a' })}${it.removed ? `<button type="button" class="lnk" data-do="catalog">Убранные в каталоге · ${it.removed}</button>` : ''}</div>`;
      case 'session': {
        const s = it.s;
        const f = S.focused();
        return K.navSess({ k, kb, sel: !!f && f.state.sid === s.id, icon: s.kind === 'agent' || s.hooked ? 'agent' : s.kind === 'editor' ? 'file' : 'term', title: s.title, ...sessStatus(s), do: 'pick', arg: k, tip: `${s.title}: ${S.stateName(s)}${it.bg ? '. Панель не открыта, процесс продолжает работать' : ''}` });
      }
      default: return '';
    }
  };

  function pick(p, key) {
    const list = items(p);
    const it = list.find((x) => keyOf(x) === key); if (!it) return;
    p.state.curKey = key;
    if (it.t === 'project') { p.filter = ''; p.filtering = false; S.openProject(it.id); }
    else if (it.t === 'wb') S.openWb(it.id);
    else if (it.t === 'session') S.openSession(it.id);
  }

  const current = (p) => { const list = items(p); return list[resolve(p, list)] || null; };
  const curProject = (p) => { const it = current(p); return it && it.t === 'project' ? it.pr : null; };

  function tree(p) {
    const list = items(p);
    if (p.state.lastQ !== (p.filter || '')) { p.state.lastQ = p.filter || ''; p.state.curKey = null; p.state.cur = 0; }
    const i = resolve(p, list);
    const focused = S.focused() === p && !!St.kbNav;
    const cut = list.findIndex((it) => it.t === 'sect' && it.id === 'sessions');
    const render = (from, to) => list.slice(from, to).map((it, n) => rowOf(p, it, from + n === i, focused)).join('');
    const end = cut >= 0 ? cut : list.length;
    const chip = p.filter && !p.filtering ? `<button type="button" class="nav-filter" data-do="clearFilter" title="Сбросить фильтр" data-k="nav-filter">${S.icon('filter', 12)}<span>${esc(p.filter)}</span>${S.icon('close', 11)}</button>` : '';
    const gap = p.filtering ? '<div class="nav-gap" data-k="nav-gap"></div>' : '';
    return `<div class="nav-col" data-k="nav-col"><div class="scroll x-hidden nav-top" data-k="nav-top">${gap}${chip}${render(0, end)}</div>${cut >= 0 ? `<div class="scroll x-hidden nav-bot" data-k="nav-bot">${render(cut, list.length)}</div>` : ''}</div>`;
  }

  const goneMark = (pr) => (pr.removedAt ? `${K.chip('Убран')}${K.btn('Вернуть', { do: 'restore', small: true, key: 'u' })}` : '');

  function overview(p) {
    const pr = S.project();
    if (!pr) return tree(p);
    const attention = [];
    S.agents().filter((a) => a.state === 'waiting').forEach((a) => attention.push(K.row({ k: a.id, st: 'wait', title: `${a.title} ждёт подтверждения`, sub: 'Откройте сессию и ответьте', do: 'openS', arg: a.id, tall: true })));
    W.pipelines.filter((x) => x.state === 'failed' && pr.repos.includes(x.repo)).forEach((x) => attention.push(K.row({ k: x.id, st: 'err', title: `Запуск ${x.id} упал на ${x.failed}`, sub: `${x.repo}, ${x.ref}`, cmd: 'pipelines', tall: true })));
    Object.values(W.mrs).filter((m) => m.drafts.length && pr.repos.includes(m.repo)).forEach((m) => attention.push(K.row({ k: m.id, icon: 'draft', title: `${m.id} ${m.title}`, sub: `${S.fmt.plural(m.drafts.length, 'черновик ждёт', 'черновика ждут', 'черновиков ждут')} отправки`, do: 'openR', arg: m.id, tall: true })));
    return K.scroll(`<div style="max-width:880px;margin:0 auto;padding:18px 20px">
      <div style="display:flex;align-items:center;gap:8px;font-size:var(--t-large);line-height:26px;font-weight:600"><span>${pr.title}</span>${goneMark(pr)}</div>
      <div style="color:var(--text-2);margin-bottom:12px">${S.ws().title}, ${S.fmt.plural(pr.repos.length, 'репозиторий', 'репозитория', 'репозиториев')}, ${S.fmt.plural(pr.workbenches.length, 'рабочая область', 'рабочие области', 'рабочих областей')}</div>
      ${K.cols([
        { html: `${K.sec('Требует внимания', attention.length || '')}${attention.join('') || `<div class="pad" style="color:var(--text-2)">Всё спокойно.</div>`}${K.sec('Рабочие области')}${pr.workbenches.map((w) => K.row({ k: w, icon: 'grid', title: W.workbenches[w].title, sub: W.workbenches[w].branch, trail: wbSummary(w), do: 'goW', arg: w, tall: true })).join('')}` },
        { w: '300px', html: `${K.sec('Репозитории')}${pr.repos.map((r) => K.row({ k: r, icon: W.repos[r].provider, title: r, sub: `${W.repos[r].branch}, ${W.repos[r].path}`, trail: W.repos[r].provider === 'gitlab' ? 'GitLab' : 'GitHub', tall: true })).join('')}${K.sec('Агенты')}${S.agents().map((a) => K.row({ k: a.id, st: S.agentSt(a), title: a.title, sub: a.profile, trail: S.stateName(a), do: 'openS', arg: a.id, tall: true })).join('')}` },
      ])}
    </div>`);
  }

  function strip() {
    const pr = S.project();
    if (!pr) return `<div style="display:flex;align-items:center;gap:6px;padding:8px 10px">${K.crumbs([S.ws().title])}<span class="grow"></span>${K.btn('Выбрать проекты', { do: 'add', small: true, key: 'a' })}</div>`;
    return `<div style="display:flex;align-items:center;gap:6px;padding:8px 10px;flex-wrap:wrap">${K.crumbs([S.ws().title, pr.title, S.wb().title])}${goneMark(pr)}<span class="grow"></span>${pr.workbenches.map((w) => K.btn(W.workbenches[w].title, { small: true, do: 'goW', arg: w, kind: w === St.route.wb ? 'primary' : '' })).join('')}${K.btn('', { icon: 'plus', cmd: 'wb.new', kind: 'flat', small: true })}</div>`;
  }

  const onProject = (p) => { const pr = curProject(p); return !!pr && !pr.removedAt; };
  // В дереве возврат относится к строке под курсором, в центре и внизу строк нет, и он относится к открытому проекту.
  const removedHere = (p) => {
    if (!p.geo) return null;
    const pr = K.adapt(p.geo, { left: () => curProject(p), right: () => curProject(p), center: () => S.project(), bottom: () => S.project() });
    return pr && pr.removedAt ? pr : null;
  };
  const canOrder = (p) => { const pr = curProject(p); return !!pr && S.listed(pr) && !(p.filter || '').trim(); };
  const withProject = (fn) => (p) => { const pr = curProject(p); if (pr) fn(pr, p); };

  S.plugin({
    id: 'navigator', title: 'Навигатор', icon: 'grid', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'left', cardinality: 'workbench', minSize: { w: 150, h: 60 }, surface: 'side', chromeless: true,
    init(p) { p.state.cur = 0; p.state.curKey = St.route.wb ? `wb-${St.route.wb}` : null; },
    view: (p, geo) => K.adapt(geo, { left: () => tree(p), right: () => tree(p), center: () => overview(p), bottom: () => strip() }),
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => { step(p, 1); S.render(); } },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => { step(p, -1); S.render(); } },
      pick: { title: 'Открыть', key: 'Enter', hint: 'Открыть', menu: false, run: (p, a) => { pick(p, a == null ? p.state.curKey : a); S.render(); } },
      // l раскрывает проект и не сворачивает его. На текущей области уходит к общему переходу между панелями.
      pickRight: { title: 'Раскрыть', key: 'l', menu: false, when: (p) => { const it = current(p); return !(it && it.t === 'wb' && it.id === St.route.wb); }, run: (p) => { const it = current(p); if (!it) return; if (it.t === 'project') open[it.id] = true; else pick(p, p.state.curKey); S.render(); } },
      collapse: { title: 'Свернуть', key: 'h', menu: false, run: (p) => { const it = current(p); if (!it) return; if (it.t === 'wb') p.state.curKey = `project-${it.parent}`; else if (open[it.id]) open[it.id] = false; S.render(); } },
      toggle: { title: '', menu: false, run: (p, key) => { const it = items(p).find((x) => keyOf(x) === key); if (it) { toggle(it.id); p.state.curKey = key; } S.render(); } },
      more: { title: '', menu: false, run: (p, key) => { const it = items(p).find((x) => keyOf(x) === key); if (it) S.projMenu(it.id, event.target.closest('[data-do]'), p); } },
      hide: { title: 'Показать в списке или скрыть', key: 'x', hint: (p) => { const pr = curProject(p); return pr && !S.listed(pr) ? 'В список' : 'Скрыть'; }, when: onProject, run: withProject((pr) => S.projToggle(pr.id)) },
      add: { title: 'Выбрать проекты для списка', key: 'a', hint: 'Выбрать', run: () => S.projPick() },
      quick: { title: '', menu: false, run: (p, key) => { const it = items(p).find((x) => keyOf(x) === key); if (!it || it.t !== 'project') return; p.state.curKey = key; if (it.pr.removedAt) S.projRestore(it.id); else S.projToggle(it.id); } },
      restore: { title: 'Вернуть проект', key: 'u', hint: 'Вернуть', when: (p) => !!removedHere(p), run: (p) => { const pr = removedHere(p); if (pr) S.projRestore(pr.id); } },
      rename: { title: 'Переименовать проект', key: 'r', when: onProject, run: withProject((pr, p) => S.startRename(p, pr.id)) },
      moveWs: { title: 'Перенести в воркспейс', key: 'm', when: onProject, run: withProject((pr) => S.projMove([pr.id])) },
      remove: { title: 'Убрать из каталога', key: 'D', when: onProject, run: withProject((pr) => S.projRemove(pr.id)) },
      listUp: { title: 'Выше в списке', key: 'K', when: canOrder, run: withProject((pr) => S.projListMove(pr.id, -1)) },
      listDown: { title: 'Ниже в списке', key: 'J', when: canOrder, run: withProject((pr) => S.projListMove(pr.id, 1)) },
      catalog: { title: '', menu: false, run: (p) => { const c = S.open('projects', { zone: 'center' }); if (c) { c.state.tab = (p.filter || '').trim() ? 'all' : 'removed'; c.state.sel = {}; c.filter = (p.filter || '').trim(); } S.render(); } },
      clearFilter: { title: '', menu: false, run: (p) => { p.filter = ''; S.render(); } },
      newWb: { title: 'Новая рабочая область', key: 'n', when: () => !!S.wb(), run: () => S.newWorkbench() },
      openS: { title: '', run: (p, id) => S.openSession(id) },
      openR: { title: '', run: (p, id) => S.open('review', { key: id, target: id }) },
      goW: { title: '', run: (p, id) => S.openWb(id) },
    },
  });
})();
