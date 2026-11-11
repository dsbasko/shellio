// Воркспейсы, проекты и операции над ними. Клавиша, меню, поиск и CLI вызывают одни и те же обработчики.
(() => {
  const St = S.st;
  const { esc, icon } = S;
  const plural = (...a) => S.fmt.plural(...a);
  const anchor = (sel) => document.querySelector(sel);
  const byTitle = (a, b) => a.title.localeCompare(b.title, 'ru');
  const repoWord = (n) => plural(n, 'репозиторий', 'репозитория', 'репозиториев');
  const projWord = (n) => plural(n, 'проект', 'проекта', 'проектов');
  const running = (n) => plural(n, 'сессия продолжает', 'сессии продолжают', 'сессий продолжают');

  S.workspaces = () => W.workspaces.filter((w) => !w.deletedAt).sort((a, b) => a.position - b.position);
  S.projectsIn = (ws) => Object.values(W.projects).filter((p) => p.ws === ws);
  S.repoWs = (repo) => { const p = Object.values(W.projects).find((x) => x.repos.includes(repo)); return p ? p.ws : null; };
  S.sessionsOf = (id) => Object.values(W.sessions).filter((s) => W.workbenches[s.wb].project === id && s.host !== 'finished' && s.state !== 'exited');
  S.repoProject = (repo) => { const p = Object.values(W.projects).find((x) => x.repos.includes(repo)); return p ? p.id : null; };
  S.projectWs = (id) => (W.projects[id] ? W.projects[id].ws : null);
  // Допустимый аккаунт ищется в воркспейсе проекта. Нет такого — операции с сервисом приостановлены.
  S.forgeAccount = (repo) => {
    const pr = W.projects[S.repoProject(repo)]; const r = W.repos[repo];
    if (!pr || !r) return null;
    const list = W.accounts.filter((a) => a.ws === pr.ws && a.provider === r.provider && a.host === r.host);
    return list.find((a) => a.id === (pr.forge || {})[r.host]) || list[0] || null;
  };
  S.rootOwner = (path) => Object.values(W.projects).find((p) => p.roots.includes(path)) || null;

  // В списке слева только выбранные вручную. Убранный сохраняет позицию на случай возврата.
  S.listed = (pr) => pr.listPos != null && !pr.removedAt;
  S.projectGroups = (ws, current) => {
    const live = S.projectsIn(ws).filter((p) => !p.removedAt);
    const outside = live.filter((p) => p.listPos == null).sort(byTitle);
    // Убранный текущий проект виден одной строкой, пока из него не ушли, но ни в какие списки не входит.
    const gone = W.projects[current] && W.projects[current].ws === ws && W.projects[current].removedAt ? W.projects[current] : null;
    return {
      listed: live.filter((p) => p.listPos != null).sort((a, b) => a.listPos - b.listPos),
      outside,
      here: outside.find((p) => p.id === current) || gone,
      total: live.length,
    };
  };
  // Точное имя важнее принадлежности к списку, совпадение по имени важнее совпадения по пути.
  S.projScore = (pr, query) => {
    const q = query.toLowerCase();
    if (pr.title.toLowerCase() === q) return 2000;
    const title = S.match(pr.title, q);
    if (title >= 500) return title;
    const root = S.match(pr.roots.join(' '), q);
    return Math.max(title, root >= 500 ? root / 2 : 0);
  };

  const DONE = ['replied', 'completed', 'failed', 'cancelled', 'expired'];
  // Идущие внешние записи и асинхронные операции проекта.
  const opsOf = (pr) => {
    const out = [];
    Object.values(W.mrs).filter((m) => pr.repos.includes(m.repo) && m.review && m.review.publication && !m.review.publication.done).forEach((m) => {
      const pub = m.review.publication; const unknown = pub.steps.some((s) => s.state === 'unknown');
      if (pub.running || unknown) out.push({ id: `pub:${m.id}`, kind: 'Публикация', title: `${m.id}: публикация ревью`, sub: unknown ? 'Результат неизвестен, нужна сверка' : 'Запрос выполняется', peer: null });
    });
    pr.workbenches.forEach((wb) => {
      const c = W.worktrees[W.workbenches[wb].worktree];
      if (c && c.msg.state === 'streaming') out.push({ id: `gen:${wb}`, kind: 'Генерация', title: `Сообщение коммита, ${W.workbenches[wb].title}`, sub: 'Идёт генерация', peer: null, cancel: () => S.genCancel(wb) });
    });
    return out;
  };
  // Несохранённый ввод: форма может быть закрыта, но текст ждёт пользователя.
  const draftsOf = (pr) => {
    const out = [];
    const row = (title, sub) => out.push({ kind: 'Черновик', title, sub });
    Object.values(W.mrs).filter((m) => pr.repos.includes(m.repo)).forEach((m) => {
      if (m.drafts.length) out.push({ kind: 'Черновики', title: `${m.id} ${m.title}`, sub: plural(m.drafts.length, 'неотправленный черновик', 'неотправленных черновика', 'неотправленных черновиков') });
      const r = m.review; if (!r) return;
      if ((r.summary || '').trim()) row(`${m.id}: сводка ревью`, 'Не опубликована');
      const replies = Object.values(r.replyTexts || {}).filter((v) => String(v || '').trim()).length;
      if (replies) row(`${m.id}: ответы в тредах`, plural(replies, 'не отправлен', 'не отправлено', 'не отправлено'));
    });
    pr.workbenches.forEach((wb) => {
      const b = W.workbenches[wb]; const c = W.worktrees[b.worktree];
      if (c && c.msg.state === 'manual' && c.msg.text.trim()) row(`Сообщение коммита, ${b.title}`, 'Коммит не создан');
      if (S.composerDrafts(wb)) row(`Вопрос агенту, ${b.title}`, 'Не отправлен');
    });
    pr.repos.forEach((repo) => { const n = W.dirtyBuffers(repo); if (n) row(`Редактор, ${repo}`, plural(n, 'несохранённый файл', 'несохранённых файла', 'несохранённых файлов')); });
    return out;
  };
  // Всё, что нельзя потерять вместе с записью проекта.
  S.projectWork = (id) => {
    const pr = W.projects[id];
    const out = S.sessionsOf(id).map((s) => ({ kind: 'Сессия', title: s.title, sub: S.stateName(s) }));
    out.push(...draftsOf(pr));
    (W.threads || []).filter((t) => !DONE.includes(t.state) && (pr.workbenches.includes(t.wb) || (W.sessions[S.threadTo(t)] && pr.workbenches.includes(W.sessions[S.threadTo(t)].wb)))).forEach((t) => out.push({ kind: 'Поручение', title: t.title, sub: 'Не завершено' }));
    (pr.blockers || []).forEach((b) => out.push(b));
    out.push(...opsOf(pr));
    return out;
  };

  const projectOfWb = (wb) => (W.workbenches[wb] ? W.workbenches[wb].project : null);
  const crossing = (t, set) => { const a = W.sessions[S.threadTo(t)]; const from = projectOfWb(t.wb); const to = a && projectOfWb(a.wb); return from && to && from !== to && set.has(from) !== set.has(to) ? { from, to } : null; };
  // Препятствия считаются из живой модели при каждом показе и повторно при выполнении.
  S.moveBlockers = (ids) => {
    const set = new Set(ids); const out = [];
    (W.threads || []).filter((t) => !DONE.includes(t.state)).forEach((t) => {
      const c = crossing(t, set); if (!c) return;
      const peer = set.has(c.from) ? c.to : c.from;
      out.push({ id: `thread:${t.id}`, kind: 'Поручение', title: t.title, sub: `Связывает ${W.projects[c.from].title} и ${W.projects[c.to].title}`, peer, cancel: () => S.threadCancel(t) });
    });
    ids.forEach((id) => {
      const pr = W.projects[id];
      (pr.blockers || []).filter((b) => !(b.peer && set.has(b.peer))).forEach((b) => out.push({ ...b, cancel: () => { pr.blockers = pr.blockers.filter((x) => x.id !== b.id); } }));
      out.push(...opsOf(pr));
    });
    return out;
  };

  // Сигналы других воркспейсов существуют только при включённой настройке и не прогревают холодные проекты.
  S.crossAttention = () => {
    const out = {};
    if (!St.prefs.crossAttention) return out;
    S.workspaces().filter((w) => w.id !== St.route.ws).forEach((w) => {
      const warm = S.projectsIn(w.id).filter((p) => S.sessionsOf(p.id).length || S.moveBlockers([p.id]).length);
      const waiting = warm.flatMap((p) => S.sessionsOf(p.id)).filter((s) => s.kind === 'agent' && s.state === 'waiting').length;
      // Новый запуск ветки вытесняет старые: упавший раньше не считается.
      const failed = new Set(warm.flatMap((p) => p.workbenches.map((id) => {
        const b = W.workbenches[id];
        return W.pipelines.filter((r) => r.repo === b.repo && r.ref === b.branch).sort((x, y) => y.id - x.id)[0];
      })).filter((r) => r && r.state === 'failed').map((r) => r.id)).size;
      if (waiting + failed) out[w.id] = waiting + failed;
    });
    return out;
  };
  S.crossTotal = () => Object.values(S.crossAttention()).reduce((a, b) => a + b, 0);

  // Переключение
  const validKey = (ws, key) => {
    if (!key) return false;
    if (S.isHomeKey(key)) return key === `home:${ws}`;
    const w = W.workbenches[key]; const pr = w && W.projects[w.project];
    return !!pr && pr.ws === ws && !pr.removedAt;
  };
  S.landing = (ws) => {
    if (validKey(ws, St.lastKey[ws])) return St.lastKey[ws];
    const pr = S.projectsIn(ws).filter((p) => S.listed(p) && p.workbenches.length).sort((a, b) => b.lastOpenedAt - a.lastOpenedAt)[0];
    return pr ? pr.lastWb || pr.workbenches[0] : `home:${ws}`;
  };
  S.land = (key) => {
    S.setRoute(key);
    S.seedLayout(key);
    if (S.isHomeKey(key)) S.open('projects', { zone: 'center' });
    else {
      S.noteOpen(key);
      const L = S.layout(key);
      if (L.focus && St.panels[L.focus]) { const def = S.plugins[St.panels[L.focus].plugin]; if (def.onFocus) def.onFocus(St.panels[L.focus]); }
    }
    S.render();
  };
  const blocking = () => St.overlays.find((o) => ['sheet', 'composer', 'welcome', 'firstrun'].includes(o.type));
  S.goWs = (id) => {
    if (!S.workspaces().some((w) => w.id === id)) return;
    if (blocking()) { S.notify({ title: 'Сначала закройте диалог', text: 'Воркспейс переключается, когда нет открытых диалогов.', tone: 'wait' }); return; }
    [...St.overlays].forEach((o) => S.ov.pop(o));
    S.clearPending();
    if (id === St.route.ws) { S.render(); return; }
    St.lastKey[St.route.ws] = S.lkey();
    S.land(S.landing(id));
  };

  S.openProject = (id) => {
    const pr = W.projects[id];
    if (pr.removedAt) { S.projRestorePrompt(id); return; }
    if (pr.missing) { S.notify({ project: id, title: `Папка не найдена: ${esc(pr.roots[0])}`, text: 'Наличие папки проверяется при открытии.', tone: 'wait', action: { label: 'Убрать из каталога', run: () => S.projRemove(id) } }); return; }
    const first = !pr.workbenches.length;
    if (first) {
      const wb = `wb-${S.uid('x')}`; const repo = W.repos[pr.repos[0]];
      W.workbenches[wb] = { id: wb, title: repo.branch, project: id, repo: repo.id, branch: repo.branch, worktree: repo.path };
      pr.workbenches.push(wb);
    }
    const target = pr.lastWb && pr.workbenches.includes(pr.lastWb) ? pr.lastWb : pr.workbenches[0];
    S.go(target);
    if (first) { S.panelsOf(target).forEach((p) => { p.mode = 'nav'; }); S.render(); }
  };

  S.openWb = (id) => {
    const gone = S.removedOwner(id);
    if (gone) S.projRestorePrompt(gone.id, () => S.go(id));
    else S.go(id);
  };

  S.projRestorePrompt = (id, then, sessionTitle) => {
    const pr = W.projects[id];
    S.sheet({
      icon: 'folder', title: `Проект «${esc(pr.title)}» убран из каталога`, width: 440,
      sub: sessionTitle ? `Сессия ${esc(sessionTitle)} продолжает работу. Чтобы открыть её, верните проект.` : 'Чтобы открыть его, верните проект.',
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Вернуть и открыть', kind: 'primary', key: 'Enter', run: () => { S.projRestore(id, { silent: true }); if (then) then(); else S.openProject(id); } }],
    });
  };

  // Операции
  S.projRenameTo = (id, raw) => {
    const pr = W.projects[id]; const title = (raw || '').trim();
    if (!title) { S.notify({ title: 'Имя не может быть пустым', tone: 'wait' }); return false; }
    if (S.projectsIn(pr.ws).some((p) => p.id !== id && p.title.toLowerCase() === title.toLowerCase())) { S.notify({ title: `Имя «${esc(title)}» уже занято`, text: 'Имя проекта уникально внутри воркспейса.', tone: 'wait' }); return false; }
    pr.title = title; S.render(); return true;
  };

  S.startRename = (p, id) => {
    p.state.renaming = id; p.state.renameVal = W.projects[id].title;
    S.renderNow();
    const inp = document.querySelector(`.panel[data-panel="${p.id}"] [data-rename]`);
    if (!inp) return;
    inp.focus(); inp.select();
    let closed = false;
    const finish = () => { closed = true; p.state.renaming = null; S.render(); };
    inp.addEventListener('keydown', (e) => {
      if (closed) return;
      if (e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); if (S.projRenameTo(id, p.state.renameVal)) finish(); }
      else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); finish(); }
    }, true);
    inp.addEventListener('blur', () => { if (closed) return; S.projRenameTo(id, p.state.renameVal); finish(); });
  };

  const tailPos = (ws) => Math.max(-1, ...S.projectsIn(ws).map((p) => (p.listPos == null ? -1 : p.listPos))) + 1;
  S.projListMove = (id, d) => {
    const pr = W.projects[id];
    if (!S.listed(pr)) return;
    const list = S.projectGroups(pr.ws).listed;
    const other = list[list.indexOf(pr) + d];
    if (!other) return;
    [pr.listPos, other.listPos] = [other.listPos, pr.listPos];
    S.render();
  };

  // Показ и скрытие всегда явные: отмена из уведомления может сработать намного позже и не должна переключать состояние.
  S.projShow = (id, o = {}) => {
    const pr = W.projects[id];
    if (!pr || pr.removedAt || pr.listPos != null) return false;
    const free = o.slot != null && o.ws === pr.ws && !S.projectsIn(pr.ws).some((p) => p.listPos === o.slot);
    pr.listPos = free ? o.slot : tailPos(pr.ws);
    S.render();
    return true;
  };
  const undoShow = (id, place) => {
    const pr = W.projects[id];
    if (!pr) return;
    if (pr.removedAt) { S.notify({ title: 'Проект убран из каталога', text: 'Сначала верните его.', tone: 'wait' }); return; }
    S.projShow(id, place);
  };
  S.projHide = (id, o = {}) => {
    const pr = W.projects[id];
    if (!pr || pr.listPos == null) return false;
    const place = { slot: pr.listPos, ws: pr.ws };
    pr.listPos = null;
    if (!o.silent) {
      const n = S.sessionsOf(id).length;
      S.notify({ project: id, title: `Проект «${esc(pr.title)}» скрыт из списка`, text: n ? `${running(n)} работу.` : '', action: { label: 'Показать', run: () => undoShow(id, place) } });
    }
    S.render();
    return true;
  };
  S.projToggle = (id, o = {}) => (S.listed(W.projects[id]) ? S.projHide(id, o) : S.projShow(id));
  // Массовые показ и скрытие применяются сразу, отмена относится только к реально изменённым.
  S.projListMany = (ids, on, done) => {
    const changed = ids.map((id) => W.projects[id]).filter((pr) => pr && !pr.removedAt && (pr.listPos != null) !== on);
    if (done) done();
    if (!changed.length) { S.notify({ title: on ? 'Уже в списке' : 'Уже вне списка' }); S.render(); return; }
    const before = Object.fromEntries(changed.map((pr) => [pr.id, { slot: pr.listPos, ws: pr.ws }]));
    changed.forEach((pr) => { if (on) S.projShow(pr.id); else S.projHide(pr.id, { silent: true }); });
    const title = (n) => `${on ? 'Показано в списке' : 'Скрыто из списка'}: ${projWord(n)}`;
    S.notify({
      title: title(changed.length), tone: 'ok', parts: changed.map((pr) => pr.id), split: title,
      action: { label: 'Отменить', run: (t) => {
        const alive = t.parts.map((id) => W.projects[id]).filter((pr) => pr && !pr.removedAt && pr.ws === t.ws).sort((a, b) => before[a.id].slot - before[b.id].slot);
        if (!alive.length) { S.notify({ title: 'Проекты убраны из каталога', text: 'Сначала верните их.', tone: 'wait' }); return; }
        alive.forEach((pr) => { if (on) S.projHide(pr.id, { silent: true }); else S.projShow(pr.id, before[pr.id]); });
      } },
    });
    S.render();
  };

  S.projRemove = (id) => {
    const pr = W.projects[id];
    pr.removedAt = Date.now();
    const n = S.sessionsOf(id).length;
    S.notify({ project: id, title: `Проект «${esc(pr.title)}» убран из каталога`, text: n ? `${running(n)} работу.` : 'Папки на диске не затронуты.', action: { label: 'Вернуть проект', run: () => S.projRestore(id) } });
    S.render();
  };
  S.projRestore = (id, o = {}) => {
    const pr = W.projects[id];
    if (!pr) return;
    if (!S.workspaces().some((w) => w.id === pr.ws)) { S.notify({ title: 'Сначала верните воркспейс', text: 'Проект вернётся только вместе с ним.', tone: 'wait' }); return; }
    pr.removedAt = null;
    if (!o.silent) S.notify({ title: `Проект «${esc(pr.title)}» возвращён`, tone: 'ok' });
    S.render();
  };

  const eraseProject = (id) => {
    const pr = W.projects[id];
    Object.keys(St.lastKey).forEach((ws) => { if (pr.workbenches.includes(St.lastKey[ws])) delete St.lastKey[ws]; });
    if (pr.workbenches.includes(St.route.wb)) S.land(S.landing(pr.ws));
    pr.workbenches.forEach((wb) => {
      Object.values(St.panels).filter((p) => p.wb === wb).forEach((p) => {
        const def = S.plugins[p.plugin];
        if (def && def.destroy) def.destroy(p, { silent: true });
        delete St.panels[p.id];
      });
      delete St.layouts[wb];
      Object.values(W.sessions).filter((s) => s.wb === wb).forEach((s) => delete W.sessions[s.id]);
      delete W.worktrees[W.workbenches[wb].worktree];
      S.dropComposerDrafts(wb);
      delete W.workbenches[wb];
    });
    Object.values(W.mrs).filter((m) => pr.repos.includes(m.repo)).forEach((m) => { if (W.review && W.review.saved) delete W.review.saved[m.url]; delete W.mrs[m.id]; });
    W.mrList = W.mrList.filter((x) => W.mrs[x]);
    W.pipelines.filter((r) => pr.repos.includes(r.repo)).forEach((r) => { S.stopRunWatch(r); delete W.jobs[r.id]; });
    W.pipelines = W.pipelines.filter((r) => !pr.repos.includes(r.repo));
    W.threads = W.threads.filter((t) => W.workbenches[t.wb] || t.homeWs);
    pr.repos.forEach((r) => { W.dropBuffers(r); delete W.repos[r]; });
    delete W.projects[id];
    Object.keys(St.histories).forEach((ws) => { St.histories[ws] = St.histories[ws].filter((h) => !h.wb || S.wsOf(h.wb)); });
    S.dropToasts(id);
  };
  const workRows = (work) => work.map((w, i) => K.row({ k: `w${i}`, title: esc(w.title), sub: `${w.kind}. ${esc(w.sub)}`, quiet: true })).join('');
  S.projForget = (ids) => {
    ids = [].concat(ids);
    const free = ids.filter((id) => !S.projectWork(id).length);
    const busy = ids.filter((id) => S.projectWork(id).length);
    S.sheet({
      icon: 'close', title: ids.length === 1 ? `Забыть проект «${esc(W.projects[ids[0]].title)}»?` : `Забыть ${projWord(ids.length)}?`, width: 480,
      sub: 'Запись, раскладки и настройки shellio будут стёрты без возможности вернуть. Папки на диске останутся.',
      body: () => `${busy.length ? `${K.sec('Сначала завершите работу')}${busy.map((id) => `${K.sec(esc(W.projects[id].title))}${workRows(S.projectWork(id))}`).join('')}` : ''}${free.length > 1 ? `${K.sec('Будут забыты', free.length)}${free.map((id) => K.row({ k: id, title: esc(W.projects[id].title), quiet: true })).join('')}` : ''}`,
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Забыть', kind: 'danger', disabled: () => !free.length, run: () => { const now = free.filter((id) => W.projects[id] && !S.projectWork(id).length); now.forEach(eraseProject); S.notify({ title: now.length === 1 ? 'Проект забыт' : `Забыто ${projWord(now.length)}`, tone: now.length ? 'ok' : 'wait' }); S.render(); } }],
    });
  };

  S.projMoveDo = (ids, target, o = {}) => {
    const from = W.projects[ids[0]].ws;
    const cur = S.project();
    const moving = !!cur && ids.includes(cur.id);
    if (o.cli && moving && S.ov.has('sheet')) { S.notify({ ws: from, title: 'Перенос из shellioctl отклонён', text: 'Для этого проекта открыт диалог.', tone: 'wait' }); return; }
    const blockers = S.moveBlockers(ids);
    if (blockers.length) { S.notify({ ws: from, title: 'Перенос отклонён', text: `Сначала завершите: ${blockers.map((b) => esc(b.title)).join(', ')}.`, tone: 'wait' }); return; }
    const set = new Set(ids);
    (W.threads || []).forEach((t) => { if (crossing(t, set)) t.homeWs = t.homeWs || from; });
    const tail = tailPos(target);
    const carried = target === from ? [] : ids.map((id) => W.projects[id]).filter((p) => p.listPos != null).sort((a, b) => a.listPos - b.listPos);
    ids.forEach((id) => { W.projects[id].ws = target; W.projects[id].forge = { ...(o.picks || {}) }; });
    carried.forEach((p, i) => { p.listPos = tail + i; });
    // Участники снова вместе: история больше не привязана к прежнему воркспейсу.
    (W.threads || []).forEach((t) => { const a = W.sessions[S.threadTo(t)]; if (t.homeWs && a && S.wsOf(a.wb) === S.wsOf(t.wb)) delete t.homeWs; });
    S.moveToasts(ids, target);
    St.histories[from] = (St.histories[from] || []).filter((h) => !h.wb || S.wsOf(h.wb) === from);
    delete St.lastKey[from];
    const title = W.workspaces.find((w) => w.id === target).title;
    const one = W.projects[ids[0]];
    if (moving && !o.cli) St.route.ws = target;
    else if (moving) S.land(S.landing(from));
    const name = ids.length === 1 ? `Проект «${esc(one.title)}»` : projWord(ids.length);
    if (o.cli) S.notify({ ws: from, title: `${name} перенесён в «${esc(title)}»`, text: 'Команда из shellioctl не переключает воркспейс.', action: { label: 'Перейти', run: () => { if (!W.projects[ids[0]] || W.projects[ids[0]].ws !== target) return; S.goWs(target); S.openProject(ids[0]); } } });
    else S.notify({ ws: moving ? target : from, title: `${name} перенесён в «${esc(title)}»`, tone: 'ok' });
    S.render();
  };

  S.projMove = (ids, opts = {}) => {
    ids = [].concat(ids);
    const from = W.projects[ids[0]].ws;
    const targets = S.workspaces().filter((w) => w.id !== from);
    if (!targets.length) { S.notify({ title: 'Перенести некуда', text: 'Создайте второй воркспейс.', tone: 'wait', action: { label: 'Новый воркспейс', run: () => S.wsNew() } }); return; }
    const st = { target: opts.target || targets[0].id, peers: [], picks: {} };
    const group = () => [...ids, ...st.peers];
    const blockers = () => S.moveBlockers(group());
    const accounts = () => {
      const repos = group().flatMap((id) => W.projects[id].repos).map((r) => W.repos[r]);
      const of = (r) => W.accounts.filter((a) => a.ws === st.target && a.provider === r.provider && a.host === r.host);
      const lacking = [...new Set(repos.filter((r) => !of(r).length).map((r) => r.host))];
      const choices = [...new Map(repos.filter((r) => of(r).length > 1).map((r) => [r.host, of(r)])).entries()];
      return { lacking, choices };
    };
    S.sheet({
      icon: 'folder', width: 520, state: st,
      title: ids.length === 1 ? `Перенести проект «${esc(W.projects[ids[0]].title)}»` : `Перенести ${projWord(ids.length)}`,
      sub: 'Запущенные процессы продолжат работу и не перезапустятся.',
      body: () => {
        const b = blockers(); const acc = accounts();
        return `${K.sec('В воркспейс')}${targets.map((w) => `<div class="${S.cx('opt', st.target === w.id && 'on')}" data-do="target" data-arg="${w.id}">${K.radio(st.target === w.id)}<div><b>${esc(w.title)}</b><span class="s">${projWord(S.projectsIn(w.id).filter((p) => !p.removedAt).length)}</span></div></div>`).join('')}
        ${group().length > ids.length ? `${K.sec('Вместе с ним')}${st.peers.map((id) => K.row({ k: id, icon: 'folder', title: esc(W.projects[id].title), quiet: true })).join('')}` : ''}
        ${b.length ? `${K.sec('Сначала разберите', b.length)}${b.map((x) => K.row({ k: x.id, title: esc(x.title), sub: `${x.kind}. ${esc(x.sub)}`, quiet: true, trail: `${x.peer ? K.btn(`Вместе с ${esc(W.projects[x.peer].title)}`, { small: true, do: 'peer', arg: x.peer }) : ''} ${x.cancel ? K.btn('Отменить', { small: true, do: 'drop', arg: x.id }) : ''}` })).join('')}` : ''}
        ${acc.choices.map(([host, list]) => `${K.sec(`Аккаунт для ${esc(host)}`)}${list.map((a, i) => { const on = st.picks[host] ? st.picks[host] === a.id : i === 0; return `<div class="${S.cx('opt', on && 'on')}" data-do="acc" data-arg="${esc(`${host}|${a.id}`)}">${K.radio(on)}<div><b>${esc(a.user)}</b><span class="s">${esc(a.host)}</span></div></div>`; }).join('')}`).join('')}
        ${acc.lacking.length ? `<div class="add-note">Нет аккаунта для ${acc.lacking.map(esc).join(', ')} в этом воркспейсе. Операции с этими сервисами приостановятся, пока вы не подключите его в настройках.</div>` : ''}`;
      },
      on: {
        target: (a) => { st.target = a; st.picks = {}; },
        peer: (a) => { if (!st.peers.includes(a)) st.peers.push(a); },
        drop: (a) => { const b = blockers().find((x) => x.id === a); if (b && b.cancel) b.cancel(); },
        acc: (a) => { const [host, id] = a.split('|'); st.picks[host] = id; },
      },
      note: () => (blockers().length ? 'Перенос станет доступен, когда список будет пуст' : ''),
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Перенести', kind: 'primary', key: 'Enter', disabled: () => blockers().length > 0, run: () => S.projMoveDo(group(), st.target, { picks: st.picks }) }],
    });
  };

  // Добавление
  const nameFor = (path, ws) => {
    const base = path.replace(/\/+$/, '').split('/').pop().replace(/\.git$/, '') || 'project';
    const taken = new Set(S.projectsIn(ws).map((p) => p.title.toLowerCase()));
    if (!taken.has(base.toLowerCase())) return base;
    let i = 2; while (taken.has(`${base}-${i}`.toLowerCase())) i++;
    return `${base}-${i}`;
  };
  const createProject = (path, ws, url, show) => {
    const title = nameFor(path, ws); const id = `${title}-${S.uid('p')}`;
    W.repos[id] = { id, provider: url && url.includes('github') ? 'github' : 'gitlab', host: url && url.includes('github') ? 'github.com' : 'gitlab.com', path, branch: 'main', head: 'c0ffee1' };
    W.projects[id] = W.newProject(id, ws, { title, repos: [id], roots: [path], listPos: show ? tailPos(ws) : null });
    return id;
  };
  const SCAN = ['~/src/new-service', '~/src/tools/release-cli', '~/src/labs/poc-rag', '~/src/atlas', '~/dotfiles', '~/src/cms-admin-old'];
  const parsePaths = (text) => [...new Set(text.split('\n').map((x) => x.trim().replace(/\/+$/, '')).filter(Boolean))];
  const classify = (path, ws) => {
    const owner = S.rootOwner(path);
    if (!owner) return { path, kind: 'new' };
    if (owner.ws === ws) return { path, owner, kind: owner.removedAt ? 'back' : 'here' };
    if (!S.workspaces().some((w) => w.id === owner.ws)) return { path, owner, kind: 'away-deleted' };
    return { path, owner, kind: owner.removedAt ? 'away-removed' : 'away' };
  };
  const wsTitle = (id) => esc(W.workspaces.find((w) => w.id === id).title);

  S.projAdd = (tab = 'folders') => {
    const ws = St.route.ws;
    const st = { tab, paths: '', url: '', dest: '~/src', root: '~/src', scan: null, picked: {}, show: {}, clone: null };
    let timer = null;
    const stop = () => { clearInterval(timer); timer = null; };
    const lines = () => (st.tab === 'scan' ? (st.scan && st.scan.done ? st.scan.found.filter((p) => st.picked[p]) : []) : st.tab === 'folders' ? parsePaths(st.paths) : []);
    const addable = () => lines().map((p) => classify(p, ws)).filter((c) => c.kind === 'new' || c.kind === 'back');
    const scanShown = () => (st.tab === 'scan' ? addable().filter((c) => c.kind === 'new' && st.show[c.path]) : []);
    const statusRow = (c) => {
      const trail = {
        here: K.btn('Открыть', { small: true, do: 'openIt', arg: c.owner && c.owner.id }),
        away: `${K.btn('Открыть там', { small: true, do: 'openIt', arg: c.owner && c.owner.id })} ${K.btn('Перенести сюда', { small: true, do: 'moveIt', arg: c.owner && c.owner.id })}`,
        'away-removed': `${K.btn('Вернуть там', { small: true, do: 'backThere', arg: c.owner && c.owner.id })} ${K.btn('Перенести сюда', { small: true, do: 'moveIt', arg: c.owner && c.owner.id })}`,
        'away-deleted': K.btn('Вернуть воркспейс', { small: true, do: 'backWs', arg: c.owner && c.owner.ws }),
      }[c.kind] || '';
      const sub = {
        new: `Будет добавлен как «${esc(nameFor(c.path, ws))}»`,
        here: `Уже есть в этом воркспейсе, ${c.owner && S.listed(c.owner) ? 'в списке' : 'вне списка'}`,
        back: 'Был убран из каталога. Вернётся прежний проект',
        away: `Занят: воркспейс «${c.owner ? wsTitle(c.owner.ws) : ''}»`,
        'away-removed': `Убран из каталога в «${c.owner ? wsTitle(c.owner.ws) : ''}»`,
        'away-deleted': `Принадлежит удалённому воркспейсу «${c.owner ? wsTitle(c.owner.ws) : ''}»`,
      }[c.kind];
      return K.row({ k: c.path, title: esc(c.path), mono: true, sub, quiet: true, trail });
    };
    const clonePath = () => `${st.dest.replace(/\/+$/, '')}/${st.url.trim().split('/').pop().replace(/\.git$/, '')}`;
    const cloneCheck = () => (st.url.trim() ? classify(clonePath(), ws) : null);
    const startScan = () => {
      st.scan = { seen: 0, found: [], done: false };
      stop();
      timer = setInterval(() => {
        st.scan.seen += 180;
        const n = Math.min(SCAN.length, Math.floor(st.scan.seen / 240));
        st.scan.found = SCAN.slice(0, n);
        if (n >= SCAN.length) { stop(); st.scan.done = true; st.picked = Object.fromEntries(SCAN.filter((p) => classify(p, ws).kind !== 'here').map((p) => [p, true])); }
        S.render();
      }, 160);
    };
    S.sheet({
      icon: 'folder', width: 600, state: st, title: `Добавить проект в «${esc(S.ws().title)}»`,
      body: () => {
        const tabs = K.seg([['folders', 'Папки'], ['clone', 'Клонировать'], ['scan', 'Найти в папке']].map(([id, label]) => ({ label, on: st.tab === id, do: 'tab', arg: id })));
        let main = '';
        if (st.tab === 'folders') main = `<label class="form-l">Папки, по одной на строку</label>${K.field({ bind: 'paths', value: st.paths, rows: 3, placeholder: '~/src/my-project' }).replace('<textarea', '<textarea data-autofocus')}${parsePaths(st.paths).length ? `<div class="add-list">${parsePaths(st.paths).map((p) => statusRow(classify(p, ws))).join('')}</div>` : ''}`;
        if (st.tab === 'clone') main = `<label class="form-l">Адрес репозитория</label>${K.field({ bind: 'url', value: st.url, placeholder: 'git@gitlab.com:team/project.git' }).replace('<input', '<input data-autofocus')}<label class="form-l">Куда</label>${K.field({ bind: 'dest', value: st.dest })}${cloneCheck() && cloneCheck().kind !== 'new' ? `<div class="add-list">${statusRow(cloneCheck())}</div>` : ''}${st.clone != null ? `<div class="add-progress"><i style="width:${st.clone}%"></i></div><div class="add-note">Клонирую, ${st.clone}%. Окно можно закрыть, клонирование отменится.</div>` : ''}`;
        if (st.tab === 'scan') main = `<label class="form-l">Папка</label><div style="display:flex;gap:8px">${K.field({ bind: 'root', value: st.root })}${st.scan && !st.scan.done ? K.btn('Отменить поиск', { do: 'cancelScan' }) : K.btn('Найти', { do: 'scan', disabled: !st.root.trim() })}</div>${st.scan ? `<div class="add-note">${st.scan.done ? `Найдено: ${repoWord(st.scan.found.length)}` : `Просмотрено папок: ${st.scan.seen}, найдено: ${st.scan.found.length}`}</div>` : ''}${st.scan && st.scan.found.length ? `<div class="add-list">${st.scan.found.map((p) => { const c = classify(p, ws); return `<div class="scan-row">${c.kind === 'here' ? '<span style="width:14px"></span>' : K.check(!!st.picked[p], { do: 'pick', arg: p })}<div style="flex:1;min-width:0">${statusRow(c)}</div>${c.kind === 'new' && st.picked[p] ? `<span class="scan-show">${K.check(!!st.show[p], { do: 'show', arg: p })}Показать слева</span>` : ''}</div>`; }).join('')}</div>` : ''}`;
        return `<div style="margin:2px 0 10px">${tabs}</div>${main}`;
      },
      on: {
        tab: (a) => { st.tab = a; },
        scan: () => startScan(),
        cancelScan: () => { stop(); st.scan = null; },
        pick: (a) => { st.picked[a] = !st.picked[a]; },
        show: (a) => { st.show[a] = !st.show[a]; },
        openIt: (id, o) => { S.ov.pop(o); if (W.projects[id].ws !== St.route.ws) S.goWs(W.projects[id].ws); S.openProject(id); },
        moveIt: (id, o) => { S.ov.pop(o); W.projects[id].removedAt = null; S.projMove([id], { target: ws }); },
        backThere: (id, o) => { S.ov.pop(o); S.projRestore(id); },
        backWs: (id) => { W.workspaces.find((w) => w.id === id).deletedAt = null; S.notify({ title: 'Воркспейс возвращён', tone: 'ok' }); },
      },
      onBind: () => S.render(),
      onClose: stop,
      actions: [
        { label: 'Отменить', key: 'Esc', run: stop },
        {
          label: () => {
            if (st.tab === 'clone') return cloneCheck() && cloneCheck().kind === 'back' ? 'Вернуть проект' : 'Клонировать';
            const n = addable().length; const shown = scanShown().length;
            return `${n > 1 || shown ? `Добавить ${projWord(n)}` : 'Добавить'}${shown ? `, показать ${shown}` : ''}`;
          },
          kind: 'primary', close: false,
          disabled: () => (st.tab === 'clone' ? !st.url.trim() || st.clone != null || !['new', 'back'].includes((cloneCheck() || {}).kind) : !addable().length),
          run: (o) => {
            if (st.tab === 'clone') {
              const path = clonePath();
              const check = classify(path, ws);
              if (check.kind === 'back') { stop(); S.ov.pop(o); S.projRestore(check.owner.id, { silent: true }); S.openProject(check.owner.id); return; }
              if (check.kind !== 'new') return;
              st.clone = 0;
              timer = setInterval(() => {
                st.clone = Math.min(100, st.clone + 20);
                if (st.clone >= 100) {
                  stop(); S.ov.pop(o);
                  if (classify(path, ws).kind !== 'new') { S.notify({ title: `Папка уже занята: ${esc(path)}`, tone: 'wait' }); return; }
                  S.openProject(createProject(path, ws, st.url, true));
                  return;
                }
                S.render();
              }, 240);
              S.render();
              return;
            }
            const list = addable();
            const scan = st.tab === 'scan';
            const ids = list.map((c) => { if (c.kind === 'back') { S.projRestore(c.owner.id, { silent: true }); return c.owner.id; } return createProject(c.path, ws, null, !scan || !!st.show[c.path]); });
            stop(); S.ov.pop(o);
            // Поиск только регистрирует найденное, даже если нашёлся один репозиторий.
            if (scan) {
              const back = list.filter((c) => c.kind === 'back').length; const added = list.length - back;
              const inList = ids.filter((id) => S.listed(W.projects[id])).length;
              S.notify({ title: back ? (added ? `Добавлено ${added}, возвращено ${back}` : `Возвращено ${projWord(back)}`) : `Добавлено ${projWord(added)}`, text: inList ? `В списке слева: ${inList}.` : 'В список слева они не попали.', tone: 'ok', action: { label: 'Выбрать', run: () => S.projPick() } });
              S.render();
            }
            else if (ids.length === 1) S.openProject(ids[0]);
            else { S.notify({ title: `Добавлено ${projWord(ids.length)}`, tone: 'ok' }); S.render(); }
          },
        },
      ],
    });
  };

  // Меню проекта и массовые действия
  S.projActions = (id, p) => {
    const pr = W.projects[id];
    if (pr.removedAt) return [
      { id: 'restore', title: 'Вернуть проект', key: 'u', run: () => S.projRestore(id) },
      { id: 'forget', title: 'Забыть проект…', key: 'F', run: () => S.projForget(id) },
    ];
    return [
      { id: 'open', title: 'Открыть', key: 'Enter', run: () => S.openProject(id) },
      { id: 'rename', title: 'Переименовать', key: 'r', run: () => S.startRename(p, id) },
      { id: 'list', title: S.listed(pr) ? 'Скрыть из списка' : 'Показать в списке', key: 'x', run: () => S.projToggle(id) },
      { id: 'move', title: 'Перенести в воркспейс…', key: 'm', run: () => S.projMove([id]) },
      { id: 'remove', title: 'Убрать из каталога', key: 'D', run: () => S.projRemove(id) },
    ];
  };
  S.projMenu = (id, el, p) => {
    const acts = S.projActions(id, p);
    S.popover(el, {
      title: esc(W.projects[id].title), width: 250,
      body: () => acts.map((a) => K.row({ k: a.id, title: a.title, do: 'act', arg: a.id, quiet: true, trail: K.kbd(a.key) })).join(''),
      on: { act: (a, o) => { S.ov.pop(o); acts.find((x) => x.id === a).run(); } },
    });
  };

  const BULK = {
    remove: { title: 'Убрать из каталога', verb: 'Убрать', run: (id) => { W.projects[id].removedAt = Date.now(); } },
    restore: { title: 'Вернуть', verb: 'Вернуть', run: (id) => { W.projects[id].removedAt = null; } },
  };
  S.projBulk = (kind, ids, done) => {
    if (kind === 'move') { S.projMove(ids); return; }
    if (kind === 'forget') { S.projForget(ids); return; }
    if (kind === 'show' || kind === 'hide') { S.projListMany(ids, kind === 'show', done); return; }
    const b = BULK[kind];
    const live = ids.flatMap((id) => S.sessionsOf(id));
    S.sheet({
      icon: 'folder', width: 480, title: `${b.title}: ${projWord(ids.length)}`,
      sub: kind === 'remove' ? 'Папки на диске не затрагиваются, процессы не останавливаются.' : '',
      body: () => `${K.sec('Состав', ids.length)}${ids.map((id) => K.row({ k: id, icon: 'folder', title: esc(W.projects[id].title), trail: S.sessionsOf(id).length ? plural(S.sessionsOf(id).length, 'сессия', 'сессии', 'сессий') : '', quiet: true })).join('')}${live.length && kind !== 'restore' ? `<div class="add-note">${running(live.length)} работу.</div>` : ''}`,
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: b.verb, kind: 'primary', key: 'Enter', run: () => { ids.forEach(b.run); if (done) done(); S.notify({ title: `${b.title}: ${projWord(ids.length)}`, tone: 'ok' }); S.render(); } }],
    });
  };

  // Воркспейсы
  const uniqueWs = (title, except) => !S.workspaces().some((w) => w.id !== except && w.title.toLowerCase() === title.toLowerCase());
  S.wsNew = () => {
    S.sheet({
      icon: 'folder', title: 'Новый воркспейс', sub: 'Проекты, сессии и уведомления воркспейсов не пересекаются.', width: 420, state: { name: '' },
      body: (o) => `<label class="form-l">Название</label>${K.field({ bind: 'name', value: o.state.name, placeholder: 'Клиенты' }).replace('<input', '<input data-autofocus')}`,
      onBind: () => S.render(),
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Создать', kind: 'primary', key: 'Enter', disabled: (o) => !o.state.name.trim() || !uniqueWs(o.state.name.trim()), run: (o) => { const id = S.uid('ws'); W.workspaces.push({ id, title: o.state.name.trim(), position: Math.max(...W.workspaces.map((w) => w.position)) + 1, deletedAt: null }); S.goWs(id); } }],
    });
  };

  const deleteBlockers = (id) => {
    const projects = S.projectsIn(id);
    const live = projects.filter((p) => !p.removedAt);
    const removedBusy = projects.filter((p) => p.removedAt && S.projectWork(p.id).length);
    return { live, removedBusy, last: S.workspaces().length < 2 };
  };
  S.wsEdit = (id = St.route.ws) => {
    const w = W.workspaces.find((x) => x.id === id);
    const st = { name: w.title };
    const move = (d) => {
      const list = S.workspaces(); const i = list.indexOf(w); const other = list[i + d];
      if (other) { [w.position, other.position] = [other.position, w.position]; S.render(); }
    };
    S.sheet({
      icon: 'folder', title: 'Воркспейс', width: 440, state: st,
      body: () => {
        const b = deleteBlockers(id); const list = S.workspaces(); const i = list.indexOf(w);
        const why = b.last ? 'Последний воркспейс удалить нельзя.' : b.live.length ? `Сначала перенесите или уберите ${projWord(b.live.length)}.` : b.removedBusy.length ? `У убранных проектов есть незавершённая работа: ${b.removedBusy.map((p) => esc(p.title)).join(', ')}.` : 'Убранные проекты удалятся вместе с воркспейсом и вернутся только вместе с ним.';
        return `<label class="form-l">Название</label>${K.field({ bind: 'name', value: st.name }).replace('<input', '<input data-autofocus')}<label class="form-l">Порядок и номер</label><div style="display:flex;align-items:center;gap:6px">${K.kbd(`Cmd-${i + 1}`)}${K.btn('Выше', { small: true, do: 'up', disabled: i === 0 })}${K.btn('Ниже', { small: true, do: 'down', disabled: i === list.length - 1 })}</div><div class="add-note">${why}</div>`;
      },
      on: { up: () => move(-1), down: () => move(1) },
      onBind: () => S.render(),
      actions: [
        { label: 'Отменить', key: 'Esc' },
        { label: 'Удалить', kind: 'danger', disabled: () => { const b = deleteBlockers(id); return b.last || b.live.length > 0 || b.removedBusy.length > 0; }, run: () => {
          w.deletedAt = Date.now();
          if (St.route.ws === id) S.goWs(S.workspaces()[0].id);
          S.notify({ title: `Воркспейс «${esc(w.title)}» удалён`, action: { label: 'Вернуть', run: () => { w.deletedAt = null; S.render(); } } });
        } },
        { label: 'Сохранить', kind: 'primary', key: 'Enter', disabled: () => !st.name.trim() || !uniqueWs(st.name.trim(), id), run: () => { w.title = st.name.trim(); S.render(); } },
      ],
    });
  };

  S.wsPopover = () => {
    const had = S.ov.has('pop'); if (had && had.wsPop) { S.ov.pop(had); return; }
    const marks = S.crossAttention();
    const openCatalog = () => S.open('projects', { zone: 'center' });
    const o = S.popover(anchor('.ws-btn'), {
      title: 'Воркспейсы', width: 300,
      body: () => `${S.workspaces().map((w, i) => K.row({ k: w.id, lead: `<span class="ws-check">${w.id === St.route.ws ? icon('check', 13) : ''}</span>`, title: esc(w.title), sub: projWord(S.projectsIn(w.id).filter((p) => !p.removedAt).length), do: 'go', arg: w.id, quiet: true, trail: `${marks[w.id] ? `<span class="ws-n">${marks[w.id]}</span>` : ''}${i < 9 ? K.kbd(`Cmd-${i + 1}`) : ''}` })).join('')}<div class="pop-sep"></div>${K.row({ k: 'sw', icon: 'branch', title: 'Проект и рабочая область…', do: 'sw', quiet: true, trail: K.kbd('s') })}${K.row({ k: 'new', icon: 'plus', title: 'Новый воркспейс', do: 'new', quiet: true, trail: K.kbd('n') })}${K.row({ k: 'edit', icon: 'gear', title: `Изменить «${esc(S.ws().title)}»…`, do: 'edit', quiet: true, trail: K.kbd('e') })}${K.row({ k: 'cat', icon: 'grid', title: 'Управление проектами…', do: 'cat', quiet: true, trail: K.kbd('c') })}`,
      on: { go: (id) => S.goWs(id), sw: (_, x) => { S.ov.pop(x); S.switcher(); }, new: (_, x) => { S.ov.pop(x); S.wsNew(); }, edit: (_, x) => { S.ov.pop(x); S.wsEdit(); }, cat: (_, x) => { S.ov.pop(x); openCatalog(); } },
      onKey: (e, tok, x) => {
        if (/^[1-9]$/.test(tok)) { const w = S.workspaces()[+tok - 1]; if (w) S.goWs(w.id); return true; }
        if (tok === 's') { S.ov.pop(x); S.switcher(); return true; }
        if (tok === 'n') { S.ov.pop(x); S.wsNew(); return true; }
        if (tok === 'e') { S.ov.pop(x); S.wsEdit(); return true; }
        if (tok === 'c') { S.ov.pop(x); openCatalog(); return true; }
        return false;
      },
    });
    o.wsPop = true;
  };

  // Выбор проектов для списка слева. Порядок строк фиксируется при открытии, галочки применяются сразу.
  const PICK_LIMIT = 100;
  S.projPick = () => {
    const had = S.ov.has('pop');
    if (had && had.projPick) { S.ov.pop(had); return; }
    const ws = St.route.ws;
    const g = S.projectGroups(ws);
    if (!g.total) { S.projAdd(); return; }
    const recentFirst = (a, b) => b.lastOpenedAt - a.lastOpenedAt || byTitle(a, b);
    const order = [...g.listed, ...[...g.outside].sort(recentFirst)].map((p) => p.id);
    const st = { q: '', cur: order[0] };
    const found = () => {
      const q = st.q.trim();
      const all = order.map((id) => W.projects[id]).filter((p) => p && !p.removedAt && p.ws === ws);
      if (!q) return all;
      return all.map((p, n) => ({ p, n, s: S.projScore(p, q) })).filter((x) => x.s > 0).sort((a, b) => b.s - a.s || a.n - b.n).map((x) => x.p);
    };
    const shown = () => found().slice(0, PICK_LIMIT);
    const field = () => document.querySelector('.pick-q input');
    const reveal = () => { S.renderNow(); const el = document.querySelector('.pick-list .row.on'); if (el) el.scrollIntoView({ block: 'nearest' }); };
    const flip = (id) => { const pr = W.projects[id]; if (!pr || pr.removedAt) return; if (pr.listPos != null) S.projHide(id, { silent: true }); else S.projShow(id); st.cur = id; S.renderNow(); const f = field(); if (f) { f.focus(); f.select(); } };
    const catalog = (o) => { S.ov.pop(o); const c = S.open('projects', { zone: 'center' }); if (c) { c.state.tab = 'all'; c.state.sel = {}; c.filter = st.q.trim(); } S.render(); };
    const add = (tab, o) => { S.ov.pop(o); S.projAdd(tab); };
    const pop = S.popover(anchor('.nav-add') || anchor('.ws-btn'), {
      title: 'Проекты в списке', width: 380, state: st,
      head: `<span class="meter">${K.kbd('Enter')} переключить, применяется сразу</span>`,
      body: () => {
        const all = found(); const rows = all.slice(0, PICK_LIMIT);
        return `<div class="pick-q" style="padding:0 10px 8px">${K.field({ bind: 'q', value: st.q, placeholder: 'Найти проект' }).replace('<input', '<input data-autofocus')}</div><div class="pick-list">${rows.map((p) => { const n = S.sessionsOf(p.id).length; return K.row({ k: p.id, lead: K.check(p.listPos != null), title: esc(p.title), sub: esc(p.roots[0] || ''), on: p.id === st.cur, do: 'flip', arg: p.id, trail: n ? `<span class="meta">${n}</span>` : '' }); }).join('') || '<div class="pad row-s">Ничего не найдено</div>'}</div><div class="pick-foot">${all.length > PICK_LIMIT ? `<div class="pick-more"><span>Показано ${PICK_LIMIT} из ${all.length}</span>${K.btn('Открыть каталог', { small: true, kind: 'flat', do: 'catalog' })}</div>` : ''}<div class="pick-add">${K.btn('Добавить папку…', { small: true, do: 'add', arg: 'folders' })}${K.btn('Клонировать…', { small: true, do: 'add', arg: 'clone' })}${K.btn('Найти в папке…', { small: true, do: 'add', arg: 'scan' })}</div></div>`;
      },
      on: { flip: (id) => flip(id), catalog: (_, o) => catalog(o), add: (tab, o) => add(tab, o) },
      onKey: (e, tok) => {
        const typing = S.isField(document.activeElement);
        const d = { Down: 1, 'Ctrl-n': 1, Up: -1, 'Ctrl-p': -1, ...(typing ? {} : { j: 1, k: -1 }) }[tok];
        if (d) {
          const list = shown();
          if (list.length) st.cur = list[S.clamp(list.findIndex((x) => x.id === st.cur) + d, 0, list.length - 1)].id;
          reveal();
          return true;
        }
        if ((tok === 'Enter' || (tok === 'Space' && !typing)) && shown().some((x) => x.id === st.cur)) { flip(st.cur); return true; }
        return false;
      },
    });
    pop.projPick = true;
    pop.onBind = () => { const list = shown(); if (!list.some((x) => x.id === st.cur)) st.cur = list[0] ? list[0].id : null; S.render(); };
    return pop;
  };

  // Переключатель проекта и рабочей области внутри воркспейса
  S.switcher = () => {
    const had = S.ov.has('pop'); if (had) S.ov.pop(had);
    const cur = S.wb(); const ws = St.route.ws;
    const st = { q: '', project: cur ? cur.project : null, wb: cur ? cur.id : null, col: 1 };
    // Строка «Сейчас» описывает открытое место, а не выбранного кандидата.
    const here = () => {
      if (!cur) return '<div class="sw-now">Проект не открыт</div>';
      const pr = W.projects[cur.project];
      return `<div class="sw-now" title="${esc(pr.title)} · ${esc(cur.title)}"><span>Сейчас:</span><b>${esc(pr.title)}</b><span>·</span><span class="sw-area">${esc(cur.title)}</span>${pr.removedAt ? `<span>· Убран</span>${K.btn('Вернуть', { small: true, kind: 'flat', do: 'restoreHere' })}` : ''}</div>`;
    };
    const prL = () => {
      const g = S.projectGroups(ws);
      const pool = [...g.listed, ...g.outside];
      const q = st.q.trim();
      if (q) return pool.map((p) => ({ p, s: S.projScore(p, q) })).filter((x) => x.s > 0).sort((a, b) => b.s - a.s).map((x) => x.p).slice(0, 100);
      return pool.filter((p) => p.lastOpenedAt || (cur && p.id === cur.project)).sort((a, b) => b.lastOpenedAt - a.lastOpenedAt || byTitle(a, b)).slice(0, 100);
    };
    const wbL = () => (st.project ? W.projects[st.project].workbenches.map((id) => W.workbenches[id]) : []);
    const fix = () => {
      const list = prL();
      if (!list.some((p) => p.id === st.project)) st.project = list[0] ? list[0].id : null;
      if (!wbL().some((w) => w.id === st.wb)) st.wb = wbL()[0] ? wbL()[0].id : null;
    };
    fix();
    const go = (o) => {
      if (!st.project) return;
      S.ov.pop(o);
      if (st.wb) S.go(st.wb); else S.openProject(st.project);
    };
    const reveal = () => { S.renderNow(); const el = document.querySelector(`.sw-${st.col} .row.on`); if (el) el.scrollIntoView({ block: 'nearest' }); };
    const pop = S.popover(anchor('.ws-btn'), {
      title: 'Проект и рабочая область', width: 520, state: st,
      head: `<span class="meter">${K.kbd('Up')}${K.kbd('Down')} строка &nbsp; ${K.kbd('Left')}${K.kbd('Right')} колонка &nbsp; ${K.kbd('Enter')} открыть</span>`,
      body: () => `${here()}<div style="padding:0 10px 8px">${K.field({ bind: 'q', value: st.q, placeholder: 'Найти проект' }).replace('<input', '<input data-autofocus')}</div>${K.cols([
        { w: '240px', html: `<div class="sw-1">${K.sec(st.q.trim() ? 'Проекты' : 'Недавние проекты')}<div class="sw-list">${prL().map((p) => K.row({ k: p.id, icon: 'folder', title: esc(p.title), sub: repoWord(p.repos.length), on: st.project === p.id, quiet: st.col !== 1, do: 'pr', arg: p.id })).join('') || '<div class="pad row-s">Ничего не найдено</div>'}</div></div>` },
        { html: `<div class="sw-2">${K.sec('Рабочая область')}<div class="sw-list">${wbL().map((w) => K.row({ k: w.id, icon: 'grid', title: esc(w.title), sub: w.branch, on: st.wb === w.id, quiet: st.col !== 2, do: 'wb', arg: w.id })).join('') || '<div class="pad row-s">Первая область создастся при открытии</div>'}</div></div>` },
      ])}`,
      on: { pr: (a) => { st.project = a; st.col = 1; st.wb = null; fix(); }, wb: (a, o) => { st.wb = a; go(o); }, restoreHere: (_, o) => { S.ov.pop(o); S.projRestore(cur.project); } },
      onKey: (e, tok, o) => {
        if (tok === 'Left') { st.col = 1; reveal(); return true; }
        if (tok === 'Right') { if (wbL().length) st.col = 2; reveal(); return true; }
        if (tok === 'Down' || tok === 'Up' || tok === 'Ctrl-n' || tok === 'Ctrl-p') {
          const d = tok === 'Down' || tok === 'Ctrl-n' ? 1 : -1;
          const list = st.col === 1 ? prL() : wbL();
          const key = st.col === 1 ? 'project' : 'wb';
          const i = list.findIndex((x) => x.id === st[key]);
          if (list.length) st[key] = list[S.clamp(i + d, 0, list.length - 1)].id;
          if (st.col === 1) { st.wb = null; fix(); }
          reveal();
          return true;
        }
        if (tok === 'Enter') { go(o); return true; }
        return false;
      },
    });
    pop.onBind = () => { st.col = 1; fix(); S.render(); };
    return pop;
  };

  const C = (id, title, keys, run, extra = {}) => S.cmd({ id, title, keys, run, ...extra });
  C('ws.switch', 'Переключить воркспейс', ['Space W'], () => S.wsPopover(), { grp: 'Воркспейс', icon: 'folder' });
  C('ws.new', 'Новый воркспейс', [], () => S.wsNew(), { grp: 'Воркспейс', icon: 'folder' });
  C('ws.edit', 'Изменить воркспейс', [], () => S.wsEdit(), { grp: 'Воркспейс', icon: 'folder' });
  for (let i = 1; i <= 9; i++) C(`ws.go.${i}`, `Воркспейс ${i}`, [`Cmd-${i}`], () => { const w = S.workspaces()[i - 1]; if (w) S.goWs(w.id); }, { hidden: true, pal: false });
  C('projects', 'Управление проектами', ['Space P'], () => S.open('projects', { zone: 'center' }), { grp: 'Воркспейс', icon: 'folder' });
  C('project.add', 'Добавить проект', [], () => S.projAdd(), { grp: 'Воркспейс', icon: 'plus' });
  C('project.pick', 'Выбрать проекты для списка', [], () => S.projPick(), { grp: 'Воркспейс', icon: 'plus' });
})();
