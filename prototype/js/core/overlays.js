// Пять типов наложений: палитра, which-key, поповер, sheet, composer. Плюс метки, уведомления и командная строка.
(() => {
  const St = S.st;
  const { icon, cx, esc } = S;
  let seq = 0;
  let toasts = [];

  S.ov = {
    push(o) {
      if (!St.overlays.length) { const a = document.activeElement; St.returnFocus = S.isField(a) && !a.closest('#overlays') ? a : null; }
      o.id = `ov${++seq}`; o.state = o.state || {}; St.overlays.push(o); S.clearPending(); S.render(); return o;
    },
    pop(o) {
      const i = St.overlays.indexOf(o || St.overlays[St.overlays.length - 1]);
      if (i >= 0) { const [x] = St.overlays.splice(i, 1); if (x.onClose) x.onClose(x); }
      if (S.isField(document.activeElement)) document.activeElement.blur();
      if (!St.overlays.length) { if (St.returnFocus && St.returnFocus.isConnected) St.returnFocus.focus(); St.returnFocus = null; }
      S.render();
    },
    has: (type) => St.overlays.find((o) => o.type === type),
  };

  function whichHtml() {
    const seqNow = St.pending;
    const map = new Map();
    S.bindings().forEach((b) => {
      if (b.hidden || b.seq.length <= seqNow.length || !seqNow.every((x, i) => x === b.seq[i])) return;
      const next = b.seq[seqNow.length];
      const deep = b.seq.length > seqNow.length + 1;
      const prefix = [...seqNow, next].join(' ');
      if (!map.has(next)) map.set(next, deep ? { group: true, title: S.keyGroups[prefix] || 'Ещё' } : { title: b.title });
    });
    const items = [...map.entries()];
    if (!items.length) return '';
    return `<div class="float wk" data-k="wk"><div class="wk-t">${K.kbd(seqNow.join(' '))}<span>${S.keyGroups[seqNow.join(' ')] || ''}</span></div><div class="wk-g">${items.map(([k, v]) => `<div class="${cx('wk-i', v.group && 'grp')}">${K.kbd(k)}<b>${v.title}</b></div>`).join('')}</div></div>`;
  }

  S.renderOverlays = () => {
    const root = document.getElementById('overlays');
    let html = St.overlays.map((o) => `<div data-ov="${o.id}" data-k="${o.id}">${o.dim ? '<div class="ov-dim" data-dim="1"></div>' : ''}${o.render(o)}</div>`).join('');
    if (St.whichShown && St.pending.length) html += whichHtml();
    S.morph(root, html);
    const picked = root.querySelector('.pi.on');
    if (picked && picked.dataset.i !== St.palShown) { St.palShown = picked.dataset.i; picked.scrollIntoView({ block: 'nearest' }); }
    St.overlays.forEach((o) => {
      if (o._focused) return;
      const box = root.querySelector(`[data-ov="${o.id}"]`);
      const f = box && box.querySelector('[data-autofocus]');
      if (f) { f.focus(); if (f.select && o.selectAll) f.select(); }
      else if (box && (o.type === 'sheet' || o.type === 'pop')) { const host = box.querySelector('.float'); if (host) host.focus({ preventScroll: true }); }
      o._focused = true;
    });
    St.overlays.forEach((o) => {
      if (o.type !== 'sheet' && o.type !== 'pop') return;
      const box = root.querySelector(`[data-ov="${o.id}"]`); if (!box) return;
      box.querySelectorAll('.sheet-b [data-do]:not(button), .pop-b [data-do]:not(button):not(.row)').forEach((el) => el.setAttribute('tabindex', '0'));
      if (o.type === 'pop') menuRows(box).forEach((r, i) => r.classList.toggle('kb', i === o.menuCur));
    });
  };

  const menuRows = (box) => Array.from(box.querySelectorAll('.pop-b .row[data-do], .pop-b .row[data-cmd]'));

  // Один обход для всех наложений: Tab идёт по кругу, Enter и Space нажимают то, что под фокусом.
  const trapKeys = (o, tok) => {
    const root = document.querySelector(`[data-ov="${o.id}"]`);
    if (!root) return false;
    if (tok === 'Tab' || tok === 'Shift-Tab') {
      const items = Array.from(root.querySelectorAll('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),[tabindex="0"]'));
      if (items.length) { const at = items.indexOf(document.activeElement); items[tok === 'Tab' ? (at + 1) % items.length : (at <= 0 ? items.length - 1 : at - 1)].focus(); }
      return true;
    }
    const a = document.activeElement;
    if ((tok === 'Enter' || tok === 'Space') && a && root.contains(a) && (a.tagName === 'BUTTON' || (!S.isField(a) && a.matches('[data-do],[data-cmd]')))) { a.click(); return true; }
    return false;
  };

  // Поток уведомлений у каждого воркспейса свой.
  // Действие живо, пока цель существует и осталась в воркспейсе уведомления.
  const liveToast = (t) => (!t.project || W.projects[t.project]) && (!t.wb || S.wsOf(t.wb) === t.ws) && (!t.split || t.parts.some((id) => W.projects[id] && W.projects[id].ws === t.ws));
  S.toasts = () => toasts.filter((t) => t.ws === St.route.ws && liveToast(t));
  // Уведомление с действием важнее нового сообщения без действия.
  S.shownToast = () => { const list = S.toasts(); return [...list].reverse().find((t) => t.action) || list[list.length - 1]; };
  const TOAST_TICK = 500;
  S.notify = (t) => {
    t.id = `toast${++seq}`;
    t.ws = t.ws || St.route.ws;
    toasts.push(t);
    S.render();
    const ttl = t.ttl || (t.action ? 8000 : 3800);
    const expire = () => {
      if (!toasts.includes(t)) return;
      // Действие не исчезает, пока его не показали целиком: считается только время на экране.
      if (t.action) {
        if (S.shownToast() === t) t.onScreen = (t.onScreen || 0) + TOAST_TICK;
        if ((t.onScreen || 0) < ttl) { setTimeout(expire, TOAST_TICK); return; }
      }
      toasts = toasts.filter((x) => x !== t); S.render();
    };
    if (t.ttl !== 0) setTimeout(expire, t.action ? TOAST_TICK : ttl);
    return t;
  };
  S.cmd({ id: 'toast.act', title: '', hidden: true, pal: false, run: (id) => { const t = toasts.find((x) => x.id === id); if (t) { toasts = toasts.filter((x) => x !== t); if (liveToast(t)) t.action.run(t); S.render(); } } });
  const withoutParts = (t, ids) => { t.parts = t.parts.filter((x) => !ids.includes(x)); t.title = t.split(t.parts.length); };
  S.moveToasts = (ids, ws) => {
    [...toasts].forEach((t) => {
      if (ids.includes(t.project) || (t.parts && t.parts.every((id) => ids.includes(id)))) { t.ws = ws; t.onScreen = 0; return; }
      // Массовое действие делится вместе с набором: каждая часть остаётся у своих проектов.
      const moved = t.split ? t.parts.filter((id) => ids.includes(id)) : [];
      if (!moved.length) return;
      withoutParts(t, moved);
      S.notify({ ...t, ws, parts: moved, title: t.split(moved.length), onScreen: 0 });
    });
  };
  S.dropToasts = (id) => {
    toasts.filter((t) => t.split && t.parts.length > 1 && t.parts.includes(id)).forEach((t) => withoutParts(t, [id]));
    toasts = toasts.filter((t) => t.project !== id && !(t.parts && t.parts.includes(id)));
  };

  S.cmd({ id: 'toast.list', title: '', hidden: true, pal: false, run: () => {
    S.popover(document.querySelector('.toast-n'), {
      title: 'Уведомления', width: 360,
      body: () => S.toasts().slice().reverse().map((t) => K.row({ k: t.id, title: t.title, sub: t.text, trail: t.action ? t.action.label : null, do: t.action ? 'act' : null, arg: t.id, quiet: !t.action })).join(''),
      on: { act: (id, o) => { S.ov.pop(o); S.run('toast.act', id); } },
    });
  } });

  document.addEventListener('mousedown', (e) => {
    const top = St.overlays[St.overlays.length - 1];
    if (!top) return;
    if (e.target.dataset && e.target.dataset.dim) { if (top.dismiss !== false) S.ov.pop(top); return; }
    if ((top.type === 'pop' || top.type === 'palette') && !e.target.closest('[data-ov]')) S.ov.pop(top);
  }, true);

  // Палитра
  const GROUPS = [['sess', 'Сессии'], ['prj', 'Проекты'], ['cmd', 'Команды'], ['proj', 'Рабочие области'], ['ws', 'Воркспейсы'], ['panel', 'Панели'], ['file', 'Файлы'], ['mr', 'Ревью'], ['demo', 'Сценарии']];
  const CATS = [['all', 'Всё'], ...GROUPS];
  const ZONE = { center: 'центр', left: 'слева', right: 'справа', bottom: 'низ' };
  const stateOf = (s) => (s.kind !== 'agent' && s.state === 'running' ? 'выполняется' : S.stateName(s));
  const shortTitle = (p) => { const def = S.plugins[p.plugin]; return p.state.sid && def.tabTitle ? def.tabTitle(p) : def.title; };

  function sessionItems() {
    const L = S.layout();
    const here = St.route.wb;
    const place = (s) => { const w = W.workbenches[s.wb]; const pr = W.projects[w.project]; return `${pr.title} / ${w.title}${pr.removedAt ? ' · убран из каталога' : ''}`; };
    const panels = Object.values(St.panels);
    const describe = (s) => {
      const p = S.panelOf(s);
      if (s.background || !p) return `в фоне · ${stateOf(s)}`;
      const zone = ZONE[(S.groupOf(S.layout(p.wb), p.id) || {}).zone] || 'центр';
      if (s.kind === 'agent') return `${s.profile} · ${stateOf(s)} · ${zone}`;
      if (s.kind === 'editor') return `${p.state.file.split('/').pop()} · ${zone}`;
      return `${s.cwd} · ${zone}`;
    };
    const live = Object.values(W.sessions).filter((s) => s.host !== 'finished' && S.wsOf(s.wb) === St.route.ws);
    const order = [...live.filter((s) => s.wb === here && !s.background), ...live.filter((s) => s.wb === here && s.background), ...live.filter((s) => s.wb !== here)];
    void L; void panels;
    return order.map((s) => ({ cat: 'sess', near: s.wb === here, icon: s.kind === 'agent' ? 'agent' : s.kind === 'editor' ? 'file' : 'term', title: s.title, sub: s.wb === here ? describe(s) : `${place(s)} · ${stateOf(s)}`, wait: s.state === 'waiting', run: () => S.openSession(s.id) }));
  }

  // Главные команды идут первыми и в этом порядке, остальные следуют за ними.
  function commandItems() {
    const L = S.layout();
    const nav = S.mode() === 'nav';
    const names = (zone) => S.zoneTabs(L, zone).map((id) => shortTitle(St.panels[id])).join(', ');
    const toggle = (hidden) => (hidden ? 'Показать' : 'Скрыть');
    const side = S.sideZone(L);
    const plateTabs = S.zoneTabs(L, 'left');
    const plateNames = plateTabs.map((id) => shortTitle(St.panels[id])).join(', ');
    const changes = S.panelsOf().find((p) => p.plugin === 'changes');
    const failed = S.wb() ? W.pipelines.find((x) => x.state === 'failed' && S.project().repos.includes(x.repo)) : null;
    const staged = W.changes.files.filter((f) => f.staged && !f.conflict).length;
    const pinned = [
      { id: 'mode.toggle', title: nav ? 'Режим «Ввод»' : 'Режим «Навигация»', sub: `клавиши ${nav ? 'идут в терминал' : 'управляют окном'}`, keys: S.modeKey() },
      { id: 'theme.toggle', title: St.prefs.theme === 'dark' ? 'Светлая тема' : 'Тёмная тема', sub: 'оформление', keys: S.keyFor('theme.toggle', 'Cmd-Alt-t') },
      { id: 'zone.left', title: `${toggle(L.hidden.left)} навигацию`, sub: 'зоны', keys: S.keyFor('zone.left', 'Cmd-0') },
      { id: 'zone.bottom', title: `${toggle(L.hidden.bottom)} нижнюю зону`, sub: names('bottom'), keys: S.keyFor('zone.bottom', 'Cmd-j') },
      { id: 'zone.right', title: `${toggle(L.hidden[side])} ${side === 'plate' ? 'левую' : 'правую'} зону`, sub: side === 'plate' ? plateNames : names('right'), keys: S.keyFor('zone.right', 'Cmd-Alt-0') },
    ];
    if (plateTabs.length && side === 'right') pinned.push({ title: `${toggle(L.hidden.plate)} панели слева`, sub: plateNames, run: () => S.toggleZone('plate') });
    if (changes) {
      const zone = S.groupOf(L, changes.id).zone === 'right' ? 'left' : 'right';
      pinned.push({ title: `Переместить «Изменения» в ${zone === 'left' ? 'левую' : 'правую'} зону`, sub: 'панель', run: () => { const f = S.focused(); S.move(changes.id, zone); if (f && f.id !== changes.id) S.focus(f.id, { noHistory: true }); } });
    }
    if (failed) pinned.push({ title: `Открыть пайплайн #${failed.id}`, sub: `упал на ${failed.failed}`, wait: true, run: () => S.run('pipelines.failed') });
    if (S.wb()) pinned.push({ title: 'Сгенерировать сообщение коммита', sub: `index: ${S.fmt.plural(staged, 'файл', 'файла', 'файлов')}`, run: () => { const p = S.open('changes', {}); S.plugins.changes.actions.gen.run(p); } });
    const ids = new Set(pinned.map((c) => c.id).filter(Boolean));
    const head = pinned.map((c) => ({ cat: 'cmd', icon: 'cmd', ...c, run: c.run || (() => S.run(c.id)) }));
    const rest = S.commands.filter((c) => c.pal !== false && !ids.has(c.id) && c.title && (!c.when || c.when()) && (!c.palWhen || c.palWhen())).map((c) => ({ cat: 'cmd', icon: 'cmd', title: c.title, sub: c.sub || '', keys: S.keysOf(c)[0], run: () => c.run() }));
    const p = S.focused();
    const own = [];
    if (p) { const def = S.plugins[p.plugin]; Object.values(def.actions || {}).forEach((a) => { if (!a.title || a.menu === false || (a.when && !a.when(p))) return; own.push({ cat: 'cmd', icon: 'cmd', title: a.title, sub: `в панели «${def.title}»`, keys: a.key ? [].concat(a.key)[0] : '', run: () => { a.run(p); S.render(); } }); }); }
    return [...head, ...own, ...rest];
  }

  function paletteItems() {
    const out = [...sessionItems(), ...commandItems()];
    const g = S.projectGroups(St.route.ws);
    const shown = [...g.listed, ...g.outside];
    const recent = new Set(shown.filter((p) => p.lastOpenedAt).sort((a, b) => b.lastOpenedAt - a.lastOpenedAt).slice(0, 5).map((p) => p.id));
    shown.forEach((p) => out.push({ cat: 'prj', near: recent.has(p.id), icon: 'folder', title: p.title, sub: `${S.fmt.plural(p.repos.length, 'репозиторий', 'репозитория', 'репозиториев')}${S.listed(p) ? '' : ', вне списка'}`, score: (q) => S.projScore(p, q), run: () => S.openProject(p.id) }));
    Object.values(W.workbenches).forEach((w) => { const p = W.projects[w.project]; if (p.ws !== St.route.ws || p.removedAt) return; out.push({ cat: 'proj', icon: 'branch', title: w.title, sub: `${p.title} / ${w.branch}`, run: () => S.go(w.id) }); });
    S.workspaces().filter((w) => w.id !== St.route.ws).forEach((w) => out.push({ cat: 'ws', near: false, icon: 'folder', title: w.title, sub: 'перейти в воркспейс', run: () => S.goWs(w.id) }));
    S.panelsOf().forEach((p) => { const def = S.plugins[p.plugin]; if (def.chromeless || p.state.sid) return; out.push({ cat: 'panel', icon: def.icon, title: def.tabTitle ? def.tabTitle(p) : def.title, sub: `панель · ${ZONE[S.groupOf(S.layout(), p.id).zone] || ''}`, run: () => S.focus(p.id) }); });
    if (S.repo()) W.allFiles().forEach((f) => out.push({ cat: 'file', icon: 'file', title: f, sub: `${S.repo().id}, ${S.wb().branch}`, run: () => S.openFile(f) }));
    W.mrList.filter((id) => S.repoWs(W.mrs[id].repo) === St.route.ws).forEach((id) => { const m = W.mrs[id]; out.push({ cat: 'mr', icon: 'review', title: `${m.id} ${m.title}`, sub: `${m.repo} · черновиков: ${m.drafts.length}`, run: () => S.open('review', { key: m.id, target: m.id }) }); });
    (S.scenarios || []).forEach((s) => out.push({ cat: 'demo', icon: 'scenes', title: s.title, sub: s.group, run: s.run }));
    return out;
  }

  const QUIET = new Set(['file', 'mr', 'demo']);

  S.palette = (cat = 'all') => {
    if (S.ov.has('palette')) { S.ov.pop(S.ov.has('palette')); return; }
    const all = paletteItems();
    const compute = (st) => {
      const q = (st.q || '').trim();
      const scope = all.filter((i) => (st.cat === 'all' ? q || (!QUIET.has(i.cat) && i.near !== false) : i.cat === st.cat));
      const scored = q ? scope.map((i) => ({ i, s: i.score ? i.score(q) : S.match(`${i.title} ${i.sub}`, q) })).filter((x) => x.s > 0) : scope.map((i, n) => ({ i, s: -n }));
      const groups = GROUPS.map(([id, title]) => ({ title, items: scored.filter((x) => x.i.cat === id).sort((a, b) => b.s - a.s).map((x) => x.i) })).filter((g) => g.items.length);
      if (q) groups.sort((a, b) => Math.max(...scored.filter((x) => x.i.cat === GROUPS.find(([, t]) => t === b.title)[0]).map((x) => x.s)) - Math.max(...scored.filter((x) => x.i.cat === GROUPS.find(([, t]) => t === a.title)[0]).map((x) => x.s)));
      let left = 60;
      const shown = groups.map((g) => { const items = g.items.slice(0, Math.max(0, left)); left -= items.length; return { ...g, items }; }).filter((g) => g.items.length);
      return { groups: shown, flat: shown.flatMap((g) => g.items) };
    };
    const o = {
      type: 'palette', state: { q: '', cat, i: 0 },
      on: { pick: (a, x) => { const it = compute(x.state).flat[+a]; S.ov.pop(x); if (it) it.run(); } },
      onBind: () => { o.state.i = 0; S.render(); },
      render: (x) => {
        const { groups, flat } = compute(x.state);
        let n = -1;
        const cat = CATS.find(([id]) => id === x.state.cat);
        return `<div class="float pal" role="dialog" aria-label="Переход и команды"><div class="pal-in">${icon('search', 16)}${x.state.cat !== 'all' ? `<span class="pal-cat">${cat[1]}</span>` : ''}<input data-autofocus data-bind="q" autocomplete="off" spellcheck="false" placeholder="Сессия, команда или действие…" value="${esc(x.state.q)}"><kbd>esc</kbd></div>
        <div class="pal-list">${flat.length ? groups.map((g) => `<div class="pg" data-k="g-${g.title}">${g.title}</div>${g.items.map((it) => { n++; return `<button type="button" class="${cx('pi', n === x.state.i && 'on')}" data-do="pick" data-arg="${n}" data-i="${n}" data-k="${esc(`${it.cat}-${it.title}`)}">${icon(it.icon, 14)}<span class="t">${it.title}</span><span class="${cx('u', it.wait && 'wait')}">${it.sub || ''}</span><span class="k">${it.keys ? K.kbd(K.keyChips(it.keys)) : ''}</span></button>`; }).join('')}`).join('') : '<div class="pal-empty">Ничего не найдено</div>'}</div>
        <div class="pal-foot"><span><kbd>↑</kbd><kbd>↓</kbd> выбрать</span><span><kbd>⏎</kbd> выполнить</span><span><kbd>esc</kbd> закрыть</span></div></div>`;
      },
      onKey: (e, tok) => {
        const list = compute(o.state).flat;
        if (tok === 'Esc' || tok === 'Cmd-k') { S.ov.pop(o); return true; }
        if (tok === 'Down' || tok === 'Ctrl-n' || tok === 'Ctrl-j') { o.state.i = Math.min(list.length - 1, o.state.i + 1); S.render(); return true; }
        if (tok === 'Up' || tok === 'Ctrl-p' || tok === 'Ctrl-k') { o.state.i = Math.max(0, o.state.i - 1); S.render(); return true; }
        if (tok === 'Tab' || tok === 'Shift-Tab') { const n = CATS.findIndex(([id]) => id === o.state.cat); o.state.cat = CATS[(n + (tok === 'Tab' ? 1 : CATS.length - 1)) % CATS.length][0]; o.state.i = 0; S.render(); return true; }
        if (tok === 'Enter') { const it = list[o.state.i]; S.ov.pop(o); if (it) it.run(); return true; }
        return 'native';
      },
    };
    St.palShown = null;
    S.ov.push(o);
  };

  document.addEventListener('mousemove', (e) => {
    const el = e.target.closest && e.target.closest('.pi[data-i]');
    const o = S.ov.has('palette');
    if (!el || !o || o.state.i === +el.dataset.i) return;
    o.state.i = +el.dataset.i; St.palShown = el.dataset.i; S.render();
  });

  // Метки перехода
  S.hintsOpen = () => {
    const els = Array.from(document.querySelectorAll('#app [data-do],#app [data-cmd],#app [data-tab]')).filter((el) => {
      if (el.parentElement.closest('.row[data-do],.row[data-cmd]')) return false;
      const r = el.getBoundingClientRect();
      if (r.width < 4 || r.height < 4 || r.bottom < 0 || r.top > innerHeight) return false;
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return hit && (el === hit || el.contains(hit) || hit.contains(el));
    }).slice(0, 240);
    const abc = 'asdfjklgh';
    const labels = [];
    const width = els.length <= abc.length ? 1 : els.length <= abc.length ** 2 ? 2 : 3;
    const spell = (n) => Array.from({ length: width }, (_, i) => abc[Math.floor(n / abc.length ** (width - 1 - i)) % abc.length]).join('');
    for (let n = 0; n < els.length; n++) labels.push(spell(n));
    const items = els.map((el, i) => { const r = el.getBoundingClientRect(); return { el, label: labels[i], x: Math.max(2, r.left + 2), y: Math.max(2, r.top + Math.min(4, r.height / 2 - 8)) }; });
    const o = {
      type: 'hints', state: { typed: '' },
      render: (x) => items.filter((it) => it.label.startsWith(x.state.typed)).map((it) => `<span class="hint" style="left:${it.x}px;top:${it.y}px" data-k="${it.label}"><em>${x.state.typed}</em>${it.label.slice(x.state.typed.length)}</span>`).join(''),
      onKey: (e, tok) => {
        if (tok === 'Esc' || tok === 'f') { if (!o.state.typed || tok === 'Esc') { S.ov.pop(o); return true; } }
        if (tok.length !== 1) return true;
        o.state.typed += tok;
        const hits = items.filter((it) => it.label.startsWith(o.state.typed));
        if (!hits.length) { S.ov.pop(o); return true; }
        if (hits.length === 1 && hits[0].label === o.state.typed) { S.ov.pop(o); hits[0].el.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })); hits[0].el.click(); return true; }
        S.render();
        return true;
      },
    };
    S.ov.push(o);
  };

  // Поповер, привязанный к источнику
  S.popover = (anchor, spec) => {
    S.ov.has('pop') && S.ov.pop(S.ov.has('pop'));
    const r = anchor ? anchor.getBoundingClientRect() : { left: innerWidth / 2, right: innerWidth / 2, bottom: 80, top: 80 };
    const w = spec.width || 320;
    const left = S.clamp(r.right - w > 8 && r.left + w > innerWidth - 8 ? r.right - w : r.left, 8, innerWidth - w - 8);
    // Поповер открывается туда, где больше места, и не выходит за окно: длинный список прокручивается.
    const roomBelow = innerHeight - r.bottom - 14; const roomAbove = r.top - 14;
    const below = roomBelow >= 360 || roomBelow >= roomAbove;
    const style = `left:${left}px;width:${w}px;max-height:${Math.round(below ? roomBelow : roomAbove)}px;${below ? `top:${r.bottom + 6}px` : `bottom:${innerHeight - r.top + 6}px`}`;
    const o = {
      type: 'pop', on: spec.on || {}, state: spec.state || {},
      render: (x) => `<div class="float pop" tabindex="-1" style="${style}"${spec.ck ? ` data-ck="${spec.ck}"` : ''}>${spec.title ? `<div class="pop-h"><span${spec.ck ? ` data-ck="${spec.ck}.title"` : ''}>${spec.title}</span>${spec.head ? spec.head : ''}</div>` : ''}<div class="pop-b">${spec.body(x)}</div></div>`,
      onKey: (e, tok) => {
        if (tok === 'Esc') { S.ov.pop(o); return true; }
        if (trapKeys(o, tok)) return true;
        if (spec.onKey && spec.onKey(e, tok, o)) return true;
        if (S.isField(document.activeElement)) return 'native';
        if (/^Cmd-/.test(tok)) return false;
        const box = document.querySelector(`[data-ov="${o.id}"]`);
        const rows = box ? menuRows(box) : [];
        if (rows.length && ['Down', 'j', 'Up', 'k'].includes(tok)) {
          const d = tok === 'Down' || tok === 'j' ? 1 : -1;
          o.menuCur = o.menuCur == null ? (d > 0 ? 0 : rows.length - 1) : (o.menuCur + d + rows.length) % rows.length;
          rows[o.menuCur].scrollIntoView({ block: 'nearest' });
          S.render();
          return true;
        }
        if ((tok === 'Enter' || tok === 'Space') && o.menuCur != null && rows[o.menuCur]) rows[o.menuCur].click();
        return true;
      },
    };
    return S.ov.push(o);
  };

  // Sheet: форма или подтверждение необратимого действия
  S.sheet = (spec) => {
    const on = { ...(spec.on || {}) };
    const fire = (a, o) => { if (a.close !== false) S.ov.pop(o); a.run && a.run(o); };
    (spec.actions || []).forEach((a, i) => { on[`__a${i}`] = (_, o) => fire(a, o); });
    const o = {
      type: 'sheet', dim: true, dismiss: false, on, state: spec.state || {}, onBind: spec.onBind,
      render: (x) => `<div class="float sheet" tabindex="-1" style="width:${spec.width || 460}px"><div class="sheet-h">${spec.icon ? icon(spec.icon, 18) : ''}<div class="sheet-t">${typeof spec.title === 'function' ? spec.title(x) : spec.title}</div></div>${spec.sub ? `<div class="sheet-sub">${spec.sub}</div>` : ''}<div class="sheet-b">${spec.body ? spec.body(x) : ''}</div><div class="sheet-f">${spec.note ? `<span class="note">${typeof spec.note === 'function' ? spec.note(x) : spec.note}</span>` : ''}${(spec.actions || []).filter((a) => !a.hide || !a.hide(x)).map((a) => K.btn(typeof a.label === 'function' ? a.label(x) : a.label, { kind: a.kind, do: `__a${spec.actions.indexOf(a)}`, key: a.key, disabled: a.disabled && a.disabled(x) })).join('')}</div></div>`,
      onKey: (e, tok) => {
        const field = S.isField(document.activeElement);
        if (tok === 'Esc') { if (spec.onEsc) spec.onEsc(o); else S.ov.pop(o); return true; }
        if (trapKeys(o, tok)) return true;
        const visible = (spec.actions || []).filter((a) => !a.hide || !a.hide(o));
        const primary = visible.find((a) => a.kind === 'primary');
        if (tok === 'Cmd-Enter' && primary) { if (!primary.disabled || !primary.disabled(o)) fire(primary, o); return true; }
        if (tok === 'Enter' && primary && !(field && document.activeElement.tagName === 'TEXTAREA')) { if (!primary.disabled || !primary.disabled(o)) fire(primary, o); return true; }
        if (field) return 'native';
        if (spec.onKey && spec.onKey(e, tok, o)) return true;
        const hit = visible.find((a) => a.key && a.key === tok && (!a.disabled || !a.disabled(o)));
        if (hit) { fire(hit, o); return true; }
        return true;
      },
    };
    return S.ov.push(o);
  };

  // Спросить и попросить: один composer у источника, контекст остаётся виден.
  const drafts = {};
  const draftKeys = (wb) => Object.keys(drafts).filter((k) => k.startsWith(`${wb}|`));
  S.composerDrafts = (wb) => draftKeys(wb).filter((k) => drafts[k].text.trim()).length;
  S.dropComposerDrafts = (wb) => draftKeys(wb).forEach((k) => delete drafts[k]);

  // Отправитель: вы сами или агент этой области. Текст от имени агента подписан, инициатор хранится отдельно.
  const senders = () => [{ id: 'you', title: 'Вы' }, ...S.agents(S.st.route.wb).filter((a) => a.host === 'ok').map((a) => ({ id: a.id, title: a.title }))];
  const recipients = (kind, from) => {
    const here = S.st.route.wb;
    const list = S.agents().filter((a) => a.id !== from).map((a) => ({ id: a.id, title: a.title, wb: a.wb, st: S.agentSt(a), note: [a.state === 'idle' ? 'свободен' : a.state === 'running' ? 'занят' : a.state === 'waiting' ? 'ждёт подтверждения' : '', a.wb !== here ? W.workbenches[a.wb].title : ''].filter(Boolean).join(', '), state: a.state, ok: true }));
    list.push({ id: 'zsh', title: 'zsh', st: 'exit', note: 'нет доставки', state: 'unsupported', ok: false });
    if (kind === 'task') list.push({ id: '__new', title: 'Новый агент', st: 'idle', note: 'отдельная копия', state: 'new', ok: true });
    return list;
  };

  S.composer = (kind, extra = {}) => {
    const reply = extra.reply || null;
    const draftKey = `${S.lkey()}|${reply ? reply.id || reply.thread || 'reply' : ''}`;
    const draft = drafts[draftKey] || (drafts[draftKey] = { text: '' });
    const cur = S.ov.has('composer');
    if (cur && !reply && !cur.reply) { cur.state.kind = kind; S.render(); return; }
    if (cur) S.ov.pop(cur);
    const p = S.focused();
    const def = p && S.plugins[p.plugin];
    const baseCtx = extra.ctx || (p && def.context ? def.context(p) : []);
    const ctx = [...baseCtx, { label: S.wb().branch, icon: 'branch' }].filter((c, i, all) => all.findIndex((x) => x.label === c.label) === i);
    const from0 = extra.from && senders().some((v) => v.id === extra.from) ? extra.from : 'you';
    const first = recipients(kind, from0);
    const firstOk = first.find((r) => r.id === (extra.to || S.st.lastRecipient) && r.ok) || first.find((r) => r.state === 'idle') || first.find((r) => r.ok);
    const el = p && document.querySelector(`.panel[data-panel="${p.id}"]`);
    const r = el ? el.getBoundingClientRect() : { left: 240, width: 800, bottom: innerHeight - 40 };
    const width = S.clamp(r.width - 20, 400, 560);
    const left = S.clamp(r.left + (r.width - width) / 2, 8, innerWidth - width - 8);
    const style = `left:${left}px;bottom:${Math.max(36, innerHeight - r.bottom + 10)}px;width:${width}px`;
    const rcOf = (x) => recipients(x.state.kind, x.state.from).find((v) => v.id === x.state.to);
    const senderOf = (x) => (x.state.from !== 'you' ? W.sessions[x.state.from] : null);
    const send = (o) => {
      const text = draft.text.trim();
      if (!text) return;
      const ctxLabels = o.state.ctx.map((c) => c.label);
      if (reply) {
        if (reply.send(text, ctxLabels) === false) return;
        draft.text = '';
        S.ov.pop(o);
        return;
      }
      const rc = rcOf(o);
      if (!rc || !rc.ok) return;
      const k = o.state.kind;
      const source = S.st.route.wb;
      draft.text = '';
      S.ov.pop(o);
      const fresh = rc.id === '__new' ? S.launchTaskAgent(text, ctxLabels) : null;
      if (!fresh) S.st.lastRecipient = rc.id;
      const first = text.split('\n')[0];
      const as = senderOf(o);
      const th = { id: S.uid('th'), kind: k, wb: source, title: first.length > 42 ? `${first.slice(0, 41)}…` : first, from: as ? as.id : undefined, fromGen: as ? as.gen : undefined, to: fresh ? fresh.id : rc.id, state: fresh ? 'accepted' : 'queued', when: 'Сейчас', ctx: ctxLabels, messages: [{ from: as ? `Вы от имени ${as.title}` : 'Вы', as: as ? as.id : undefined, initiator: 'Вы', text, state: fresh ? 'accepted' : 'queued', when: 'Сейчас' }] };
      W.threads.unshift(th);
      if (fresh) S.notify({ ...S.owner(fresh.wb), title: `Агент ${fresh.title} запущен`, text: `Отдельная копия ${fresh.cwd}. Фокус остаётся у вас.`, action: { label: 'Открыть агента', run: () => S.openSession(fresh.id) } });
      else S.notify({ ...(source ? S.threadToast(source, rc.wb) : {}), title: rc.state === 'idle' ? (k === 'ask' ? 'Вопрос отправлен' : 'Поручение отправлено') : 'Поставлено в очередь', text: `Получатель ${rc.title}${rc.wb !== source ? `, ${W.workbenches[rc.wb].title}` : ''}. Фокус остаётся у вас.`, action: { label: 'Открыть тред', run: () => S.open('threads', { wb: th.wb, thread: th.id }) } });
      S.simulateDelivery(th, fresh || rc);
    };
    const o = {
      type: 'composer', reply, state: { kind, from: from0, to: firstOk ? firstOk.id : null, ctx: ctx.slice() },
      on: {
        mode: (a, x) => { x.state.kind = a; if (!rcOf(x)) { const list = recipients(a, x.state.from); x.state.to = (list.find((v) => v.state === 'idle') || list.find((v) => v.ok)).id; } },
        from: (a, x) => { x.state.from = a; if (!rcOf(x)) { const list = recipients(x.state.kind, a); const next = list.find((v) => v.state === 'idle') || list.find((v) => v.ok); x.state.to = next ? next.id : null; } },
        to: (a, x) => { x.state.to = a; }, rmctx: (a, x) => { x.state.ctx.splice(+a, 1); }, send: (_, x) => send(x), close: (_, x) => S.ov.pop(x),
      },
      onBind: (b, v) => { draft.text = v; S.render(); },
      render: (x) => {
        const list = reply ? [] : recipients(x.state.kind, x.state.from);
        const from = reply ? [] : senders();
        const as = senderOf(x);
        const rc = reply ? { ok: true } : list.find((v) => v.id === x.state.to) || {};
        const isAsk = x.state.kind === 'ask';
        const failed = reply && reply.error ? reply.error() : '';
        const warn = as && rc.ok && rc.state !== 'new' ? `Сообщение уйдёт от имени ${as.title} с пометкой «Вы от имени». Ответ ${rc.title} получит ${as.title}, копия в треде.` : rc.state === 'running' ? `${rc.title} занят. Сообщение встанет в очередь и будет доставлено после текущей задачи.` : rc.state === 'waiting' ? `${rc.title} ждёт вашего подтверждения. Доставка после него.` : rc.state === 'new' ? 'Новый агент получит отдельную копию от последнего коммита. Незакоммиченные изменения в неё не попадут.' : '';
        const cta = reply ? 'Ответить' : rc.state === 'running' || rc.state === 'waiting' ? 'В очередь' : rc.state === 'new' ? 'Запустить' : 'Отправить';
        return `<div class="float composer" style="${style}">
          <div class="co-h">${reply ? '<b>Ответить в треде</b>' : K.seg([{ label: 'Спросить', on: isAsk, do: 'mode', arg: 'ask' }, { label: 'Попросить', on: !isAsk, do: 'mode', arg: 'task' }])}<span class="grow"></span>${K.iconBtn('close', { do: 'close' })}</div>
          ${!reply && from.length > 1 ? `<div class="co-rcp co-from"><span class="co-lab">От</span>${from.map((v) => `<button type="button" class="${cx('rcp', x.state.from === v.id && 'on')}" data-do="from" data-arg="${v.id}">${esc(v.title)}</button>`).join('')}</div>` : ''}
          ${reply ? K.ctx(esc(reply.to)) : `<div class="co-rcp">${from.length > 1 ? '<span class="co-lab">Кому</span>' : ''}${list.map((v) => `<button type="button" class="${cx('rcp', x.state.to === v.id && 'on')}" ${v.ok ? `data-do="to" data-arg="${v.id}"` : 'disabled'}>${K.status(v.st)}${v.title}${v.note ? `<span class="note">${v.note}</span>` : ''}</button>`).join('')}</div>`}
          ${warn ? `<div class="${cx('co-note', as && rc.ok && rc.state !== 'new' && 'info')}">${warn}</div>` : ''}
          ${x.state.ctx.length ? `<div class="co-chips">${x.state.ctx.map((c, i) => K.chip(c.label, { icon: c.icon || 'link', remove: 'rmctx', arg: i })).join('')}</div>` : ''}
          ${failed ? K.banner('err', 'Ответ не отправлен', esc(failed)) : ''}
          <div class="co-b"><textarea class="field" rows="${isAsk ? 3 : 4}" data-autofocus data-co="1" data-bind="text" placeholder="${reply ? 'Ваш ответ' : isAsk ? 'Что вы хотите узнать?' : 'Что нужно сделать и как понять, что готово?'}">${esc(draft.text)}</textarea></div>
          <div class="co-f"><span>${reply ? 'Ответ в текущем треде' : `${K.kbd('Tab')} Получатель`} &nbsp; ${K.kbd('Esc')} Черновик сохранится</span>${K.btn(cta, { kind: 'primary', do: 'send', key: '⌘↩', disabled: !rc.ok || !draft.text.trim() })}</div>
        </div>`;
      },
      onKey: (e, tok) => {
        if (tok === 'Esc') { S.ov.pop(o); return true; }
        if (reply && trapKeys(o, tok)) return true;
        if (tok === 'Tab' || tok === 'Shift-Tab') {
          const ok = recipients(o.state.kind, o.state.from).filter((v) => v.ok); const n = ok.findIndex((v) => v.id === o.state.to);
          o.state.to = ok[(n + (tok === 'Tab' ? 1 : ok.length - 1)) % ok.length].id; S.render(); return true;
        }
        if (tok === 'Cmd-Enter') { send(o); return true; }
        if (trapKeys(o, tok)) return true;
        if (S.isField(document.activeElement)) return 'native';
        return false;
      },
    };
    S.ov.push(o);
    setTimeout(() => { const t = document.querySelector('[data-co]'); if (t) t.setSelectionRange(t.value.length, t.value.length); }, 60);
  };

  S.simulateDelivery = (th, rc) => {
    const session = W.sessions[rc.id];
    const hit = (s, ms) => setTimeout(() => {
      th.state = s; th.messages[0].state = s;
      if (s === 'replied') {
        th.messages.push({ from: rc.title, text: 'Смотрю. Причина в том, что state удаляется до проверки, поэтому повторный callback получает 400 вместо редиректа. Предлагаю вернуть 303 на /login.', when: 'Сейчас' });
        S.notify({ ...S.threadToast(th.wb, rc.wb), title: `Ответ от ${rc.title}`, text: th.title, action: { label: 'Открыть тред', run: () => S.open('threads', { wb: th.wb, thread: th.id }) } });
      }
      S.render();
    }, ms);
    const delay = session && session.state === 'waiting' ? 4000 : 0;
    hit('delivered', 700 + delay); hit('accepted', 1400 + delay); hit('running', 2000 + delay); hit('replied', 5200 + delay);
  };

  // Командная строка
  S.cmdline = () => {
    const o = {
      type: 'cmdline', state: { v: '' },
      render: (x) => `<div class="cmdline">:<input data-autofocus data-bind="v" value="${esc(x.state.v)}" placeholder="Команда: zoom, split, theme light, q"></div>`,
      onKey: (e, tok) => {
        if (tok === 'Esc') { S.ov.pop(o); return true; }
        if (tok === 'Enter') {
          const v = (o.state.v || '').trim(); S.ov.pop(o); runCmdline(v); return true;
        }
        return 'native';
      },
    };
    S.ov.push(o);
  };
  function runCmdline(v) {
    if (!v) return;
    const [head, arg] = v.split(/\s+/);
    if (head === 'q' || head === 'close') { const p = S.focused(); if (p) S.close(p.id); return; }
    if (head === 'zoom') { S.zoom(); return; }
    if (head === 'vs' || head === 'vsplit') { S.split('row'); return; }
    if (head === 'sp' || head === 'split') { S.split('stack'); return; }
    if (head === 'theme') { S.setTheme(arg === 'light' ? 'light' : 'dark'); return; }
    if (head === 'go') { if (W.workbenches[arg] && S.wsOf(arg) === St.route.ws) S.openWb(arg); return; }
    const c = S.commands.filter((x) => x.pal !== false).map((x) => ({ x, s: S.match(`${x.title} ${x.id}`, v) })).filter((x) => x.s > 0).sort((a, b) => b.s - a.s)[0];
    if (c) { c.x.run(); return; }
    S.notify({ title: `Неизвестная команда: ${esc(v)}`, text: 'Откройте поиск ⌘K, чтобы найти нужное действие.', tone: 'wait' });
  }

  // Шпаргалка
  S.help = () => {
    const p = S.focused();
    const def = p && S.plugins[p.plugin];
    const local = p ? Object.values(def.actions || {}).filter((a) => a.key && a.title && (!a.when || a.when(p))) : [];
    const groups = {};
    S.commands.filter((c) => !c.hidden && c.pal !== false && S.keysOf(c).length).forEach((c) => { (groups[c.grp || 'Общие'] = groups[c.grp || 'Общие'] || []).push(c); });
    // Аккорд показывается только для направлений, где он ещё назначен.
    const chord = (ids, prefix) => { const tail = ids.map((id) => S.keysOf(S.commands.find((c) => c.id === id)).find((k) => k.startsWith(prefix))).filter(Boolean).map((k) => k.slice(prefix.length)); return tail.length ? `${prefix}${tail.join('/')}` : ''; };
    const dirs = ['h', 'j', 'k', 'l'];
    const nums = Array.from({ length: 9 }, (_, i) => S.keyFor(`tab.${i + 1}`, String(i + 1)));
    const block = (title, all) => { const items = all.filter(([k]) => k); return items.length ? `<div class="help-s"><h3>${title}</h3>${items.map(([k, v]) => `<div class="help-i"><span>${v}</span><span>${K.kbd(k)}</span></div>`).join('')}</div>` : ''; };
    S.sheet({
      icon: 'keys', title: 'Клавиши', width: 720, sub: `Режим «Навигация»: клавиши управляют приложением. Режим «Ввод»: клавиши получает программа, выход по ${S.modeKey()}. Всё настраивается в разделе «Клавиши».`,
      body: () => `<div class="help-g">${local.length ? block(`Панель «${def.title}»`, local.map((a) => [[].concat(a.key)[0], a.title])) : ''}${block('Поиск', [[S.keyFor('palette', 'Cmd-k'), 'Сессии и команды'], ['Tab', 'Сменить раздел в поиске']])}${block('Движение', [[S.navKeys(), 'Фокус, если панель не занимает клавиши'], [chord(dirs.map((d) => `focus.${d}`), 'Ctrl-w '), 'Фокус между панелями'], [chord(dirs.map((d) => `move.${d}`), 'Ctrl-w '), 'Перенести панель'], [[S.keyFor('tab.prev', 'H'), S.keyFor('tab.next', 'L')].filter(Boolean).join(' '), 'Вкладки'], [nums.every((k, i) => k === String(i + 1)) ? '1..9' : nums.filter(Boolean).join(' '), 'Вкладка по номеру'], ['Cmd-1..9', 'Воркспейс по номеру'], [S.keyFor('back', 'Ctrl-o'), 'Назад'], [S.keyFor('hints', 'f'), 'Метки'], [S.keyFor('filter', '/'), 'Фильтр'], [S.keyFor('cmdline', ':'), 'Команда']])}${Object.entries(groups).map(([g, list]) => block(g, list.map((c) => [S.keysOf(c)[0], c.title]))).join('')}</div>`,
      actions: [{ label: 'Закрыть', kind: 'primary', key: 'Enter' }],
    });
  };

  S.scenarios = [];
  S.scenario = (s) => S.scenarios.push(s);

  S.welcome = () => {
    const o = {
      type: 'welcome', dim: true, on: { go: (_, x) => S.ov.pop(x) },
      render: () => `<div class="float welcome"><h1>Прототип shellio</h1><p>Всё живое: вкладки, зоны, режимы, оверлеи. Попробуйте по очереди:</p><div class="try">
        <span>${K.kbd('Ctrl-\\')}</span><span>Переключает режимы Ввод и Навигация</span>
        <span>${K.kbd('Space')}</span><span>Подсказка всех команд</span>
        <span>${K.kbd('⌘K')}</span><span>Поиск проектов, панелей, файлов и команд</span>
        <span>${K.kbd('⌘1')} ${K.kbd('⌘2')}</span><span>Воркспейсы. Первая часть заголовка открывает их список</span>
        <span>${K.kbd('f')}</span><span>Метки: перейти к любой кнопке без мыши</span>
        <span>${K.kbd('Ctrl-w')} ${K.kbd('h j k l')}</span><span>Фокус между панелями, заглавные переносят панель в зону</span>
        <span>${K.kbd('Space q')}</span><span>Спросить агента, контекст подставится сам</span>
        <span>${K.kbd('⌘K')} Сценарии</span><span>Вызывают любое состояние: потерю хоста, смену head, обновление</span>
      </div><div style="display:flex;justify-content:flex-end">${K.btn('Начать', { kind: 'primary', do: 'go', key: 'Enter' })}</div></div>`,
      onKey: (e, tok) => { if (tok === 'Enter' || tok === 'Esc') { S.ov.pop(o); return true; } return true; },
    };
    S.ov.push(o);
  };
})();
