// Все проекты воркспейса. Одна модель и одна выборка в каждой зоне.
(() => {
  const St = S.st;
  const { esc } = S;
  const TABS = [['all', 'Все'], ['listed', 'В списке'], ['removed', 'Убранные']];
  const SORTS = { list: 'Как в списке', title: 'По имени', recent: 'По открытию', sessions: 'По сессиям' };
  const ORDER = ['title', 'recent', 'sessions'];
  const sortKey = (p) => (p.state.tab === 'listed' ? 'listedSort' : 'sort');
  const orderOf = (p) => (p.state.tab === 'listed' ? ['list', ...ORDER] : ORDER);

  const pool = (p) => S.projectsIn(S.wsOf(p.wb));
  const inTab = (pr, tab) => (tab === 'removed' ? !!pr.removedAt : !pr.removedAt && (tab === 'all' || S.listed(pr)));
  const live = (pr) => S.sessionsOf(pr.id).length;

  function rows(p) {
    const q = (p.filter || '').trim();
    const list = pool(p).filter((pr) => inTab(pr, p.state.tab) && (!q || S.match(`${pr.title} ${pr.roots.join(' ')}`, q) > 0));
    const byTitle = (a, b) => a.title.localeCompare(b.title, 'ru');
    const cmp = { list: (a, b) => a.listPos - b.listPos, title: byTitle, recent: (a, b) => b.lastOpenedAt - a.lastOpenedAt || byTitle(a, b), sessions: (a, b) => live(b) - live(a) || byTitle(a, b) }[p.state[sortKey(p)]];
    return list.sort(cmp);
  }
  const cursor = (p, list = rows(p)) => list.find((x) => x.id === p.state.cur) || list[0] || null;
  const picked = (p) => rows(p).filter((pr) => p.state.sel[pr.id]);
  const targets = (p) => { const sel = picked(p); if (sel.length) return sel; const c = cursor(p); return c ? [c] : []; };
  const removedTab = (p) => p.state.tab === 'removed';

  function step(p, d) {
    const list = rows(p); if (!list.length) return;
    const i = list.indexOf(cursor(p, list));
    p.state.cur = list[S.clamp(i + d, 0, list.length - 1)].id;
  }

  const tabs = (p, compact) => {
    const all = pool(p);
    const count = (id) => all.filter((pr) => inTab(pr, id)).length;
    if (compact) { const [id, label] = TABS.find(([x]) => x === p.state.tab); return K.btn(`${label} ${count(id)}`, { do: 'nextTab', small: true, key: 't', tip: 'Следующий раздел' }); }
    return K.seg(TABS.map(([id, label]) => ({ label, on: p.state.tab === id, do: 'tab', arg: id, count: count(id) })));
  };

  const emptyView = () => K.empty({
    icon: 'folder', title: 'В этом воркспейсе пока нет проектов',
    text: 'Добавьте папку, клонируйте репозиторий или найдите репозитории в папке.',
    actions: `${K.btn('Добавить папку', { do: 'add', kind: 'primary', key: 'a' })}${K.btn('Клонировать', { do: 'addClone' })}${K.btn('Найти в папке', { do: 'addScan' })}`,
  });
  const noRows = (p) => K.empty({ icon: 'filter', title: 'Проекты не найдены', text: p.filter ? 'Измените фильтр.' : 'В этом разделе пока ничего нет.', actions: p.filter ? K.btn('Сбросить фильтр', { do: 'clearFilter' }) : '' });

  const titleCell = (p, pr) => {
    if (p.state.renaming === pr.id) return `<input class="tree-rn" data-bind="renameVal" data-rename data-own-esc value="${esc(p.state.renameVal || '')}" spellcheck="false" autocomplete="off" aria-label="Имя проекта">`;
    const mark = pr.removedAt ? 'Убран' : S.listed(pr) ? 'В списке' : '';
    return `<span class="cat-t"><b>${esc(pr.title)}</b>${mark ? `<span class="cat-mark">${mark}</span>` : ''}</span>`;
  };
  const listBtn = (pr) => (pr.removedAt ? '' : K.iconBtn(S.listed(pr) ? 'minus' : 'plus', { do: 'listRow', arg: pr.id, tip: S.listed(pr) ? 'Скрыть из списка (x)' : 'Показать в списке (x)' }));
  const whenOpened = (pr) => (pr.lastOpenedAt ? (pr.lastOpenedAt > 100000 ? 'только что' : 'недавно') : 'не открывался');

  const bulkBar = (p, sel) => K.ctx(`<b style="font-weight:500">Выбрано: ${sel.length}</b>${K.btn('Снять выбор', { do: 'clearSel', small: true, kind: 'flat' })}`, removedTab(p)
    ? `${K.btn('Вернуть', { do: 'bulk', arg: 'restore', small: true, key: 'u' })}${K.btn('Забыть', { do: 'bulk', arg: 'forget', small: true, kind: 'danger', key: 'F' })}`
    : `${K.btn('Показать', { do: 'bulk', arg: 'show', small: true })}${K.btn('Скрыть', { do: 'bulk', arg: 'hide', small: true })}${K.btn('Перенести', { do: 'bulk', arg: 'move', small: true, key: 'm' })}${K.btn('Убрать из каталога', { do: 'bulk', arg: 'remove', small: true, key: 'D' })}`);

  function table(p, geo) {
    const list = rows(p); const cur = cursor(p, list); const sel = picked(p);
    const head = sel.length ? bulkBar(p, sel) : K.ctx(tabs(p), `${K.btn(SORTS[p.state[sortKey(p)]], { do: 'sort', small: true, key: 's', tip: 'Изменить сортировку' })}${K.btn('Найти в папке', { do: 'addScan', small: true })}${K.btn('Добавить', { icon: 'plus', do: 'add', small: true, kind: 'primary', key: 'a' })}`);
    if (!pool(p).length) return `${head}${emptyView()}`;
    if (!list.length) return `${head}${noRows(p)}`;
    const wide = geo.width >= 720;
    const all = sel.length === list.length ? true : sel.length ? 'mixed' : false;
    return `${head}${K.scroll(K.table({
      cols: [{ t: K.check(all, { do: 'selAll' }), w: '34px' }, { t: 'Проект' }, ...(wide ? [{ t: 'Папка' }, { t: 'Области', w: '78px', num: true }] : []), { t: 'Сессии', w: '68px', num: true }, ...(wide ? [{ t: 'Открыт', w: '116px' }] : []), { t: '', w: '76px' }],
      rows: list.map((pr) => ({
        k: pr.id, on: !!cur && cur.id === pr.id, dim: !!pr.removedAt, do: 'row', arg: pr.id,
        cells: [K.check(!!p.state.sel[pr.id], { do: 'sel', arg: pr.id }), titleCell(p, pr), ...(wide ? [`<span class="mono">${esc(pr.roots[0] || '')}</span>`, String(pr.workbenches.length)] : []), live(pr) ? String(live(pr)) : '', ...(wide ? [whenOpened(pr)] : []), `${listBtn(pr)}${K.iconBtn('more', { do: 'more', arg: pr.id, tip: 'Действия' })}`],
      })),
    }))}`;
  }

  function detail(p, pr, back) {
    const sessions = S.sessionsOf(pr.id);
    const acts = S.projActions(pr.id, p);
    const status = pr.removedAt ? 'Убран из каталога' : S.listed(pr) ? 'В списке слева' : 'Вне списка';
    const forge = pr.repos.some((r) => !S.forgeAccount(r)) ? 'Операции с GitHub и GitLab приостановлены: нет аккаунта в этом воркспейсе' : '';
    return `${back ? K.ctx(`${K.iconBtn('chevL', { do: 'toList', tip: 'К списку' })}${K.crumbs([esc(pr.title)])}`) : ''}${K.scroll(`${forge ? K.banner('wait', forge) : ''}${K.pad(K.kv([['Статус', status], ['Папка', `<span class="mono">${esc(pr.roots.join(', '))}</span>`], ['Репозитории', String(pr.repos.length)], ['Рабочие области', String(pr.workbenches.length)], ['Сессии', String(sessions.length)]]))}<div class="cat-actions">${acts.map((a) => K.btn(a.title, { do: 'act', arg: a.id, small: true, key: a.key, kind: a.id === 'forget' ? 'danger' : a.id === 'open' || a.id === 'restore' ? 'primary' : '' })).join('')}</div>${sessions.length ? `${K.sec('Сессии', sessions.length)}${sessions.map((s) => K.row({ k: s.id, st: S.agentSt(s), title: esc(s.title), sub: S.stateName(s), quiet: true })).join('')}` : ''}`)}`;
  }

  const list = (p, o = {}) => {
    const items = rows(p); const cur = cursor(p, items);
    const sel = picked(p);
    const head = sel.length ? bulkBar(p, sel) : K.ctx(tabs(p, o.compact), K.btn('', { icon: 'plus', do: 'add', small: true, kind: 'flat', tip: 'Добавить проект' }));
    if (!pool(p).length) return `${head}${emptyView()}`;
    if (!items.length) return `${head}${noRows(p)}`;
    return `${head}${K.scroll(items.map((pr) => K.row({
      k: pr.id, icon: 'folder', on: !!cur && cur.id === pr.id, dim: !!pr.removedAt, title: p.state.renaming === pr.id ? titleCell(p, pr) : esc(pr.title), sub: pr.roots[0] || '', do: 'row', arg: pr.id,
      lead: K.check(!!p.state.sel[pr.id], { do: 'sel', arg: pr.id }),
      trail: `${live(pr) ? `<span class="meta">${live(pr)}</span>` : ''}${listBtn(pr)}${K.iconBtn('more', { do: 'more', arg: pr.id, tip: 'Действия' })}`,
    })).join(''))}`;
  };

  const side = (p) => { const pr = p.state.area === 'detail' ? cursor(p) : null; return pr ? detail(p, pr, true) : list(p, { compact: true }); };
  const bottom = (p) => { const pr = cursor(p); return K.cols([{ w: '50%', html: list(p, { compact: true }) }, { html: pr ? detail(p, pr, false) : '' }]); };

  const applyOne = (p, name) => { const pr = cursor(p); if (!pr) return; const act = S.projActions(pr.id, p).find((a) => a.id === name); if (act) act.run(); };
  // Выбор нескольких строк всегда проходит через показ состава.
  const applyMany = (p, name, kind) => { const sel = picked(p); if (sel.length) S.projBulk(kind, sel.map((x) => x.id), () => { p.state.sel = {}; }); else applyOne(p, name); };
  const actionable = (p) => !removedTab(p) && !!cursor(p);
  const canOrder = (p) => p.state.tab === 'listed' && p.state.listedSort === 'list' && !(p.filter || '').trim() && !!cursor(p);
  const shift = (p, d) => { const pr = cursor(p); if (pr) { p.state.cur = pr.id; S.projListMove(pr.id, d); } };
  const isRemoved = (p) => { const pr = cursor(p); return !!pr && !!pr.removedAt; };

  S.plugin({
    id: 'projects', title: 'Все проекты', icon: 'folder', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'workspace', minSize: { w: 220, h: 90 }, surface: 'side',
    init(p) { Object.assign(p.state, { tab: 'all', sort: 'title', listedSort: 'list', sel: {}, cur: null, area: 'list' }); },
    confirmClose: (p) => { if (!S.isHomeKey(p.wb)) return false; S.notify({ title: 'Начальный экран воркспейса', text: 'Откройте проект, чтобы перейти к работе.' }); return true; },
    view: (p, geo) => K.adapt(geo, { left: () => side(p), right: () => side(p), center: () => table(p, geo), bottom: () => bottom(p) }),
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => { step(p, 1); S.render(); } },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => { step(p, -1); S.render(); } },
      open: { title: 'Открыть', key: 'Enter', hint: 'Открыть', menu: false, when: (p) => !!cursor(p), run: (p) => { const pr = cursor(p); if (pr) S.openProject(pr.id); } },
      deeper: { title: 'Подробности', key: 'l', menu: false, when: (p) => !!p.geo && p.geo.zone !== 'center' && p.geo.zone !== 'bottom' && p.state.area !== 'detail', run: (p) => { p.state.area = 'detail'; S.render(); } },
      toList: { title: 'К списку', key: 'h', menu: false, when: (p) => p.state.area === 'detail', run: (p) => { p.state.area = 'list'; S.render(); } },
      row: { title: '', menu: false, run: (p, id) => { if (p.state.cur === id) S.openProject(id); else p.state.cur = id; S.render(); } },
      more: { title: '', menu: false, run: (p, id) => S.projMenu(id, event.target.closest('button'), p) },
      act: { title: '', menu: false, run: (p, name) => applyOne(p, name) },
      tab: { title: '', menu: false, run: (p, id) => { p.state.tab = id; p.state.sel = {}; p.state.cur = null; p.state.area = 'list'; S.render(); } },
      nextTab: { title: 'Следующий раздел', key: 't', run: (p) => { const i = TABS.findIndex(([id]) => id === p.state.tab); p.state.tab = TABS[(i + 1) % TABS.length][0]; p.state.sel = {}; p.state.cur = null; S.render(); } },
      sort: { title: 'Изменить сортировку', key: 's', run: (p) => { const key = sortKey(p); const list = orderOf(p); p.state[key] = list[(list.indexOf(p.state[key]) + 1) % list.length]; S.render(); } },
      sel: { title: 'Выбрать строку', key: 'v', run: (p, id) => { const key = id || (cursor(p) || {}).id; if (key) p.state.sel[key] = !p.state.sel[key]; S.render(); } },
      selAll: { title: 'Выбрать все', key: 'V', run: (p) => { const list = rows(p); const on = picked(p).length !== list.length; p.state.sel = on ? Object.fromEntries(list.map((x) => [x.id, true])) : {}; S.render(); } },
      clearSel: { title: '', menu: false, run: (p) => { p.state.sel = {}; S.render(); } },
      bulk: { title: '', menu: false, run: (p, kind) => S.projBulk(kind, picked(p).map((x) => x.id), () => { p.state.sel = {}; }) },
      add: { title: 'Добавить проект', key: 'a', hint: 'Добавить', run: () => S.projAdd() },
      addClone: { title: '', menu: false, run: () => S.projAdd('clone') },
      addScan: { title: '', menu: false, run: () => S.projAdd('scan') },
      rename: { title: 'Переименовать', key: 'r', hint: 'Имя', when: actionable, run: (p) => applyOne(p, 'rename') },
      list: { title: 'Показать в списке или скрыть', key: 'x', when: actionable, run: (p) => { const sel = picked(p); if (!sel.length) { applyOne(p, 'list'); return; } S.projListMany(sel.map((x) => x.id), sel.some((x) => x.listPos == null), () => { p.state.sel = {}; }); } },
      listRow: { title: '', menu: false, run: (p, id) => { p.state.cur = id; S.projToggle(id); } },
      listUp: { title: 'Выше в списке', key: 'K', when: canOrder, run: (p) => shift(p, -1) },
      listDown: { title: 'Ниже в списке', key: 'J', when: canOrder, run: (p) => shift(p, 1) },
      moveWs: { title: 'Перенести в воркспейс', key: 'm', when: actionable, run: (p) => S.projMove(targets(p).map((x) => x.id)) },
      remove: { title: 'Убрать из каталога', key: 'D', when: actionable, run: (p) => applyMany(p, 'remove', 'remove') },
      restore: { title: 'Вернуть проект', key: 'u', when: isRemoved, run: (p) => applyMany(p, 'restore', 'restore') },
      forget: { title: 'Забыть проект', key: 'F', when: isRemoved, run: (p) => S.projForget(targets(p).map((x) => x.id)) },
      clearFilter: { title: '', menu: false, run: (p) => { p.filter = ''; S.render(); } },
    },
    tabTitle: () => 'Все проекты',
  });
})();
