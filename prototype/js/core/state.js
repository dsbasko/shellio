// Модель приложения: панели, раскладка, фокус, режимы, история переходов.
(() => {
  const St = S.st = {
    route: { ws: 'work', wb: 'feature-sso' },
    lastKey: {},
    histories: {},
    layouts: {},
    panels: {},
    overlays: [],
    pending: [],
    prefs: { theme: 'dark', contrast: false, motion: false, crossAttention: false },
    keymap: {},
    tips: { input: false },
    empty: false,
  };

  // История переходов у каждого воркспейса своя.
  Object.defineProperty(St, 'history', {
    get: () => St.histories[St.route.ws] || (St.histories[St.route.ws] = []),
    set: (v) => { St.histories[St.route.ws] = v; },
  });

  S.plugins = {};
  S.plugin = (def) => { S.plugins[def.id] = def; };
  // Маршрут: WorkspaceHome, когда открывать нечего, иначе рабочая область.
  S.wb = () => (St.route.wb ? W.workbenches[St.route.wb] : null);
  S.project = () => { const w = S.wb(); return w ? W.projects[w.project] : null; };
  S.repo = () => { const w = S.wb(); return w ? W.repos[w.repo] : null; };
  S.ws = () => W.workspaces.find((w) => w.id === St.route.ws);
  S.lkey = () => St.route.wb || `home:${St.route.ws}`;
  S.isHomeKey = (key) => key.startsWith('home:');
  S.wsOf = (key) => { if (S.isHomeKey(key)) return key.slice(5); const w = W.workbenches[key]; return w ? W.projects[w.project].ws : null; };
  S.owner = (key) => { const w = W.workbenches[key]; return { ws: S.wsOf(key), project: w ? w.project : null }; };
  // Уведомление о сообщении между проектами принадлежит обоим и переезжает только с обоими.
  S.threadToast = (source, to) => {
    const mine = S.owner(source); const peer = W.workbenches[to] ? W.workbenches[to].project : null;
    const cross = !!peer && peer !== mine.project;
    return { ws: mine.ws, project: cross ? null : mine.project, parts: cross ? [mine.project, peer] : undefined, wb: source };
  };
  S.setRoute = (key) => {
    if (S.isHomeKey(key)) { St.route.ws = key.slice(5); St.route.wb = null; return; }
    St.route.wb = key; St.route.ws = S.wsOf(key);
  };
  S.noteOpen = (key) => { const pr = W.projects[W.workbenches[key].project]; pr.lastWb = key; pr.lastOpenedAt = Date.now(); };

  // Зона делится на части по одной оси: боковые одна над другой, центр и нижняя рядом (ADR-0031).
  S.AXIS = { left: 'stack', right: 'stack', center: 'row', bottom: 'row' };
  S.ZONE_TITLE = { left: 'Левая зона', right: 'Правая зона', center: 'Центр', bottom: 'Нижняя зона' };
  const group = (zone, w = 1) => ({ id: S.uid('g'), zone, tabs: [], active: null, w });
  // Навигатор живёт в своей группе левой зоны и не делится: делится плитка панелей рядом с ним.
  const blank = () => {
    const nav = group('left'); nav.nav = true;
    const l = group('left'); const r = group('right'); const b = group('bottom'); const c = group('center');
    return { groups: { [nav.id]: nav, [l.id]: l, [r.id]: r, [b.id]: b, [c.id]: c }, nav: nav.id, parts: { left: [l.id], right: [r.id], bottom: [b.id], center: [c.id] }, last: {}, size: { left: 236, right: 372, bottom: 236, plate: 372 }, hidden: { left: false, right: false, bottom: false, plate: false }, zoom: null, focus: null };
  };
  S.layout = (wb = S.lkey()) => St.layouts[wb] || (St.layouts[wb] = blank());
  S.resetLayouts = () => { St.layouts = {}; St.panels = {}; St.history = []; };

  const groups = (L) => Object.values(L.groups);
  const groupOf = (L, id) => groups(L).find((g) => g.tabs.includes(id));
  S.groupOf = groupOf;
  const partsOf = (L, zone) => L.parts[zone].map((id) => L.groups[id]);
  S.partsOf = partsOf;
  S.zoneTabs = (L, zone) => partsOf(L, zone).flatMap((g) => g.tabs);
  const centerGroups = (L) => partsOf(L, 'center');
  S.centerGroups = centerGroups;
  // Новая панель открывается в части с фокусом, иначе в последней активной части зоны.
  const zoneGroup = (L, zone) => {
    const g = L.focus && groupOf(L, L.focus);
    if (g && g.zone === zone && !g.nav) return g;
    return L.parts[zone].includes(L.last[zone]) ? L.groups[L.last[zone]] : partsOf(L, zone)[0];
  };
  // Новая часть встаёт рядом с указанной и забирает половину её доли.
  const newPart = (L, ref, side = 'after') => {
    const g = group(ref.zone, ref.w / 2); ref.w /= 2;
    L.groups[g.id] = g;
    const list = L.parts[ref.zone]; list.splice(list.indexOf(ref.id) + (side === 'before' ? 0 : 1), 0, g.id);
    return g;
  };

  S.panel = (id) => St.panels[id];
  S.focused = () => { const L = S.layout(); return L.focus ? St.panels[L.focus] : null; };
  S.panelsOf = (wb = S.lkey()) => Object.values(St.panels).filter((p) => p.wb === wb);

  // Левая зона показывает навигатор и панели раздельно, поэтому у них свои флаги.
  const reveal = (L, g, id) => {
    if (g.zone === 'center') return;
    if (g.zone !== 'left') { L.hidden[g.zone] = false; return; }
    if (g.nav) L.hidden.left = false; else L.hidden.plate = false;
  };
  // at: { split: groupId, side } ставит панель в новую часть рядом с группой, { group } в существующую часть.
  function place(p, zone, L, at = {}) {
    const ref = L.groups[at.split || at.group];
    const g = ref && at.split ? newPart(L, ref, at.side) : ref || (S.plugins[p.plugin].chromeless && zone === 'left' ? L.groups[L.nav] : zoneGroup(L, zone));
    g.tabs.push(p.id); g.active = p.id; reveal(L, g, p.id); return g;
  }

  S.create = (pluginId, opts = {}) => {
    const def = S.plugins[pluginId];
    const p = { id: S.uid(pluginId), plugin: pluginId, wb: opts.wb || S.lkey(), state: {}, mode: def.inputDefault ? 'input' : 'nav', key: opts.key || null, filter: '' };
    St.panels[p.id] = p;
    if (def.init) def.init(p, opts);
    return p;
  };

  S.findPanel = (pluginId, opts = {}) => {
    const def = S.plugins[pluginId];
    const wb = opts.wb || S.lkey();
    const all = Object.values(St.panels).filter((p) => p.plugin === pluginId);
    switch (def.cardinality) {
      case 'app': return all[0] || null;
      case 'workspace': return all.find((p) => S.wsOf(p.wb) === S.wsOf(wb)) || null;
      case 'project': { const w = W.workbenches[wb]; return (w && all.find((p) => W.workbenches[p.wb] && W.workbenches[p.wb].project === w.project)) || null; }
      case 'workbench': return all.find((p) => p.wb === wb) || null;
      default: return opts.key ? all.find((p) => p.wb === wb && p.key === opts.key) || null : null;
    }
  };

  S.open = (pluginId, opts = {}) => {
    const gone = opts.wb && opts.focus !== false ? S.removedOwner(opts.wb) : null;
    if (gone) { S.projRestorePrompt(gone.id, () => S.open(pluginId, opts)); return null; }
    const def = S.plugins[pluginId];
    const found = S.findPanel(pluginId, opts);
    if (found) {
      const moved = (def.cardinality === 'app' || def.cardinality === 'workspace') && found.wb !== S.lkey();
      if (moved) {
        const from = S.layout(found.wb);
        detach(from, found.id); collapse(from);
        if (from.zoom === found.id) from.zoom = null;
        if (from.focus === found.id) from.focus = groups(from).map((x) => x.active).find(Boolean) || null;
        found.wb = S.lkey();
        place(found, opts.zone || def.defaultZone || 'center', S.layout(found.wb));
      } else if (opts.at) S.moveTo(found.id, opts.at, { focus: false });
      if (def.reopen) def.reopen(found, { ...opts, moved });
      if (opts.focus !== false) S.focus(found.id); else { const L = S.layout(found.wb); const g = groupOf(L, found.id); if (g) { g.active = found.id; reveal(L, g, found.id); } }
      S.render();
      return found;
    }
    const p = S.create(pluginId, opts);
    const L = S.layout(p.wb);
    place(p, opts.zone || def.defaultZone || 'center', L, opts.at);
    if (opts.focus !== false) S.focus(p.id);
    S.render();
    return p;
  };

  S.focus = (id, o = {}) => {
    const p = St.panels[id]; if (!p) return;
    if (p.wb !== S.lkey()) S.setRoute(p.wb);
    const L = S.layout(p.wb);
    const g = groupOf(L, id); if (!g) return;
    g.active = id;
    if (!g.nav) L.last[g.zone] = g.id;
    reveal(L, g, id);
    if (L.zoom && L.zoom !== id) L.zoom = null;
    if (L.focus && L.focus !== id && !o.noHistory) St.history.push({ wb: p.wb, panel: L.focus });
    L.focus = id;
    if (document.activeElement && document.activeElement.matches && document.activeElement.matches('input,textarea')) document.activeElement.blur();
    const def = S.plugins[p.plugin];
    if (def.onFocus) def.onFocus(p);
    S.render();
  };

  // Плагин отмечает шаг вглубь внутри панели: Ctrl-o вернёт его.
  S.mark = (p, restore) => { St.history.push({ panel: p.id, wb: p.wb, restore }); };

  // Открытая область убранного проекта доступна, переход в другую начинается с его возврата.
  S.removedOwner = (wb) => {
    const area = W.workbenches[wb]; const pr = area && W.projects[area.project];
    return pr && pr.removedAt && wb !== St.route.wb ? pr : null;
  };

  S.back = () => {
    while (St.history.length) {
      const h = St.history[St.history.length - 1];
      if (!St.panels[h.panel] || S.wsOf(St.panels[h.panel].wb) !== St.route.ws) { St.history.pop(); continue; }
      const gone = S.removedOwner(St.panels[h.panel].wb);
      if (gone) { S.projRestorePrompt(gone.id, () => S.back()); return; }
      St.history.pop();
      if (h.restore) { h.restore(); S.focus(h.panel, { noHistory: true }); return; }
      S.focus(h.panel, { noHistory: true });
      return;
    }
    S.notify({ title: 'Некуда возвращаться', text: 'История переходов пуста.' });
  };

  S.go = (wb) => {
    if (St.route.wb !== wb) {
      const L = S.layout();
      if (S.wsOf(wb) === St.route.ws) St.history.push({ wb: S.lkey(), panel: L.focus });
      else St.lastKey[St.route.ws] = S.lkey();
      S.setRoute(wb);
      S.noteOpen(wb);
      if (S.seedLayout) S.seedLayout(wb);
      const NL = S.layout(wb);
      if (NL.focus) { const def = S.plugins[St.panels[NL.focus].plugin]; if (def.onFocus) def.onFocus(St.panels[NL.focus]); }
    }
    S.render();
  };

  function detach(L, id) {
    const g = groupOf(L, id); if (!g) return null;
    const i = g.tabs.indexOf(id);
    g.tabs.splice(i, 1);
    if (g.active === id) g.active = g.tabs[Math.min(i, g.tabs.length - 1)] || null;
    return g;
  }

  // Пустая часть исчезает, соседние занимают её место. Последняя часть зоны остаётся пустой группой.
  function collapse(L) {
    Object.keys(L.parts).forEach((zone) => {
      const ids = L.parts[zone];
      const full = ids.filter((id) => L.groups[id].tabs.length);
      const keep = full.length ? full : [ids[0]];
      ids.filter((id) => !keep.includes(id)).forEach((id) => delete L.groups[id]);
      L.parts[zone] = keep;
    });
  }

  S.close = (id, o = {}) => {
    const p = St.panels[id]; if (!p) return;
    const def = S.plugins[p.plugin];
    if (!o.force && def.confirmClose && def.confirmClose(p)) return;
    const L = S.layout(p.wb);
    const g = detach(L, id);
    if (def.destroy) def.destroy(p, o);
    delete St.panels[id];
    collapse(L);
    if (L.zoom === id) L.zoom = null;
    if (L.focus === id) {
      const back = [...St.history].reverse().find((h) => h.wb === p.wb && h.panel !== id && St.panels[h.panel] && St.panels[h.panel].wb === p.wb && !h.restore);
      const next = (back && back.panel) || (g && g.active && L.groups[g.id] && g.active) || centerGroups(L).map((x) => x.active).find(Boolean) || groups(L).map((x) => x.active).find(Boolean) || null;
      L.focus = next;
      if (next) S.focus(next, { noHistory: true });
    }
    S.render();
  };

  S.move = (id, zone) => S.moveTo(id, { zone });

  // Перенос в зону, в часть ({ group }) или в новую часть рядом с частью ({ split, side }).
  S.moveTo = (id, t, o = {}) => {
    const p = St.panels[id]; const L = S.layout(p.wb); const def = S.plugins[p.plugin];
    const ref = L.groups[t.split || t.group] || null;
    const zone = ref ? ref.zone : t.zone;
    if (def.placements && !def.placements.includes(zone)) { S.notify({ title: 'Нельзя перенести', text: `${def.title} не живёт в этой зоне.`, tone: 'wait' }); return; }
    const cur = groupOf(L, id);
    if (!ref && cur.zone === zone) return;
    if (t.group && cur === ref) return;
    if (t.split && cur === ref && cur.tabs.length === 1) return;
    const stay = t.split ? ref.tabs.filter((x) => x !== id) : [];
    if (stay.length) {
      const room = S.splitRoom(ref, [St.panels[stay.includes(ref.active) ? ref.active : stay[0]].plugin, p.plugin]);
      if (!room.ok) { tight(ref, room); return; }
    }
    detach(L, id);
    if (L.focus === id) L.focus = null;
    place(p, zone, L, ref ? t : {});
    collapse(L);
    if (o.focus === false) { S.render(); return; }
    S.focus(id, { noHistory: true });
  };

  // Сколько места займёт панель вдоль оси зоны. Ниже этого часть покажет заглушку.
  const minOf = (pluginId) => S.plugins[pluginId].minSize || { w: 160, h: 90 };
  const KEY = { row: 'Ctrl-w v', stack: 'Ctrl-w s' };
  const AXIS_TEXT = { row: 'рядом', stack: 'одна над другой' };
  // Отказ клавиши не той оси: зона, её ось и действующая клавиша этой оси из keymap.
  S.axisText = (zone) => { const axis = S.AXIS[zone]; const key = S.keyFor(axis === 'row' ? 'split.v' : 'split.s', KEY[axis]); return `${S.ZONE_TITLE[zone]} делится ${AXIS_TEXT[axis]}${key ? `: ${key}` : ''}`; };
  S.splitAxis = () => { const L = S.layout(); const g = L.focus && groupOf(L, L.focus); return g && !g.nav ? S.AXIS[g.zone] : null; };

  // Хватит ли места остающейся и новой части: plugins — [остающаяся, новая]. Деление пересчитывает
  // всю зону по долям, поэтому место под новый разделитель отдают все её части, а не только делимая.
  S.splitRoom = (g, plugins, o = {}) => {
    const axis = S.AXIS[g.zone];
    const groupEl = (x) => document.querySelector(`.group[data-g="${x.id}"]`);
    const list = partsOf(S.layout(), g.zone);
    const els = list.map(groupEl);
    const at = list.indexOf(g);
    const body = at >= 0 && els[at] && els[at].querySelector('.panel-body');
    if (!body || els.some((e) => !e)) return { ok: true };
    const rect = (e) => e.getBoundingClientRect();
    const along = (e) => (axis === 'row' ? rect(e).width : rect(e).height);
    // Заглушку решает размер тела панели, поэтому из длины части вычитаются строка вкладок и рамки.
    const overhead = along(els[at]) - along(body);
    const weights = list.flatMap((x) => (x === g ? [x.w / 2, x.w / 2] : [x.w]));
    const total = weights.reduce((n, w) => n + w, 0) || 1;
    const avail = els.reduce((n, e) => n + along(e), 0) + 6 * (list.length - 1) - 6 * (weights.length - 1);
    const lens = weights.map((w) => Math.floor((avail * w) / total));
    lens[lens.length - 1] = avail - lens.slice(0, -1).reduce((n, v) => n + v, 0);
    const cross = Math.floor(axis === 'row' ? rect(body).height : rect(body).width);
    const parts = plugins.map((plugin, i) => ({ plugin, have: Math.floor(lens[at + i] - overhead), fresh: !!o.fresh && i === 1 }));
    const fits = (p) => { const m = minOf(p.plugin); return axis === 'row' ? m.w <= p.have && m.h <= cross : m.h <= p.have && m.w <= cross; };
    return { ok: parts.every(fits), axis, cross, parts };
  };
  // Отказ деления — одна строка в строке статуса, без уведомления (docs/ux.md, «Пограничные пути»).
  // Она называет каждую затронутую часть: сначала остающуюся, затем новую. Часть, панель которой ещё
  // не открыта, зовётся «новая часть». Следующее действие пользователя строку убирает.
  S.roomText = (room) => {
    const parts = room.parts.map((p) => { const m = minOf(p.plugin); return `${p.fresh ? 'новая часть' : `«${S.plugins[p.plugin].title}»`} — нужно ${m.w}×${m.h}, будет ${room.axis === 'row' ? `${p.have}×${room.cross}` : `${room.cross}×${p.have}`}`; });
    return parts.length ? `Не хватает места для новой части: ${parts.join('; ')}` : 'Не хватает места для новой части';
  };
  S.refuse = (text) => { St.statusMessage = text; S.render(); };
  const forget = () => { if (St.statusMessage) { St.statusMessage = null; S.render(); } };
  document.addEventListener('mousedown', forget, true);
  document.addEventListener('keydown', forget, true);
  function tight(g, room) { S.refuse(S.roomText(room)); }

  // Деление переносит активную вкладку в новую часть. Одну вкладку не переносят: рядом открывается выбор.
  S.split = (want) => {
    const L = S.layout(); const id = L.focus; const g = id && groupOf(L, id);
    if (!g) return;
    if (g.nav) { S.notify({ title: 'Навигатор не делится', text: 'Делится плитка панелей рядом с ним: перенесите панель влево, Ctrl-w H, и нажмите Ctrl-w s.' }); return; }
    const axis = S.AXIS[g.zone];
    if (want !== axis) { S.refuse(S.axisText(g.zone)); return; }
    if (g.tabs.length > 1) { S.moveTo(id, { split: g.id, side: 'after' }); return; }
    const room = S.splitRoom(g, [St.panels[id].plugin, 'terminal'], { fresh: true });
    if (!room.ok) { tight(g, room); return; }
    S.partPicker(g);
  };

  S.only = () => {
    const L = S.layout(); const id = L.focus; const g = groupOf(L, id);
    if (!g) return;
    L.hidden.left = true; L.hidden.right = true; L.hidden.bottom = true; L.hidden.plate = true;
    reveal(L, g, id);
    S.render();
  };

  S.equalize = () => {
    const L = S.layout();
    groups(L).forEach((g) => { g.w = 1; });
    L.size = { left: 236, right: 372, bottom: 236, plate: 372 }; S.render();
  };

  // Доли частей переводятся в пиксели, чтобы менять размер двух соседей и не трогать остальные.
  S.partPx = (L, zone) => {
    partsOf(L, zone).forEach((g) => {
      const el = document.querySelector(`.part[data-part="${g.id}"]`);
      if (el) g.w = S.AXIS[zone] === 'row' ? el.getBoundingClientRect().width : el.getBoundingClientRect().height;
    });
  };
  S.resizePart = (L, zone, i, a0, b0, delta) => {
    const list = partsOf(L, zone); const a = list[i - 1]; const b = list[i]; const total = a0 + b0; const min = 80;
    const na = S.clamp(a0 + delta, min, total - min);
    a.w = na; b.w = total - na;
  };

  S.nudge = (axis, delta) => {
    const L = S.layout(); const g = groupOf(L, L.focus); if (!g) return;
    const ax = S.AXIS[g.zone];
    if (!g.nav && L.parts[g.zone].length > 1 && ((ax === 'row' && axis === 'h') || (ax === 'stack' && axis === 'v'))) {
      S.partPx(L, g.zone);
      const list = L.parts[g.zone]; const i = list.indexOf(g.id);
      if (i === list.length - 1) S.resizePart(L, g.zone, i, L.groups[list[i - 1]].w, g.w, -delta);
      else S.resizePart(L, g.zone, i + 1, g.w, L.groups[list[i + 1]].w, delta);
      S.render(); return;
    }
    if (g.zone === 'center') { if (axis === 'h') L.size.right = S.clamp(L.size.right - delta, 240, 720); else L.size.bottom = S.clamp(L.size.bottom - delta, 120, 520); }
    else if (g.zone === 'bottom') L.size.bottom = S.clamp(L.size.bottom + (axis === 'v' ? delta : 0), 120, 520);
    else L.size[g.zone] = S.clamp(L.size[g.zone] + (axis === 'h' ? delta : 0), 200, 720);
    S.render();
  };

  S.zoom = () => { const L = S.layout(); L.zoom = L.zoom ? null : L.focus; S.render(); };
  // Третья кнопка зон и её клавиша управляют правой зоной, а если она пуста, панелями слева от центра.
  S.sideZone = (L = S.layout()) => (S.zoneTabs(L, 'left').length && !S.zoneTabs(L, 'right').length ? 'plate' : 'right');

  S.toggleZone = (zone) => {
    const L = S.layout();
    L.hidden[zone] = !L.hidden[zone];
    const g = L.focus && groupOf(L, L.focus);
    const inZone = g && (zone === 'plate' ? g.zone === 'left' && !g.nav : zone === 'left' ? !!g.nav : g.zone === zone);
    if (L.hidden[zone] && inZone) { const c = centerGroups(L).map((x) => x.active).find(Boolean); if (c) S.focus(c, { noHistory: true }); }
    S.render();
  };

  const tabsOf = (g) => g.tabs;
  S.tabStep = (d) => {
    const L = S.layout(); const g = L.focus && groupOf(L, L.focus); if (!g) return;
    const tabs = tabsOf(g); if (tabs.length < 2) return;
    const i = Math.max(0, tabs.indexOf(g.active));
    S.focus(tabs[(i + d + tabs.length) % tabs.length]);
  };
  S.tabAt = (n) => { const L = S.layout(); const g = L.focus && groupOf(L, L.focus); if (g && tabsOf(g)[n]) S.focus(tabsOf(g)[n]); };

  S.focusDir = (dir) => {
    const L = S.layout();
    const cur = document.querySelector(`.group[data-active="${L.focus}"]`);
    if (!cur) return;
    const a = cur.getBoundingClientRect();
    let best = null; let bestD = Infinity;
    document.querySelectorAll('.group[data-active]').forEach((el) => {
      if (el === cur) return;
      const b = el.getBoundingClientRect();
      const ok = dir === 'h' ? b.right <= a.left + 4 : dir === 'l' ? b.left >= a.right - 4 : dir === 'k' ? b.bottom <= a.top + 4 : b.top >= a.bottom - 4;
      if (!ok) return;
      const overlap = dir === 'h' || dir === 'l' ? Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) : Math.min(a.right, b.right) - Math.max(a.left, b.left);
      if (overlap <= 0) return;
      const d = dir === 'h' ? a.left - b.right : dir === 'l' ? b.left - a.right : dir === 'k' ? a.top - b.bottom : b.top - a.bottom;
      if (d < bestD) { bestD = d; best = el.dataset.active; }
    });
    if (best) S.focus(best);
  };

  S.isField = (el) => !!el && el.matches && el.matches('input,textarea');
  S.mode = () => {
    const a = document.activeElement;
    if (S.isField(a) && !a.closest('#overlays')) return 'input';
    const p = S.focused();
    if (!p) return 'nav';
    const def = S.plugins[p.plugin];
    // У панели с полем режим не хранится: «ВВОД» — только пока фокус в поле.
    if (def.fieldInput) return 'nav';
    return def.surfaceless && def.surfaceless(p) ? 'nav' : p.mode;
  };
  S.panelField = (p) => [...document.querySelectorAll(`.panel[data-panel="${p.id}"] .panel-body input, .panel[data-panel="${p.id}"] .panel-body textarea`)][0] || null;
  S.setMode = (m) => {
    const p = S.focused(); if (!p) return;
    if (S.plugins[p.plugin].fieldInput) {
      const field = S.panelField(p);
      if (m === 'input') { if (field) field.focus(); } else if (S.isField(document.activeElement)) document.activeElement.blur();
      S.render();
      return;
    }
    p.mode = m; S.render();
  };

  // Сводка ресурсов считается из единого снимка.
  S.totals = () => {
    const rows = Object.values(W.sessions).filter((s) => s.host !== 'finished');
    const cpu = rows.reduce((a, s) => a + s.cpu, W.resources.shared.cpu + W.resources.service.cpu);
    const mem = rows.reduce((a, s) => a + s.mem, W.resources.shared.mem + W.resources.service.mem);
    return { cpu: Math.round(cpu * 10) / 10, mem };
  };
  S.agents = (wb) => Object.values(W.sessions).filter((s) => s.kind === 'agent' && s.host !== 'finished' && (wb ? s.wb === wb : S.wsOf(s.wb) === St.route.ws));

  // Выбор оформления из трёх: светлое, тёмное или системное. Системный выбор берёт тему у системы.
  const systemTheme = () => (window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
  S.appearance = () => St.prefs.appearance || St.prefs.theme;
  S.setAppearance = (choice) => { St.prefs.appearance = choice; S.setTheme(choice === 'system' ? systemTheme() : choice); };
  S.setTheme = (t) => { St.prefs.theme = t; if (St.prefs.appearance !== 'system') St.prefs.appearance = t; document.documentElement.dataset.theme = t; S.render(); };
  S.setPref = (k, v) => {
    St.prefs[k] = v;
    if (k === 'contrast') { if (v) document.documentElement.dataset.contrast = 'more'; else delete document.documentElement.dataset.contrast; }
    if (k === 'motion') { if (v) document.documentElement.dataset.motion = 'reduced'; else delete document.documentElement.dataset.motion; }
    S.render();
  };
})();
