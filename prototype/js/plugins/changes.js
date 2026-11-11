// Изменения и коммит. Эталон контракта плагина: одна модель и разные представления по зонам.
(() => {
  const C = W.changes;
  const AI = 'feat(auth): вход через SSO\n\nСессия создаётся только после проверки токена,\nсрок действия ограничен SESSION_TTL.';
  const timers = new Map();
  const stopGen = (own) => { clearInterval(timers.get(own)); timers.delete(own); };

  const hunksOf = (f) => f.hunks || W.diffs[f.path] || [];
  const stats = (f) => { let a = 0; let d = 0; hunksOf(f).forEach((h) => h.lines.forEach((l) => { if (l.t === '+') a++; else if (l.t === '-') d++; })); return { a, d }; };
  const order = () => [...C.files.filter((f) => f.conflict), ...C.files.filter((f) => !f.conflict && f.staged), ...C.files.filter((f) => !f.conflict && !f.staged)];
  const stagedList = () => C.files.filter((f) => f.staged && !f.conflict);
  const name = (path) => path.split('/').pop();
  const dir = (path) => path.split('/').slice(0, -1).join('/');
  // Выбор держится за путь: файл не подменяется, когда перемещается между группами.
  const fileAt = (p) => {
    const all = order(); if (!all.length) return null;
    let i = all.findIndex((f) => f.path === p.state.path);
    if (i < 0) i = S.clamp(p.state.cur, 0, all.length - 1);
    p.state.cur = i; p.state.path = all[i].path;
    return all[i];
  };
  const pick = (p, i) => { const all = order(); const f = all[S.clamp(i, 0, Math.max(0, all.length - 1))]; if (f) { p.state.cur = all.indexOf(f); p.state.path = f.path; } };

  function indexChanged() {
    C.indexRev++;
    if (C.msg.state === 'fresh' && C.msg.indexAt !== C.indexRev) C.msg.state = 'stale';
  }

  function toggle(path) {
    const f = C.files.find((x) => x.path === path);
    if (!f || f.conflict) return;
    f.staged = !(f.staged === true && !f.partial);
    f.partial = false;
    delete f.lines;
    indexChanged();
  }

  // Кнопка у файла всегда выполняет то, что на ней написано, в отличие от x, который доводит частичный выбор до полного.
  function setStaged(path, on) {
    const f = C.files.find((x) => x.path === path);
    if (!f || f.conflict) return;
    f.staged = on;
    f.partial = false;
    delete f.lines;
    indexChanged();
  }

  function stageLines(f, a, b) {
    const changed = K.flat({ hunks: hunksOf(f) }).map((l, i) => (l.t === ' ' ? -1 : i)).filter((i) => i >= 0);
    if (!f.lines) { f.lines = {}; if (f.staged === true) changed.forEach((i) => { f.lines[i] = true; }); }
    const picked = changed.filter((i) => i >= a && i <= b);
    if (!picked.length) return;
    const on = !picked.every((i) => f.lines[i]);
    picked.forEach((i) => { f.lines[i] = on; });
    const n = changed.filter((i) => f.lines[i]).length;
    f.staged = n > 0;
    f.partial = n > 0 && n < changed.length;
    if (!f.partial) delete f.lines;
    indexChanged();
  }

  // Выбранные строки вошли в HEAD: добавленные стали контекстом, удалённые исчезли, остальное осталось в рабочей копии.
  function remainder(f) {
    let at = -1; let shift = 0;
    return hunksOf(f).map((h) => {
      const m = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,\d+)? @@(.*)$/.exec(h.header);
      const start = +m[1] + shift;
      let old = start;
      const lines = [];
      h.lines.forEach((l) => {
        at++;
        if (l.t === ' ') lines.push({ ...l, o: old++ });
        else if (!f.lines[at]) lines.push(l.t === '-' ? { ...l, o: old++ } : l);
        else if (l.t === '+') lines.push({ ...l, t: ' ', o: old++ });
      });
      const count = old - start;
      shift += count - h.lines.filter((l) => l.t !== '+').length;
      return { ...h, header: `@@ -${start},${count} +${m[3]},${lines.filter((l) => l.t !== '-').length} @@${m[4]}`, lines };
    }).filter((h) => h.lines.some((l) => l.t !== ' '));
  }

  function generate(p) {
    if (!stagedList().length) { S.notify({ title: 'Index пуст', text: 'Добавьте файлы в index, затем сгенерируйте сообщение.', tone: 'wait' }); return; }
    const own = W.changesOf(p.wb);
    if (own.msg.state === 'streaming') return;
    stopGen(own);
    const prev = own.msg.state === 'manual' && own.msg.text.trim() ? own.msg.text : null;
    const startWs = S.st.route.ws; const startProject = S.project() && S.project().id;
    const msg = { text: '', state: 'streaming', indexAt: own.indexRev, before: own.msg.text.trim() ? own.msg : null };
    own.msg = msg;
    let i = 0;
    timers.set(own, setInterval(() => {
      if (own.msg !== msg || msg.state !== 'streaming') return;
      i += 3;
      own.msg.text = AI.slice(0, i);
      if (i >= AI.length) {
        stopGen(own);
        own.msg.state = own.msg.indexAt === own.indexRev ? 'fresh' : 'stale';
        if (prev) S.notify({ ws: startWs, project: startProject, title: 'Сообщение заменено', text: 'Ваш текст можно вернуть.', action: { label: 'Вернуть мой текст', run: () => { own.msg = { text: prev, state: 'manual', indexAt: own.indexRev }; S.render(); } } });
      }
      S.render();
    }, 45));
  }

  function cancelOwn(own) {
    stopGen(own);
    const back = own.msg.state === 'streaming' && own.msg.before;
    if (back) own.msg = back; else own.msg.state = own.msg.text ? 'manual' : 'empty';
    S.render();
  }
  function cancelGen(p) { cancelOwn(W.changesOf(p.wb)); }
  S.genCancel = (wb) => cancelOwn(W.changesOf(wb));

  function commit(p) {
    const list = stagedList();
    if (C.conflict || C.msg.state === 'streaming') return;
    if (!list.length) { S.notify({ title: 'Index пуст', text: 'В коммит нечего включить.', tone: 'wait' }); return; }
    if (!C.msg.text.trim()) { S.notify({ title: 'Сначала введите или сгенерируйте сообщение коммита' }); return; }
    if (C.hookFails) { C.hookError = true; S.render(); return; }
    const sha = 'c8d42f6';
    const extra = C.addedAfterCheck;
    const rest = [];
    C.files.forEach((f) => {
      if (!f.staged) { rest.push(f); return; }
      if (!f.partial) return;
      const hunks = remainder(f);
      if (hunks.length) { const left = { ...f, staged: false, partial: false, hunks }; delete left.lines; rest.push(left); }
    });
    C.files = rest;
    C.head = sha;
    C.lastCommit = { sha, title: C.msg.text.split('\n')[0], count: list.length, extra };
    C.msg = { text: '', state: 'empty', indexAt: C.indexRev };
    C.addedAfterCheck = null; C.hookError = false;
    if (!C.files.some((f) => f.path === p.state.path)) { p.state.area = 'list'; p.state.line = 0; p.state.anchor = null; }
    S.render();
  }

  const fileRow = (p, f, i, focused, showSel) => {
    const st = stats(f);
    const here = i === p.state.cur;
    const status = f.conflict ? 'U' : f.status;
    const parts = [];
    if (f.by) parts.push(`<span class="tag" title="Файл создан агентом ${f.by}">новый · ${S.icon('agent', 11)}агент</span>`);
    else if (dir(f.path)) parts.push(`<span class="dir">${dir(f.path)}</span>`);
    if (f.partial) parts.push('<span class="tag">часть в index</span>');
    return K.frow({
      k: f.path, on: showSel && here, kb: focused && here && !!S.st.kbNav, status, do: 'openFile', arg: i,
      name: f.status === 'R' ? `${name(f.from)} <span style="color:var(--text-3)">→</span> ${name(f.path)}` : name(f.path),
      note: parts.join(''), a: f.status === 'B' ? 0 : st.a, d: f.status === 'B' ? 0 : st.d, flag: f.status === 'R' ? `R ${f.sim}%` : f.status === 'B' ? 'bin' : '',
      act: f.conflict ? null : { do: f.staged ? 'unstageFile' : 'stageFile', arg: f.path, icon: f.staged ? 'minus' : 'plus', label: f.staged ? 'Убрать из index' : 'Добавить в index' },
    });
  };

  function list(p, focused, showSel = false) {
    fileAt(p);
    let i = -1;
    const of = (arr) => arr.map((f) => { i++; return fileRow(p, f, i, focused, showSel); }).join('');
    const conflicts = C.files.filter((f) => f.conflict);
    const staged = C.files.filter((f) => !f.conflict && f.staged);
    const rest = C.files.filter((f) => !f.conflict && !f.staged);
    return `${conflicts.length ? `${K.grp('Конфликты', conflicts.length)}${of(conflicts)}` : ''}${K.grp('В index', staged.length, staged.length ? K.lnk('Убрать всё', { do: 'unstageAll' }) : '')}${of(staged) || '<div class="row-s" style="padding:4px 6px 8px">Нажмите x на файле, чтобы добавить его.</div>'}${K.grp('Рабочая копия', rest.length, rest.length ? K.lnk('Добавить всё', { do: 'stageAll' }) : '')}${of(rest)}`;
  }

  function commitBox(p) {
    const m = C.msg;
    const n = stagedList().length;
    const banners = [];
    if (m.state === 'stale') banners.push(K.chip('Сообщение устарело: index изменился', { tone: 'wait', icon: 'warn' }));
    if (C.hookError) banners.push(`<div style="border:1px solid var(--err);border-radius:6px;background:var(--err-tint);padding:6px 8px;font-size:var(--t-sm)"><b>pre-commit завершился с кодом 1</b><div class="mono" style="color:var(--text-2);white-space:pre-wrap;margin-top:3px">test sso::reject_expired_state ... FAILED\nleft: 500, right: 401</div><div style="color:var(--text-2);margin-top:3px">Сообщение и выбор сохранены. Исправьте и повторите.</div></div>`);
    const label = C.detached ? 'Создать коммит без ветки' : C.conflict ? 'Завершить слияние' : C.hookError ? 'Повторить коммит' : 'Создать коммит';
    const disabled = !n || m.state === 'streaming' || C.conflict;
    return `<div class="commit">
      ${banners.join('')}
      ${K.field({ bind: 'msg', value: m.text, placeholder: 'Сообщение коммита', rows: 3 })}
      <div class="commit-row">
        ${m.state === 'streaming' ? K.btn('Отменить', { do: 'cancelGen', key: 'Esc' }) : K.btn(m.state === 'stale' ? 'Сгенерировать снова' : 'Сгенерировать', { do: 'gen', icon: 'agent' })}
        ${K.btn(label, { do: 'commit', kind: 'primary grow', key: 'Cmd-Enter', disabled })}
      </div>
      <div class="commit-hint">${n ? `В коммит войдёт весь index: ${S.fmt.plural(n, 'файл', 'файла', 'файлов')}` : 'Index пуст'}${C.detached ? ', коммит не попадёт ни в одну ветку' : ''}</div>
    </div>`;
  }

  function topBanners() {
    let h = '';
    if (C.detached) h += K.banner('wait', `Detached HEAD на ${C.head}`, 'Новый коммит не попадёт ни в одну ветку.', K.btn('Создать ветку', { do: 'mkBranch', small: true }));
    if (C.conflict) h += K.banner('err', 'Слияние main приостановлено', 'Разрешите конфликт и добавьте файл в index.', K.btn('Открыть в nvim', { do: 'gf', small: true, key: 'g f' }));
    if (C.lastCommit) h += K.banner(C.lastCommit.extra ? 'wait' : 'ok', C.lastCommit.extra ? `В коммит ${C.lastCommit.sha} попал дополнительный файл` : `Коммит ${C.lastCommit.sha} создан`, C.lastCommit.extra ? `${C.lastCommit.extra} добавлен между проверкой и коммитом. Коммит не изменён.` : C.lastCommit.title, K.btn('Закрыть', { do: 'dismiss', small: true, kind: 'flat' }));
    return h;
  }

  function diffOf(p, geo) {
    const f = fileAt(p);
    if (!f) return K.empty({ icon: 'changes', title: 'Выберите файл' });
    if (f.status === 'B') return K.stack([K.ctx(K.crumbs(f.path.split('/')), ''), K.empty({ icon: 'file', title: 'Бинарный файл', text: `${f.sizes}. Показать diff нельзя, построчный выбор недоступен. Файл добавляется в index только целиком.`, actions: K.btn(f.staged ? 'Убрать из index' : 'Добавить в index', { do: 'toggleCur', key: 'x' }) })]);
    const data = { path: f.path, hunks: hunksOf(f) };
    const mode = p.state.side && geo.width >= 780 ? 'split' : 'unified';
    p.state.mode = mode;
    const focusDiff = p.state.area === 'diff' && S.focused() === p;
    const flat = K.flat(data);
    const sel = p.state.anchor != null ? [Math.min(p.state.anchor, p.state.line), Math.max(p.state.anchor, p.state.line)] : null;
    const staged = mode === 'unified' && !f.conflict ? (i) => { if (flat[i].t === ' ') return false; return f.lines ? !!f.lines[i] : f.staged === true; } : null;
    const st = stats(f);
    return `${K.ctx(`${K.crumbs(geo.width < 420 ? [name(f.path)] : f.path.split('/'))}<span style="color:var(--ok);font-size:var(--t-sm)">+${st.a}</span><span style="color:var(--err);font-size:var(--t-sm)">−${st.d}</span><span style="color:var(--text-3);font-size:var(--t-sm);white-space:nowrap">${f.partial ? 'часть в index' : f.staged === true ? 'HEAD → index' : 'index → рабочая копия'}</span>`, `${sel ? K.chip(`Выделено ${sel[1] - sel[0] + 1}`, { tone: 'info' }) : ''}${geo.width >= 780 ? K.seg([{ label: 'Рядом', on: mode === 'split', do: 'setSide', arg: 1 }, { label: 'В одну', on: mode !== 'split', do: 'setSide', arg: 0 }]) : ''}${K.btn('nvim', { do: 'gf', small: true, kind: 'flat', key: 'g f' })}`)}${K.scroll(K.diff(data, { mode, cursor: focusDiff ? p.state.line : null, sel, staged: f.conflict ? null : staged, onLine: 'lineClick' }))}`;
  }

  function emptyState() {
    return K.empty({ icon: 'check', title: 'Нет изменений', text: 'Рабочая копия совпадает с HEAD.', actions: `${K.btn('Открыть терминал', { cmd: 'terminal.new', key: 'Space t' })}${K.btn('Попросить агента', { cmd: 'task', key: 'Space d' })}` });
  }

  const head = () => `<div class="chg-head">${S.icon('branch', 14)}<b>${S.repo().id}</b><span>${C.detached ? 'detached HEAD' : S.wb().branch}</span></div>`;
  const listBox = (p, focused, showSel) => `<div class="chg-list scroll x-hidden">${list(p, focused, showSel)}</div>`;

  // Узкая зона: список и сообщение, diff раскрывается вглубь.
  function narrow(p, geo) {
    const focused = S.focused() === p;
    if (p.state.area === 'diff') {
      return `<div class="ctx"><div class="ctx-l">${K.btn('', { icon: 'chevL', kind: 'flat', do: 'back', tip: 'К файлам' })}<span style="color:var(--text-2)">К файлам</span></div></div>${diffOf(p, geo)}`;
    }
    if (!C.files.length) return `${topBanners()}${emptyState()}`;
    return `<div class="chg">${head()}${topBanners()}${listBox(p, focused)}${commitBox(p)}</div>`;
  }

  function wide(p, geo) {
    const focused = S.focused() === p && p.state.area !== 'diff';
    if (!C.files.length) return `${topBanners()}${emptyState()}`;
    return `${topBanners()}${K.cols([{ w: '300px', html: `<div class="chg">${head()}${listBox(p, focused, true)}${commitBox(p)}</div>` }, { html: diffOf(p, { ...geo, width: geo.width - 301 }) }])}`;
  }

  function bottom(p, geo) {
    if (geo.width < 620) return narrow(p, geo);
    const focused = S.focused() === p && p.state.area !== 'diff';
    if (!C.files.length) return emptyState();
    return K.cols([{ w: '250px', html: listBox(p, focused, true) }, { html: diffOf(p, { ...geo, width: geo.width - 251 - (geo.width > 900 ? 261 : 0) }) }, geo.width > 900 ? { w: '260px', html: `<div class="grow"></div>${commitBox(p)}` } : null]);
  }

  const inDiff = (p) => p.state.area === 'diff';
  const maxLine = (p) => { const f = fileAt(p); return f ? K.flat({ hunks: hunksOf(f) }).length - 1 : 0; };
  // В режиме «Рядом» курсор идёт по строкам экрана, а не по строкам патча.
  const stepLine = (p, d) => {
    const f = fileAt(p); if (!f) return;
    if (p.state.mode !== 'split') { p.state.line = S.clamp(p.state.line + d, 0, maxLine(p)); return; }
    const keys = K.splitKeys({ hunks: hunksOf(f) });
    let at = keys.indexOf(p.state.line);
    if (at < 0) at = Math.max(0, keys.findIndex((k) => k >= p.state.line));
    p.state.line = keys[S.clamp(at + d, 0, keys.length - 1)] ?? 0;
  };
  const stepFile = (p, d) => { fileAt(p); pick(p, p.state.cur + d); };

  S.plugin({
    id: 'changes', title: 'Изменения', icon: 'changes', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'right', cardinality: 'workbench', minSize: { w: 230, h: 110 },
    init(p) { Object.assign(p.state, { cur: 0, area: 'list', line: 0, anchor: null, side: true, msg: '' }); },
    tabStatus: (p) => (W.changesOf(p.wb).conflict ? 'err' : null),
    tabCount: (p) => W.changesOf(p.wb).files.length,
    meter: () => ({ kind: 'estimate', text: '≈ 0,1% CPU · 4,2 МБ' }),
    location: (p) => { const f = fileAt(p); return f ? { file: f.path, line: 1 } : {}; },
    context: (p) => { const f = fileAt(p); if (!f) return []; if (p.state.anchor != null) return [{ label: `${name(f.path)}: выделение`, icon: 'file' }]; return [{ label: `Diff ${name(f.path)}`, icon: 'changes' }]; },
    onBind: (p, name2, v) => { if (name2 !== 'msg') return; const own = W.changesOf(p.wb); stopGen(own); own.msg.text = v; own.msg.state = v ? 'manual' : 'empty'; },
    view: (p, geo) => K.adapt(geo, {
      left: () => narrow(p, geo),
      right: () => narrow(p, geo),
      center: () => (geo.width < 620 ? narrow(p, geo) : wide(p, geo)),
      bottom: () => bottom(p, geo),
    }),
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => { if (inDiff(p)) stepLine(p, 1); else stepFile(p, 1); S.render(); } },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => { if (inDiff(p)) stepLine(p, -1); else stepFile(p, -1); S.render(); } },
      toggleCur: { title: 'В index или из него', key: 'x', hint: 'В index', run: (p) => {
        const f = fileAt(p); if (!f) return;
        if (inDiff(p) && p.state.anchor != null) {
          stageLines(f, Math.min(p.state.anchor, p.state.line), Math.max(p.state.anchor, p.state.line));
          p.state.anchor = null;
        } else toggle(f.path);
        S.render();
      } },
      openFile: { title: 'Открыть diff', key: ['Enter', 'l'], hint: 'Diff', run: (p, i) => { if (i != null) pick(p, +i); if (!C.files.length) return; p.state.area = 'diff'; p.state.line = 0; p.state.anchor = null; S.mark(p, () => { p.state.area = 'list'; }); S.render(); } },
      back: { title: 'К файлам', key: 'h', when: inDiff, menu: false, run: (p) => { p.state.area = 'list'; p.state.anchor = null; S.render(); } },
      select: { title: 'Выделить строки', key: 'v', when: inDiff, hint: 'Выделить', run: (p) => { if (p.state.mode === 'split') p.state.side = false; p.state.anchor = p.state.anchor == null ? p.state.line : null; S.render(); } },
      toggle: { title: '', menu: false, run: (p, path) => { toggle(path); S.render(); } },
      stageFile: { title: '', menu: false, run: (p, path) => { setStaged(path, true); S.render(); } },
      unstageFile: { title: '', menu: false, run: (p, path) => { setStaged(path, false); S.render(); } },
      stageAll: { title: 'Добавить всё в index', key: 'A', when: () => C.files.some((f) => !f.conflict && !f.staged), run: () => { C.files.forEach((f) => { if (f.conflict) return; f.staged = true; f.partial = false; delete f.lines; }); indexChanged(); S.render(); } },
      unstageAll: { title: 'Убрать всё из index', key: 'U', when: () => C.files.some((f) => !f.conflict && f.staged), run: () => { C.files.forEach((f) => { if (f.conflict) return; f.staged = false; f.partial = false; delete f.lines; }); indexChanged(); S.render(); } },
      gen: { title: 'Сгенерировать сообщение', key: 'a', hint: 'Сообщение', when: (p) => W.changesOf(p.wb).msg.state !== 'streaming', run: generate },
      cancelGen: { title: 'Отменить генерацию', key: 'Esc', when: (p) => W.changesOf(p.wb).msg.state === 'streaming', run: cancelGen },
      commit: { title: 'Создать коммит', key: 'Cmd-Enter', run: commit },
      gf: { title: 'Открыть в nvim', key: 'g f', run: (p) => { const f = fileAt(p); if (f) S.openFile(f.path); } },
      setSide: { title: '', menu: false, run: (p, a) => { p.state.side = a === '1'; S.render(); } },
      lineClick: { title: '', menu: false, run: (p, i) => { p.state.area = 'diff'; p.state.line = +i; S.render(); } },
      mkBranch: { title: 'Создать ветку', when: () => C.detached, run: () => { C.detached = false; S.notify({ title: 'Ветка создана', text: 'feature/sso-login-fix', tone: 'ok' }); S.render(); } },
      dismiss: { title: '', menu: false, run: () => { C.lastCommit = null; S.render(); } },
    },
  });
})();
