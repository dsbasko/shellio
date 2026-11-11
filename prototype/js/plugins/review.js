(() => {
  const E = S.esc;
  const copy = (v) => JSON.parse(JSON.stringify(v));
  const storageKey = 'shellio.review.journal.v1';
  const status = (s) => ({ opened: 'Открыто', closed: 'Закрыто', merged: 'Слито', running: 'Выполняется', pending: 'В очереди', ok: 'Успешно', failed: 'Ошибка', skipped: 'Пропущено', manual: 'Ожидает запуска' }[s] || s);
  const tone = (s) => ({ running: 'run', pending: 'idle', failed: 'err', ok: 'ok', merged: 'ok', closed: 'exit' }[s] || 'idle');
  const labels = { overview: 'Обзор', files: 'Файлы', threads: 'Треды', versions: 'Версии' };
  const outcomeLabels = { COMMENT: 'Comment', APPROVE: 'Approve', REQUEST_CHANGES: 'Request changes' };
  const paragraph = (s) => E(s).replace(/\n/g, '<br>');
  const field = (label, bind, value, rows, auto) => `<label class="form-l">${E(label)}${K.field({ bind, value, rows }).replace(auto ? '<input' : '<never', '<input data-autofocus').replace(auto ? '<textarea' : '<never', '<textarea data-autofocus')}</label>`;
  const button = (label, action, arg, extra = {}) => K.btn(label, { do: action, arg, small: true, ...extra });
  const account = (m) => S.forgeAccount(m.repo);
  // Без допустимого аккаунта в воркспейсе проекта запись на сервер приостановлена.
  function syncForge(m) {
    const a = account(m); const c = m.review.capabilities;
    m.review.account = a ? a.id : 'local';
    m.review.paused = !a;
    if (!a && c.write) { c.write = false; c.pausedWrite = true; }
    else if (a && c.pausedWrite) { c.write = true; delete c.pausedWrite; }
  }
  const refs = (m) => ({ base_sha: m.base, start_sha: m.start || m.base, head_sha: m.head });
  const freshRevision = (m) => ({ ...refs(m), files: [...m.files], diffs: copy(Object.fromEntries(m.files.map((f) => [f, W.diffs[f] || []]))) });

  const seedTargets = copy(W.mrs);
  W.review = W.review || { next: 1, remote: {}, saved: {}, storageError: '' };
  W.review.saved = {};
  function persist() {
    const saved = {};
    Object.values(W.mrs).forEach((m) => { if (m.review) saved[m.url] = { drafts: m.drafts, threads: m.threads, review: m.review }; });
    W.review.saved = saved;
  }
  const uid = (prefix) => `${prefix}-${Date.now().toString(36)}-${W.review.next++}`;
  function anchor(m, revision, file, line, side = 'new', start = line) {
    if (m.provider === 'github') return { commit_id: revision.head_sha, path: file, line, side: side === 'old' ? 'LEFT' : 'RIGHT', start_line: start, start_side: side === 'old' ? 'LEFT' : 'RIGHT' };
    return { base_sha: revision.base_sha, start_sha: revision.start_sha, head_sha: revision.head_sha, old_path: file, new_path: file, old_line: side === 'old' ? line : null, new_line: side === 'new' ? line : null, line_range: { start: { type: side, line: start }, end: { type: side, line } } };
  }
  function ensure(m, restore = true) {
    if (!m) return null;
    if (m.review) { syncForge(m); return m; }
    const current = freshRevision(m);
    const previous = { ...copy(current), head_sha: m.commits[1] || m.base };
    const previousFiles = new Set(m.commits.slice(1).flatMap((sha) => W.commits[sha]?.files || []));
    previous.diffs = Object.fromEntries(m.files.map((f) => [f, previousFiles.has(f) ? copy(W.diffs[f] || []) : []]));
    const base = { ...copy(current), head_sha: m.base, diffs: Object.fromEntries(m.files.map((f) => [f, []])) };
    m.review = {
      account: account(m)?.id || 'local', host: W.repos[m.repo].host, repository: m.repo,
      revision: copy(current), revisions: [base, previous, current].filter((r, i, all) => all.findIndex((v) => v.head_sha === r.head_sha) === i),
      capabilities: { requestChanges: true, approve: true, merge: true, write: true },
      summary: '', outcome: 'COMMENT', lastReviewed: previous.head_sha, browser: null, browserChoice: null,
      publication: null, publications: [], viewedByRevision: { [m.head]: copy(m.viewed) }, serverViewed: {}, reanchor: null,
    };
    m.drafts.forEach((d) => Object.assign(d, { side: d.side || 'new', start: d.start || d.line, head: m.head, saved: true, anchor: anchor(m, current, d.file, d.line, d.side || 'new', d.start || d.line) }));
    m.threads.forEach((t) => { t.head = t.head || m.head; t.side = t.side || 'new'; t.remoteId = t.remoteId || String(t.id); });
    const saved = restore && W.review.saved[m.url];
    if (saved?.review && saved.review.host === m.review.host && saved.review.account === m.review.account) {
      m.drafts = saved.drafts || [];
      m.review = saved.review;
      const pub = m.review.publication;
      if (pub) pub.steps.forEach((s) => { if (s.state === 'in_flight') s.state = 'unknown'; });
      if (pub) { pub.running = false; pub.reconciling = false; }
      if (saved.threads) m.threads = saved.threads;
    }
    return m;
  }
  Object.values(W.mrs).forEach((m) => ensure(m));
  const target = (p) => {
    const m = ensure(W.mrs[p.state.target]);
    if (m?.autoMerge && m.autoMerge.head === m.head && m.autoMerge.target === m.target && !blockers(m).length) {
      m.state = 'merged'; m.mergeMethod = 'auto'; m.mergedHead = m.head; m.autoMerge = null; persist();
    }
    return m;
  };
  const hasTarget = (p) => !!target(p);
  const isDiff = (p) => hasTarget(p) && p.state.area === 'diff';
  const editable = (m) => m.review.capabilities.write && (!m.review.publication || m.review.publication.done);
  const matching = (p, text) => S.match(text, p.filter || '') > 0;
  const targets = (p) => W.mrList.map((id) => ensure(W.mrs[id])).filter((m) => m && S.repoWs(m.repo) === S.st.route.ws && matching(p, `${m.id} ${m.title} ${m.provider} ${m.repo} ${m.author} ${m.branch} ${m.labels.join(' ')}`));
  const files = (p) => target(p)?.review.revision.files.filter((f) => matching(p, f)) || [];
  const currentFile = (p) => p.state.file && files(p).includes(p.state.file) ? p.state.file : files(p)[0];
  const threads = (p) => target(p)?.threads.filter((t) => matching(p, `${t.file} ${t.body} ${t.who}`)) || [];
  const currentThread = (p) => { const list = threads(p); return (p.state.area === 'thread' ? list.find((t) => t.id === p.state.thread) : list[p.state.cur]) || list[0]; };
  const diffData = (p) => ({ path: currentFile(p), hunks: target(p)?.review.revision.diffs[currentFile(p)] || [] });
  const flat = (p) => K.flat(diffData(p));
  const viewed = (m, f) => !!m.review.viewedByRevision[m.review.revision.head_sha]?.[f];
  const stale = (m, d) => d.head !== m.head;
  const pipeline = (m) => W.pipelines.find((r) => r.id === m.pipeline);
  function blockers(m) {
    const result = [];
    if (m.review.paused) result.push('Нет аккаунта в этом воркспейсе');
    else if (!m.review.capabilities.merge || !m.review.capabilities.write) result.push('Нет прав на слияние');
    if (m.state !== 'opened') result.push(status(m.state));
    if (m.draft) result.push('Цель пока в статусе Draft');
    if (m.conflicts) result.push('Ветка конфликтует с целевой');
    const run = pipeline(m);
    if (!run || run.sha !== m.head) result.push('Нет проверок текущего head');
    else if (run.state !== 'ok') result.push(`Проверки: ${status(run.state).toLowerCase()}`);
    if (m.threads.some((t) => !t.resolved)) result.push('Есть неразрешённые треды');
    if (!m.reviewers.some((r) => r.state === 'approved')) result.push('Нужно одобрение ревьюера');
    if (m.review.outcomeSent === 'REQUEST_CHANGES') result.push('Запрошены изменения');
    return result;
  }
  function mark(p) {
    const before = { ...p.state }; const filterBefore = p.filter;
    S.mark(p, () => { p.state = before; p.filter = filterBefore; });
  }
  function openTarget(id) { if (W.mrs[id]) return S.open('review', { key: id, target: id }); }
  function section(p, name) { if (!target(p)) return; mark(p); p.state.section = name; p.state.area = 'list'; p.state.cur = 0; p.filter = ''; S.render(); }
  function openFile(p, path) {
    if (!target(p)) return;
    mark(p); p.state.file = path || currentFile(p); p.state.section = 'files'; p.state.area = 'diff'; p.state.line = 0; p.state.selection = null; S.render();
  }
  function openThread(p, id) {
    const t = target(p)?.threads.find((v) => v.id === id) || currentThread(p); if (!t) return;
    mark(p); p.state.thread = t.id; p.state.section = 'threads'; p.state.area = 'thread'; S.render();
  }
  // Пока операции приостановлены, новая отправка недоступна. Начатую публикацию всё ещё можно открыть.
  const submitMuted = (m) => !!m.review.paused && !m.review.publication;
  const pausedText = (m) => `В этом воркспейсе нет аккаунта для ${W.repos[m.repo].host}. Подключите его в настройках.`;
  function denied(m) { if (m.review.paused) return K.banner('wait', 'Операции с сервисом приостановлены', pausedText(m), button('Настройки', 'settings')); return K.banner('err', 'Нет прав на изменение', `Для ${E(m.id)} доступен просмотр. Проверьте аккаунт и права в настройках.`, button('Настройки', 'settings')); }
  function startAllowed(p) {
    const m = target(p); if (!m) return false;
    if (m.review.browser && !m.review.browserChoice) { browserSheet(p); return false; }
    if (!editable(m)) { p.state.error = 'Изменение недоступно. Проверьте права или завершите сверку отправки.'; S.render(); return false; }
    return true;
  }

  function targetHeader(p, compact = false) {
    const m = target(p);
    const count = K.chip(`Черновики: ${m.drafts.length}`, { tone: m.drafts.length ? 'wait' : undefined, icon: 'draft' });
    if (compact) return K.ctx(`${S.icon(m.provider, 14)}${E(m.id)}`, `${count}${button('', 'submit', null, { icon: 'send', tip: 'Отправить ревью', disabled: submitMuted(m) })}`) + K.ctx(`<span title="${E(m.title)}">${E(m.title)}</span>`) + K.ctx(`<span class="row-s">${E(m.author)} / ${E(m.branch)}</span>`);
    return K.ctx(`${S.icon(m.provider, 14)}<span title="${E(m.title)}">${E(m.title)}</span><span class="row-s">${E(m.author)} / ${E(m.branch)}</span>`, `${count}${button('Отправить ревью', 'submit', null, { kind: 'primary', disabled: submitMuted(m) })}`);
  }
  function banners(p) {
    const m = target(p), r = m.review; let html = '';
    if (r.revision.head_sha !== m.head) html += K.banner('wait', `Head изменился: ${E(m.head)}`, `Открыта ревизия ${E(r.revision.head_sha)}. ${r.stay ? 'Вы остались на ней; черновики сохраняют прежние якоря.' : 'Выберите ревизию перед отправкой.'}`) + `<div class="co-chips">${button('Остаться', 'stayHead')}${button('Перейти', 'followHead')}${button('Перепривязать', 'reanchor')}</div>`;
    if (r.browser && !r.browserChoice) html += K.banner('wait', 'Есть браузерный черновик', 'Откройте весь состав перед началом работы.', button('Просмотреть', 'browserDraft'));
    if (!r.capabilities.write) html += denied(m);
    if (W.review.storageError) html += K.banner('err', W.review.storageError);
    if (p.state.error) html += K.banner('wait', E(p.state.error), '', button('Закрыть', 'clearError'));
    if (r.publication) {
      const pub = r.publication;
      const unknown = pub.steps.some((s) => s.state === 'unknown');
      const confirmed = pub.steps.filter((s) => s.state === 'confirmed').length;
      html += K.banner(pub.done ? 'ok' : pub.error ? 'err' : 'wait', pub.done ? 'Ревью отправлено' : unknown ? 'Результат неизвестен' : pub.error ? 'Ревью отправлено частично' : 'Публикация ревью', pub.error || `Подтверждено шагов: ${confirmed} из ${pub.steps.length}.`, button(unknown ? 'Сверить' : 'Подробности', unknown ? 'reconcile' : 'submit'));
    }
    if (m.autoMerge) html += K.banner('info', m.autoMerge.head === m.head ? 'Автослияние включено' : 'Автослияние ждёт подтверждения нового head', `Ветка ${E(m.target)}. ${blockers(m).join(', ') || 'Все условия выполнены.'}`, button('Проверить условия', 'autoMerge'));
    return html;
  }
  function listView(p, geo) {
    const list = targets(p); p.state.cur = S.clamp(p.state.cur, 0, Math.max(0, list.length - 1));
    const narrow = geo.size === 'Narrow' || geo.width < 620;
    return K.ctx('GitLab и GitHub', `${button('По ссылке', 'openURL', null, { icon: 'link' })}${button(narrow ? '' : 'Создать', 'create', null, { icon: 'plus', tip: 'Создать MR или PR' })}`) + K.scroll(list.length ? K.table({
      cols: narrow ? [{ t: 'Цель', w: '90px' }, { t: 'Название' }] : [{ t: 'Цель', w: '90px' }, { t: 'Название' }, { t: 'Репозиторий', w: '130px' }, { t: 'Автор', w: '110px' }, { t: 'Статус', w: '100px' }, { t: 'Черновики', w: '85px', num: true }],
      rows: list.map((m, i) => ({ k: m.id, on: p.state.cur === i, do: 'openTarget', arg: m.id, cells: narrow ? [K.chip(E(m.id), { icon: m.provider }), `${E(m.title)}<span class="row-s"> / ${E(m.provider)}</span>`] : [K.chip(E(m.id), { icon: m.provider }), `${E(m.title)}${m.draft ? ' (Draft)' : ''}`, E(m.repo), E(m.author), K.status(tone(m.state), status(m.state)), m.drafts.length] })),
    }) : K.empty({ icon: 'review', title: 'Целей не найдено', text: 'Измените фильтр или откройте ссылку.', actions: button('Открыть ссылку', 'openURL') }));
  }
  function overview(p) {
    const m = target(p), r = m.review, run = pipeline(m), blocked = blockers(m);
    return K.scroll(`${K.pad(paragraph(m.description))}${K.sec('Коммиты', m.commits.length)}${m.commits.map((sha) => {
      const c = W.commits[sha]; return K.row({ k: sha, icon: 'commit', title: E(c?.title || sha), sub: `${E(sha)} / ${E(c?.author || m.author)}`, do: 'commit', arg: sha });
    }).join('')}${K.sec('Проверки')}${run ? K.row({ icon: 'pipeline', st: tone(run.state), title: `Запуск ${E(run.id)}: ${status(run.state)}`, sub: `SHA ${E(run.sha)}${run.sha !== m.head ? ', относится к прошлой ревизии' : ', текущий head'}`, do: 'checks', arg: String(run.id) }) : K.pad('Проверок нет')}${(W.jobs[run?.id] || []).map((stage) => stage.jobs.map((job) => K.row({ indent: 1, st: tone(job.state), title: E(job.name), trail: E(job.dur), do: 'checks', arg: String(run.id) })).join('')).join('')}${K.sec('Участники', button('Изменить', 'reviewers'))}${K.pad(K.kv(m.reviewers.map((v) => [E(v.who), v.state === 'approved' ? 'Одобрено' : v.state === 'you' ? 'Ваше ревью' : 'Ожидается'])))}${K.sec('Метки', button('Изменить', 'labels'))}${K.pad(m.labels.map((l) => K.chip(E(l))).join(' '))}${K.sec('Готовность к слиянию')}${K.pad(K.kv([['Целевая ветка', E(m.target)], ['Состояние', status(m.state)], ['Готовность', blocked.length ? 'Есть препятствия' : 'Можно сливать'], ['Просмотрено', `${r.revision.files.filter((f) => viewed(m, f)).length} из ${r.revision.files.length}`]]))}${blocked.map((b) => K.row({ icon: 'warn', title: E(b) })).join('')}${K.pad(`${button('Merge', 'merge')}${button('Squash', 'squash')}${button('Auto-merge', 'autoMerge')}`)}${K.sec('Действия')}${K.pad(`${button(m.draft ? 'Ready' : 'Draft', m.draft ? 'ready' : 'draft')}${button(m.state === 'closed' ? 'Reopen' : 'Close', m.state === 'closed' ? 'reopenTarget' : 'closeTarget')}${button('Assignees', 'assignees')}${button('Checkout', 'checkout')}${button('В браузере', 'browser')}`)}`);
  }
  function fileList(p) {
    const m = target(p); return files(p).map((f) => K.row({ k: f, on: f === currentFile(p), lead: K.check(viewed(m, f), { do: 'viewed', arg: f }), title: E(f.split('/').pop()), sub: E(f.split('/').slice(0, -1).join('/')), do: 'openFile', arg: f, trail: m.drafts.filter((d) => d.file === f).length || '' })).join('') || K.empty({ title: 'Файлов не найдено', actions: button('Сбросить фильтр', 'clearFilter') });
  }
  function draftCard(m, d, editableCard = true) {
    return `<div class="inline-card draft" data-k="draft-${E(d.id)}"><div class="ic-h">${K.chip(d.by ? `Предложено: ${E(d.by)}` : d.remoteId ? 'Серверный черновик' : 'Локальный черновик', { tone: 'wait' })}<span>${E(d.file)}:${E(d.range || d.line)} / ${d.side === 'old' ? 'старая' : 'новая'}</span>${stale(m, d) ? K.chip('Старая ревизия', { tone: 'err' }) : ''}</div><div class="ic-b">${paragraph(d.body)}${editableCard ? `<div style="margin-top:6px">${button('Изменить', 'editDraft', d.id)}${button('Удалить', 'deleteDraft', d.id)}</div>` : ''}</div></div>`;
  }
  function threadCard(p, t) {
    return `<div class="inline-card" data-k="thread-${E(t.id)}"><div class="ic-h">${E(t.who)}<span>${E(t.file)}:${E(t.line)}</span>${K.chip(t.resolved ? 'Разрешён' : 'Открыт', { tone: t.resolved ? 'ok' : undefined })}</div><div class="ic-b">${paragraph(t.body)}${t.replies.map((r) => `<p><b>${E(r.who)}</b><br>${paragraph(r.body)}</p>`).join('')}<div style="margin-top:6px">${button('Ответить', 'reply', t.id)}${button(t.resolved ? 'Открыть снова' : 'Resolve', 'resolve', t.id)}</div></div></div>`;
  }
  function renderRows(data, split) {
    const rows = []; let offset = 0;
    data.hunks.forEach((h) => {
      for (let i = 0; i < h.lines.length;) {
        if (!split || h.lines[i].t === ' ') { rows.push({ key: offset + i, indices: [offset + i] }); i++; continue; }
        const begin = i, dels = [], adds = [];
        while (i < h.lines.length && h.lines[i].t === '-') dels.push(offset + i++);
        while (i < h.lines.length && h.lines[i].t === '+') adds.push(offset + i++);
        for (let n = 0; n < Math.max(dels.length, adds.length); n++) rows.push({ key: offset + begin + n, indices: [dels[n], adds[n]].filter((x) => x != null) });
      }
      offset += h.lines.length;
    }); return rows;
  }
  function composer(p) {
    const m = target(p), d = m.drafts.find((v) => v.id === p.state.editing); if (!d) return '';
    return `<div class="inline-card draft" data-k="composer-${E(d.id)}"><div class="ic-h">Черновик к ${d.side === 'old' ? 'старой' : 'новой'} стороне, строки ${E(d.range)}</div><div class="ic-b">${K.field({ bind: 'draftBody', value: d.body, rows: 3, placeholder: 'Комментарий к строке' })}${button('Готово', 'saveDraft', null, { kind: 'primary', key: '⌘↩' })}${button('Suggestion', 'suggestion')}${button('Закрыть', 'stopEditing')}<div class="row-s" style="margin-top:4px">Хранится на этом Mac и ещё не опубликован</div></div></div>`;
  }
  function fileDiff(p, geo) {
    const m = target(p), data = diffData(p), lines = K.flat(data); if (!data.path) return K.empty({ title: 'Выберите файл' });
    const split = geo.width >= 780 && p.state.sideBySide;
    const rows = renderRows(data, split); p.state.renderRows = rows;
    p.state.line = S.clamp(p.state.line, 0, Math.max(0, lines.length - 1));
    const selected = rows.find((r) => r.indices.includes(p.state.line));
    const selection = p.state.selection == null ? null : [Math.min(p.state.selection, p.state.line), Math.max(p.state.selection, p.state.line)];
    const editor = m.drafts.find((d) => d.id === p.state.editing);
    const visibleDrafts = new Set();
    const after = (index) => {
      const row = rows.find((r) => r.key === index); const nums = (row?.indices || [index]).map((i) => lines[i]);
      const at = (d) => d.file === data.path && d.head === m.review.revision.head_sha && nums.some((l) => (d.side === 'old' ? l.o : l.n) === d.line);
      const ds = m.drafts.filter((d) => at(d) && d.id !== editor?.id); ds.forEach((d) => visibleDrafts.add(d.id));
      const edit = editor && at(editor); if (edit) visibleDrafts.add(editor.id);
      return m.threads.filter(at).map((t) => threadCard(p, t)).join('') + ds.map((d) => draftCard(m, d)).join('') + (edit ? composer(p) : '');
    };
    const html = K.diff(data, { mode: split ? 'split' : 'unified', cursor: selected?.key, sel: selection ? [rows.find((r) => r.indices.includes(selection[0]))?.key, rows.find((r) => r.indices.includes(selection[1]))?.key] : null, onLine: 'lineClick', sideArg: true, side: p.state.side, after });
    const sides = K.seg([{ label: 'Старая', on: p.state.side === 'old', do: 'side', arg: 'old' }, { label: 'Новая', on: p.state.side !== 'old', do: 'side', arg: 'new' }]);
    const controls = `${button('Диапазон', 'selectRange')}${button('Комментарий', 'comment')}${geo.width >= 780 ? button(split ? 'В одну' : 'Рядом', 'diffMode') : ''}`;
    return K.ctx(`${geo.size === 'Narrow' || p.state.area === 'diff' ? button('', 'back', null, { icon: 'chevL', tip: 'К файлам' }) : ''}<span title="${E(data.path)}">${E(data.path.split('/').pop())}</span>${geo.width >= 460 ? K.chip(E(m.review.revision.head_sha)) : ''}`, `${button(geo.width < 360 ? '' : viewed(m, data.path) ? 'Просмотрено' : 'Отметить просмотренным', 'viewed', data.path, { icon: viewed(m, data.path) ? 'check' : 'eye', tip: 'Отметить файл просмотренным' })}${button('nvim', 'gf')}`) + (geo.width < 520 ? K.ctx(sides) + K.ctx(controls) : K.ctx(sides, controls)) + K.scroll(html + m.drafts.filter((d) => d.file === data.path && !visibleDrafts.has(d.id) && d.id !== editor?.id).map((d) => draftCard(m, d)).join('') + (editor?.file === data.path && !visibleDrafts.has(editor.id) ? composer(p) : ''));
  }
  function threadList(p) {
    return threads(p).map((t, i) => K.row({ k: t.id, on: i === p.state.cur, icon: t.resolved ? 'check' : 'review', title: E(t.body), sub: `${E(t.file)}:${t.line} / ${E(t.who)}`, do: 'openThread', arg: t.id })).join('') || K.empty({ title: 'Тредов нет', text: 'Откройте файл и добавьте комментарий.', actions: button('К файлам', 'files') });
  }
  function snapshotLines(m, rev, path, hi) {
    const h = rev.diffs[path]?.[hi];
    const source = h || m.review.revisions.find((r) => r.diffs[path]?.[hi])?.diffs[path][hi];
    if (!source) return [];
    return source.lines.filter((l) => h ? l.t !== '-' : l.t !== '+').map((l) => ({ number: h ? l.n : l.o, text: l.s }));
  }
  function compare(m, from, to, path) {
    const count = Math.max(from.diffs[path]?.length || 0, to.diffs[path]?.length || 0); const hunks = [];
    for (let hi = 0; hi < count; hi++) {
      const a = snapshotLines(m, from, path, hi), b = snapshotLines(m, to, path, hi);
      if (JSON.stringify(a.map((l) => l.text)) === JSON.stringify(b.map((l) => l.text))) continue;
      const dp = Array.from({ length: a.length + 1 }, () => Array(b.length + 1).fill(0));
      for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--) dp[i][j] = a[i].text === b[j].text ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
      const lines = []; let i = 0, j = 0;
      while (i < a.length || j < b.length) {
        if (i < a.length && j < b.length && a[i].text === b[j].text) { lines.push({ t: ' ', o: a[i].number, n: b[j].number, s: a[i].text }); i++; j++; }
        else if (i < a.length && (j === b.length || dp[i + 1][j] >= dp[i][j + 1])) { lines.push({ t: '-', o: a[i].number, n: null, s: a[i++].text }); }
        else { lines.push({ t: '+', o: null, n: b[j].number, s: b[j++].text }); }
      }
      hunks.push({ header: `${from.head_sha} / ${to.head_sha}`, lines });
    }
    return { path, hunks };
  }
  function versions(p, geo) {
    const m = target(p), r = m.review, from = r.revisions.find((v) => v.head_sha === p.state.from) || r.revisions[0], to = r.revisions.find((v) => v.head_sha === p.state.to) || r.revisions[r.revisions.length - 1];
    const diffs = [...new Set([...from.files, ...to.files])].map((f) => compare(m, from, to, f)).filter((d) => d.hunks.length);
    return K.scroll(`${K.sec('Исходная ревизия')}${K.pad(K.seg(r.revisions.map((v) => ({ label: E(v.head_sha), on: v === from, do: 'compareFrom', arg: v.head_sha }))))}${K.sec('Конечная ревизия')}${K.pad(K.seg(r.revisions.map((v) => ({ label: E(v.head_sha), on: v === to, do: 'compareTo', arg: v.head_sha }))))}${K.pad(button('Изменения с прошлого ревью', 'sinceReview'))}${diffs.map((d) => `${K.sec(E(d.path))}${K.diff(d, { mode: geo.width >= 780 ? 'split' : 'unified' })}`).join('') || K.empty({ title: 'Между ревизиями нет изменений' })}`);
  }
  function targetView(p, geo, narrow) {
    const m = target(p); if (!m) return listView(p, geo);
    return targetHeader(p, narrow) + sectionNav(p, narrow) + banners(p) + sectionBody(p, geo, narrow);
  }
  function sectionNav(p, narrow) {
    const entries = Object.entries(labels);
    const segment = (items) => K.seg(items.map(([id, label]) => ({ label, on: p.state.section === id, do: id })));
    return narrow ? K.ctx(segment(entries.slice(0, 2))) + K.ctx(segment(entries.slice(2))) : K.ctx(segment(entries));
  }
  function sectionBody(p, geo, narrow) {
    let body;
    if (p.state.section === 'overview') body = overview(p);
    else if (p.state.section === 'files') body = narrow ? (p.state.area === 'diff' ? fileDiff(p, geo) : K.scroll(fileList(p))) : K.cols([{ w: geo.size === 'Wide' ? '230px' : '180px', html: K.scroll(fileList(p)) }, { html: fileDiff(p, { ...geo, width: geo.width - (geo.size === 'Wide' ? 230 : 180) }) }]);
    else if (p.state.section === 'threads') body = p.state.area === 'thread' ? K.stack([K.ctx(button('К тредам', 'back')), K.scroll(currentThread(p) ? threadCard(p, currentThread(p)) : '')]) : K.scroll(threadList(p));
    else body = versions(p, geo);
    return body;
  }
  function bottom(p, geo) {
    const m = target(p); if (!m) return listView(p, geo);
    const pub = m.review.publication; const unknown = pub?.steps.some((s) => s.state === 'unknown');
    const compact = geo.width < 460;
    const action = unknown ? 'reconcile' : m.drafts.length || pub?.error ? 'submit' : 'expandFiles';
    const label = unknown ? 'Сверить' : action === 'submit' ? 'Отправить ревью' : 'Следующий файл';
    const strip = K.ctx(`${E(m.id)} ${K.status(tone(m.state), compact ? '' : status(m.state))}${K.chip(`Черновики: ${m.drafts.length}`, { tone: m.drafts.length ? 'wait' : undefined })}${compact ? '' : `<span class="row-s">${unknown ? 'Нужна сверка' : pub?.error ? 'Нужно повторить итог' : m.review.revision.head_sha !== m.head ? 'Head изменился' : `${files(p).filter((f) => !viewed(m, f)).length} непросмотренных`}</span>`}`, button(compact ? '' : label, action, null, { icon: compact ? unknown ? 'refresh' : action === 'submit' ? 'send' : 'file' : undefined, tip: label, disabled: action === 'submit' && submitMuted(m) }));
      if (geo.height < 120) return strip;
    return `${strip}${sectionNav(p, false)}${sectionBody(p, geo, geo.width < 620)}`;
  }


  function focusDraft(p) {
    S.render();
    requestAnimationFrame(() => {
      const el = document.querySelector(`[data-panel="${p.id}"] [data-bind="draftBody"]`);
      if (el) { el.focus(); el.setSelectionRange(el.value.length, el.value.length); }
    });
  }
  function addDraft(p) {
    if (!isDiff(p) || !startAllowed(p)) return;
    const m = target(p), lines = flat(p), at = lines[p.state.line];
    const side = (p.state.side === 'old' ? at?.o : at?.n) != null ? p.state.side : (at?.o != null ? 'old' : 'new');
    p.state.side = side;
    const number = side === 'old' ? at?.o : at?.n;
    if (number == null) { p.state.error = 'На выбранной стороне нет этой строки. Выберите другую строку или сторону.'; S.render(); return; }
    const begin = p.state.selection == null ? p.state.line : p.state.selection;
    const range = lines.slice(Math.min(begin, p.state.line), Math.max(begin, p.state.line) + 1).map((l) => side === 'old' ? l.o : l.n).filter((n) => n != null);
    const start = Math.min(...range), end = Math.max(...range);
    const d = { id: uid('draft'), file: currentFile(p), start, line: end, range: start === end ? String(end) : `${start}–${end}`, side, head: m.review.revision.head_sha, anchor: anchor(m, m.review.revision, currentFile(p), end, side, start), body: '', saved: false, by: null };
    m.drafts.push(d); p.state.editing = d.id; p.state.selection = null; p.state.error = ''; persist(); focusDraft(p);
  }
  function editDraft(p, id) {
    if (!startAllowed(p)) return;
    const m = target(p), d = m.drafts.find((x) => x.id === id); if (!d) return;
    openFile(p, d.file); p.state.editing = d.id; p.state.side = d.side; focusDraft(p);
  }
  function saveDraft(p) {
    const m = target(p), d = m?.drafts.find((x) => x.id === p.state.editing); if (!d) return;
    if (!d.body.trim()) { p.state.error = 'Напишите комментарий или удалите пустой черновик.'; S.render(); return; }
    d.saved = true; p.state.editing = null; p.state.error = ''; persist();
    if (S.isField(document.activeElement)) document.activeElement.blur(); S.render();
  }
  function nextFile(p, delta) {
    const m = target(p); if (!m) return;
    const list = files(p), at = list.indexOf(currentFile(p));
    for (let n = 1; n <= list.length; n++) {
      const f = list[(at + delta * n + list.length * 2) % list.length];
      if (!viewed(m, f)) { openFile(p, f); return; }
    }
    p.state.error = 'Все файлы просмотрены. Можно отправить ревью.'; S.render();
  }
  function move(p, delta) {
    if (isDiff(p)) {
      const rows = p.state.renderRows || renderRows(diffData(p), false), lines = flat(p);
      const i = rows.findIndex((r) => r.indices.includes(p.state.line)), next = rows[S.clamp(i + delta, 0, rows.length - 1)];
      if (next) p.state.line = next.indices.find((index) => p.state.side === 'old' ? lines[index].o != null : lines[index].n != null) ?? next.indices[0];
    } else if (!target(p)) p.state.cur = S.clamp(p.state.cur + delta, 0, Math.max(0, targets(p).length - 1));
    else if (p.state.section === 'overview') { const box = document.querySelector(`.panel[data-panel="${p.id}"] .scroll`); if (box) box.scrollTop += delta * 40; return; }
    else if (p.state.section === 'threads') p.state.cur = S.clamp(p.state.cur + delta, 0, Math.max(0, threads(p).length - 1));
    else { const list = files(p); p.state.file = list[S.clamp(list.indexOf(currentFile(p)) + delta, 0, list.length - 1)]; }
    S.render();
  }
  function changeHunk(p, delta) {
    const lines = flat(p); const starts = lines.map((l, i) => l.t !== ' ' && (i === 0 || lines[i - 1].t === ' ' || lines[i - 1].hi !== l.hi) ? i : -1).filter((i) => i >= 0);
    const next = delta > 0 ? starts.find((i) => i > p.state.line) ?? starts[0] : [...starts].reverse().find((i) => i < p.state.line) ?? starts[starts.length - 1];
    if (next != null) { p.state.line = next; p.state.side = lines[next].t === '-' ? 'old' : 'new'; } S.render();
  }
  function syncHead(m) {
    const r = m.review;
    if (!r.revisions.some((v) => v.head_sha === m.head)) r.revisions.push(freshRevision(m));
    return r.revisions.find((v) => v.head_sha === m.head);
  }
  function followHead(p) {
    const m = target(p); if (!m) return;
    m.review.revision = copy(syncHead(m)); m.review.stay = false;
    m.review.viewedByRevision[m.head] = m.review.viewedByRevision[m.head] || {};
    m.viewed = m.review.viewedByRevision[m.head];
    p.state.line = 0; p.state.selection = null; p.state.error = ''; persist(); S.render();
  }
  function reanchorSheet(p) {
    const m = target(p); if (!m) return;
    const revision = syncHead(m), outdated = m.drafts.filter((d) => stale(m, d));
    if (!outdated.length) { followHead(p); return; }
    const st = { i: 0, choices: {}, error: '' };
    const item = () => outdated[st.i];
    const options = () => K.flat({ hunks: revision.diffs[item().file] || [] }).filter((l) => (item().side === 'old' ? l.o : l.n) != null);
    const pick = (line) => { st.choices[item().id] = { start: line, end: line }; st.error = ''; S.render(); };
    sheet({ icon: 'pin', title: 'Перепривязать черновики', width: 720, state: st,
      sub: `Выберите строки вручную в ${m.head}. До подтверждения исходные якоря сохраняются.`,
      body: () => `${K.sec(`Комментарий ${st.i + 1} из ${outdated.length}`)}${draftCard(m, item(), false)}${K.ctx(button('Предыдущий', 'prev'), button('Следующий', 'next'))}${K.sec(`Новая позиция: ${E(item().file)}, ${item().side === 'old' ? 'старая сторона' : 'новая сторона'}`)}${K.table({ cols: [{ t: 'Строка', w: '70px' }, { t: 'Код' }], rows: options().map((l) => { const n = item().side === 'old' ? l.o : l.n; return { on: st.choices[item().id]?.end === n, do: 'pick', arg: n, cells: [n, E(l.s)] }; }) })}${field('Начало диапазона', 'start', st.choices[item().id]?.start || '')}${field('Конец диапазона', 'end', st.choices[item().id]?.end || '')}${st.error ? K.banner('err', E(st.error)) : ''}${K.pad(`Привязано: ${Object.keys(st.choices).length} из ${outdated.length}`)}`,
      on: { pick: (a) => pick(+a), prev: () => { st.i = Math.max(0, st.i - 1); }, next: () => { st.i = Math.min(outdated.length - 1, st.i + 1); } },
      onBind: (name, value) => { const c = st.choices[item().id] || (st.choices[item().id] = {}); if (name === 'start' || name === 'end') c[name] = Number(value); S.render(); },
      onKey: (_, tok) => { if (tok === 'j' || tok === 'k') { const opts = options(), current = st.choices[item().id]?.end; const i = opts.findIndex((l) => (item().side === 'old' ? l.o : l.n) === current); const l = opts[S.clamp(i + (tok === 'j' ? 1 : -1), 0, opts.length - 1)]; if (l) pick(item().side === 'old' ? l.o : l.n); return true; } if (tok === ']' || tok === '[') { st.i = S.clamp(st.i + (tok === ']' ? 1 : -1), 0, outdated.length - 1); S.render(); return true; } return false; },
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Перепривязать', kind: 'primary', close: false, disabled: () => outdated.some((d) => !st.choices[d.id]?.start || !st.choices[d.id]?.end), run: (o) => {
        if (revision.head_sha !== m.head) { st.error = 'Head снова изменился. Закройте окно и начните перепривязку заново.'; S.render(); return; }
        for (const d of outdated) {
          const c = st.choices[d.id], nums = K.flat({ hunks: revision.diffs[d.file] || [] }).map((l) => d.side === 'old' ? l.o : l.n);
          if (c.start > c.end || !nums.includes(c.start) || !nums.includes(c.end)) { st.error = 'Диапазон отсутствует на выбранной стороне. Выберите начало и конец из diff.'; S.render(); return; }
        }
        outdated.forEach((d) => { const c = st.choices[d.id]; d.previousAnchor = copy(d.anchor); Object.assign(d, { head: m.head, start: c.start, line: c.end, range: c.start === c.end ? String(c.end) : `${c.start}–${c.end}`, anchor: anchor(m, revision, d.file, c.end, d.side, c.start) }); });
        followHead(p); persist(); S.ov.pop(o);
      } }],
    });
  }
  function browserSheet(p) {
    const m = target(p), r = m?.review; if (!r?.browser) return;
    const b = r.browser;
    const choose = (value, o) => {
      r.browserChoice = value;
      if (value === 'adopt') {
        b.comments.forEach((c) => { if (!m.drafts.some((d) => d.remoteId === c.remoteId)) m.drafts.push(copy(c)); });
        if (b.summary && !r.summary) r.summary = b.summary;
        r.pendingReviewId = m.provider === 'github' ? b.id : null;
      }
      persist(); S.ov.pop(o); S.render();
    };
    sheet({ icon: 'draft', title: 'Черновик из браузера', width: 660,
      sub: `${m.provider === 'github' ? 'Pending review' : 'Draft notes'} / ${b.id}. Весь состав показан ниже.`,
      body: () => `${K.pad(paragraph(b.summary || 'Без сводки'))}${b.comments.map((d) => draftCard(m, d, false)).join('')}${m.provider === 'github' ? K.banner('info', 'Один pending review на аккаунт', 'При локальной работе он сохранится. Перед публикацией нужно принять его или завершить в браузере.') : K.banner('info', 'Публикуются только выбранные черновики', 'Браузерные draft notes вне этой сессии останутся на сервере.')}`,
      actions: [{ label: 'Закрыть', key: 'Esc' }, { label: 'Работать локально', key: 'l', close: false, run: (o) => choose('local', o) }, { label: 'Продолжить в нём', kind: 'primary', close: false, run: (o) => choose('adopt', o) }],
    });
  }
  function parseURL(value) {
    try {
      const u = new URL(value.trim()); if (!['https:', 'http:'].includes(u.protocol)) return null;
      const path = u.pathname.replace(/\/$/, ''), hit = path.match(/^(.*)\/-\/merge_requests\/([^/]+)$/) || path.match(/^(.*)\/pull\/([^/]+)$/);
      if (!hit) return null;
      const provider = path.includes('/-/merge_requests/') ? 'gitlab' : 'github';
      const known = Object.values(W.mrs).find((m) => { const v = new URL(m.url); return S.repoWs(m.repo) === S.st.route.ws && v.host === u.host && v.pathname.replace(/\/$/, '') === path; });
      return { url: `${u.origin}${path}`, provider, nativeId: hit[2], repoPath: hit[1], known };
    } catch (_) { return null; }
  }
  function urlSheet() {
    const st = { url: '', error: '' };
    sheet({ icon: 'link', title: 'Открыть по ссылке', width: 580, state: st,
      body: () => {
        const parsed = parseURL(st.url), m = parsed?.known;
        return field('URL merge request или pull request', 'url', st.url, null, true) + (m ? K.pad(K.kv([['Цель', `${E(m.id)} ${E(m.title)}`], ['Провайдер', E(m.provider)], ['Автор', E(m.author)], ['Ветки', `${E(m.branch)} / ${E(m.target)}`], ['Head', E(m.head)]])) : st.url ? K.banner('wait', parsed ? 'Цель отсутствует в данных прототипа' : 'Ссылка не распознана', parsed ? 'Выберите цель из списка или создайте новую.' : 'Используйте URL GitLab /-/merge_requests/id или GitHub /pull/id.') : K.pad('Вставьте ссылку, чтобы увидеть предпросмотр.'));
      }, onBind: () => S.render(), actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Открыть', kind: 'primary', disabled: () => !parseURL(st.url)?.known, run: () => openTarget(parseURL(st.url).known.id) }],
    });
  }
  function replySheet(p, id) {
    if (!startAllowed(p)) return;
    const m = target(p), t = m.threads.find((x) => x.id === id) || currentThread(p); if (!t) return;
    m.review.replyTexts = m.review.replyTexts || {}; const st = { text: m.review.replyTexts[t.id] || '' };
    sheet({ icon: 'review', title: 'Ответить в тред', width: 580, state: st,
      body: () => `${K.pad(paragraph(t.body))}${field('Ответ', 'text', st.text, 4, true)}`,
      onBind: (_, value) => { m.review.replyTexts[t.id] = value; persist(); S.render(); },
      actions: [{ label: 'Закрыть', key: 'Esc' }, { label: 'Готово', kind: 'primary', disabled: () => !st.text.trim(), run: () => {
        const revision = m.review.revisions.find((v) => v.head_sha === t.head) || m.review.revision;
        m.drafts.push({ id: uid('reply'), threadId: t.id, file: t.file, line: t.line, start: t.line, range: String(t.line), side: t.side, head: t.head, body: st.text, saved: true, anchor: anchor(m, revision, t.file, t.line, t.side), by: null });
        delete m.review.replyTexts[t.id]; persist(); S.render();
      } }],
    });
  }
  function resolveSheet(p, id) {
    const m = target(p), t = m?.threads.find((x) => x.id === id) || currentThread(p); if (!t) return;
    sheet({ icon: 'review', title: t.resolved ? 'Открыть тред снова' : 'Разрешить тред', body: () => `${K.pad(paragraph(t.body))}${!m.review.capabilities.write ? denied(m) : ''}`,
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: t.resolved ? 'Открыть снова' : 'Resolve', kind: 'primary', disabled: () => !m.review.capabilities.write, run: () => { if (!m.review.capabilities.write) return; t.resolved = !t.resolved; persist(); S.render(); } }],
    });
  }
  function publishReason(m, st) {
    const r = m.review, pub = r.publication;
    if (r.paused) return pausedText(m);
    if (!r.capabilities.write) return 'Нет прав на отправку. Проверьте доступ к репозиторию.';
    if (st.checking) return 'Проверяем свежий head и якоря…';
    if (st.checkedHead !== m.head) return 'Head изменился после проверки. Проверьте его снова.';
    if (r.revision.head_sha !== m.head || m.drafts.some((d) => stale(m, d))) return 'Есть якоря прошлой ревизии. Перепривяжите черновики перед отправкой.';
    if (pub && !pub.done && pub.head !== m.head) return 'План относится к прошлому head. Завершите оставшиеся операции в браузере.';
    if (pub?.running) return 'Публикация выполняется.';
    if (pub?.steps.some((s) => s.state === 'unknown')) return 'Результат неизвестен. Сначала выполните сверку.';
    if (r.browser && !r.browserChoice) return 'Сначала выберите, как работать с браузерным черновиком.';
    if (m.provider === 'github' && r.browser && r.browserChoice !== 'adopt') return 'В браузере есть pending review. Примите его перед отправкой.';
    if (r.outcome === 'REQUEST_CHANGES' && !r.capabilities.requestChanges) return 'Провайдер не поддерживает Request changes для этого аккаунта.';
    if (r.outcome === 'APPROVE' && !r.capabilities.approve) return 'Нет права формального одобрения.';
    if (m.drafts.some((d) => !d.body.trim() || !d.saved)) return 'Сохраните все комментарии и удалите пустые черновики.';
    if (!m.drafts.length && !r.summary.trim() && r.outcome === 'COMMENT' && (!pub || pub.done)) return 'Добавьте комментарий или сводку.';
    if (W.review.storageError) return W.review.storageError;
    return '';
  }
  function stepRows(pub) {
    const names = { planned: 'Ожидает', in_flight: 'Отправляется', confirmed: 'Готово', failed: 'Ошибка', unknown: 'Неизвестно' };
    return K.table({ cols: [{ t: 'Шаг' }, { t: 'Состояние', w: '120px' }], rows: pub.steps.map((s) => ({ cells: [E(s.label), K.chip(names[s.state] || E(s.state), { tone: s.state === 'confirmed' ? 'ok' : s.state === 'unknown' || s.state === 'failed' ? 'err' : s.state === 'in_flight' ? 'info' : undefined })] })) });
  }
  function submitSheet(p) {
    const m = target(p); if (!m) { const selected = targets(p)[p.state.cur]; if (selected) submitSheet(openTarget(selected.id)); return; }
    if (submitMuted(m)) { S.notify({ title: 'Операции с сервисом приостановлены', text: pausedText(m), tone: 'wait', action: { label: 'Настройки', run: () => S.run('settings') } }); return; }
    if (m.review.browser && !m.review.browserChoice) { browserSheet(p); return; }
    const r = m.review, st = { checking: true, checkedHead: null };
    const check = () => {
      st.checking = true; S.render();
      setTimeout(() => { syncHead(m); st.checking = false; st.checkedHead = m.head; st.checkedAt = new Date().toLocaleTimeString('ru-RU'); S.render(); }, 320);
    };
    const finished = () => !!st.started && !!r.publication?.done;
    const doneView = () => {
      const pub = r.publication;
      return K.banner('ok', `Итог: ${E(outcomeLabels[pub.outcome])}`, `Ревизия ${E(pub.head)}. Подтверждено шагов: ${pub.steps.length}.`) +
        K.sec('Комментарии', pub.comments.length) + (pub.comments.map((d) => K.row({ k: d.id, icon: 'review', title: E(d.body), sub: `${E(d.file)}:${E(d.range || d.line)}` })).join('') || K.pad('Построчных комментариев нет.')) +
        (pub.summary ? K.sec('Сводка') + K.pad(paragraph(pub.summary)) : '') +
        K.pad(button(st.steps ? 'Скрыть шаги' : 'Показать шаги', 'toggleSteps')) + (st.steps ? stepRows(pub) : '');
    };
    const overlay = sheet({ icon: 'send', title: () => finished() ? `Ревью ${m.id} отправлено` : `Отправить ревью ${m.id}`, sub: `${E(m.repo)}, ${E(m.branch)} → ${E(m.target)}`, width: 760, state: st,
      body: () => {
        if (finished()) return doneView();
        const pub = r.publication, reason = publishReason(m, st), locked = pub && !pub.done;
        const comments = locked ? pub.comments : m.drafts;
        return K.banner(st.checking ? 'info' : st.checkedHead === m.head && r.revision.head_sha === m.head ? 'ok' : 'wait', st.checking ? 'Проверяем head' : `Свежий head: ${E(st.checkedHead)}`, st.checking ? 'Получаем согласованный набор refs.' : `Проверено в ${E(st.checkedAt)}. Ревью закреплено на ${E(r.revision.head_sha)}.`, button('Проверить снова', 'check')) +
          (reason && !st.checking ? K.banner('wait', E(reason)) : '') + (r.revision.head_sha !== m.head || m.drafts.some((d) => stale(m, d)) ? K.pad(button('Перепривязать', 'reanchor')) : '') +
          K.sec(`Комментарии: ${comments.length}`) + (comments.map((d) => draftCard(m, d, false)).join('') || K.pad('Построчных комментариев нет.')) +
          K.sec('Итог') + K.pad(Object.entries(outcomeLabels).map(([value, label]) => button(label, 'outcome', value, { kind: (locked ? pub.outcome : r.outcome) === value ? 'primary' : undefined, disabled: !!locked || !r.capabilities.write || value === 'REQUEST_CHANGES' && !r.capabilities.requestChanges || value === 'APPROVE' && !r.capabilities.approve })).join(' ')) +
          (!r.capabilities.requestChanges ? K.banner('wait', 'Request changes недоступен', m.provider === 'gitlab' ? 'На этом сервере недоступна mergeRequestRequestChanges. Выберите другой итог; комментарий не заменяет запрос изменений.' : 'У аккаунта нет права запрашивать изменения.') : '') +
          (locked ? K.pad(paragraph(pub.summary || 'Без сводки')) : field('Сводка ревью', 'summary', r.summary, 3)) +
          K.pad(m.provider === 'gitlab' ? 'Комментарии публикуются по отдельности. Сводка и итог отправляются следующими шагами.' : r.pendingReviewId ? 'Комментарии сохраняются в принятом pending review. Затем отправляется именно это ревью.' : 'Комментарии и выбранный итог отправляются одним ревью.') +
          (pub ? `${K.sec('Публикация')}${stepRows(pub)}${pub.error ? K.banner('err', E(pub.error)) : ''}${pub.steps.some((s) => s.state === 'unknown') ? K.pad(button('Сверить результат', 'reconcile')) : ''}` : '') +
          (r.browser && r.browserChoice === 'local' && m.provider === 'github' ? K.pad(button('Принять браузерный черновик', 'browserDraft')) : '');
      },
      on: { check, toggleSteps: () => { st.steps = !st.steps; }, outcome: (value) => { if (!r.publication || r.publication.done) { if (value !== 'REQUEST_CHANGES' || r.capabilities.requestChanges) r.outcome = value; persist(); } }, reanchor: (_, o) => { S.ov.pop(o); reanchorSheet(p); }, reconcile: () => reconcile(m), browserDraft: (_, o) => { S.ov.pop(o); browserSheet(p); } },
      onBind: (name, value) => { if (name === 'summary' && (!r.publication || r.publication.done)) { r.summary = value; persist(); } S.render(); },
      onKey: (_, tok) => { if (['c', 'a', 'r'].includes(tok) && !finished() && (!r.publication || r.publication.done)) { const value = { c: 'COMMENT', a: 'APPROVE', r: 'REQUEST_CHANGES' }[tok]; if (value !== 'REQUEST_CHANGES' || r.capabilities.requestChanges) r.outcome = value; persist(); S.render(); return true; } return false; },
      actions: [{ label: 'Закрыть', key: 'Esc', hide: finished }, { label: () => (r.publication && !r.publication.done ? 'Отправить оставшееся' : 'Опубликовать ревью'), kind: 'primary', close: false, hide: finished, disabled: () => !!publishReason(m, st), run: () => { if (!publishReason(m, st)) { st.started = true; publish(m); } } }, { label: 'Готово', kind: 'primary', hide: () => !finished() }],
    }); overlay.reviewSubmit = m.id; check();
  }
  function makePlan(m) {
    const r = m.review, comments = copy(m.drafts), pub = { id: uid('publication'), head: m.head, refs: copy(refs(m)), account: r.account, host: r.host, repository: m.repo, comments, summary: r.summary, outcome: r.outcome, steps: [], receipts: {}, running: false, done: false, error: '' };
    const add = (kind, label, ids, payload) => pub.steps.push({ id: uid('step'), kind, label, draftIds: ids || [], payload, state: 'planned', history: ['planned'] });
    if (m.provider === 'github' && !r.pendingReviewId) add('create_review', 'Создать и отправить ревью', comments.map((d) => d.id), { commit_id: m.head, event: r.outcome, body: r.summary, comments: comments.map((d) => ({ ...d.anchor, body: d.body, in_reply_to: d.threadId || undefined })) });
    else if (m.provider === 'github') {
      comments.forEach((d) => add('sync_comment', `Сохранить ${d.file}:${d.range}`, [d.id], { review_id: r.pendingReviewId, comment_id: d.remoteId || null, ...d.anchor, body: d.body }));
      add('submit_review', 'Отправить pending review', comments.map((d) => d.id), { review_id: r.pendingReviewId, event: r.outcome, body: r.summary });
    } else {
      comments.forEach((d) => {
        if (!d.remoteId) add('create_note', `Сохранить draft note ${d.file}:${d.range}`, [d.id], { position: d.anchor, note: d.body, discussion_id: d.threadId || null });
        add('publish_note', `Опубликовать ${d.file}:${d.range}`, [d.id], { draft_note_id: d.remoteId || null });
      });
      if (r.summary.trim()) add('summary', 'Опубликовать сводку', [], { body: r.summary });
      if (r.outcome !== 'COMMENT') add('outcome', r.outcome === 'APPROVE' ? 'Одобрить текущий SHA' : 'Запросить изменения', [], { operation: r.outcome === 'APPROVE' ? 'approve' : 'mergeRequestRequestChanges', sha: m.head });
      if (!pub.steps.length) add('summary', 'Опубликовать сводку', [], { body: r.summary });
    }
    return pub;
  }
  function confirmStep(m, pub, step, remoteId) {
    if (step.state === 'confirmed') return;
    step.state = 'confirmed'; step.history.push('confirmed'); step.remoteId = String(remoteId);
    if (step.kind === 'create_note' || step.kind === 'sync_comment') {
      step.draftIds.forEach((id) => { const d = m.drafts.find((v) => v.id === id), snapshot = pub.comments.find((v) => v.id === id); if (d) d.remoteId = String(remoteId); if (snapshot) snapshot.remoteId = String(remoteId); });
      if (step.kind === 'create_note') { const publishStep = pub.steps.find((s) => s.kind === 'publish_note' && s.draftIds[0] === step.draftIds[0]); if (publishStep) publishStep.payload.draft_note_id = String(remoteId); }
    }
    if (['publish_note', 'create_review', 'submit_review'].includes(step.kind)) {
      step.draftIds.forEach((id) => {
        const d = pub.comments.find((v) => v.id === id); if (!d) return;
        const existing = m.threads.some((t) => t.sourceDraft === id || t.replies.some((v) => v.sourceDraft === id));
        if (!existing) {
          const parent = m.threads.find((t) => t.id === d.threadId);
          if (parent) parent.replies.push({ who: account(m)?.user || 'Вы', when: 'Сейчас', body: d.body, sourceDraft: id, remoteId: String(remoteId) });
          else m.threads.push({ id: uid('thread'), remoteId: `${remoteId}/${id}`, sourceDraft: id, file: d.file, line: d.line, start: d.start, side: d.side, head: d.head, body: d.body, who: account(m)?.user || 'Вы', when: 'Сейчас', resolved: false, replies: [] });
        }
        m.drafts = m.drafts.filter((v) => v.id !== id);
      });
    }
    if (['outcome', 'create_review', 'submit_review'].includes(step.kind)) {
      m.review.outcomeSent = pub.outcome;
      const who = account(m)?.user || 'Вы'; let reviewer = m.reviewers.find((r) => r.who === who);
      if (!reviewer) { reviewer = { who, state: 'you' }; m.reviewers.push(reviewer); }
      if (pub.outcome === 'APPROVE') reviewer.state = 'approved';
      if (pub.outcome === 'REQUEST_CHANGES') reviewer.state = 'changes_requested';
    }
    if (step.kind === 'submit_review') { m.review.pendingReviewId = null; m.review.browser = null; m.review.browserChoice = null; }
    persist(); S.render();
  }
  function finishPublication(m, pub) {
    if (!pub.done && pub.steps.every((s) => s.state === 'confirmed')) {
      pub.done = true; pub.running = false; pub.error = ''; m.review.summary = ''; m.review.lastReviewed = pub.head;
      m.review.publications.push({ id: pub.id, head: pub.head, outcome: pub.outcome, steps: copy(pub.steps) });
      if (!S.st.overlays.some((o) => o.reviewSubmit === m.id)) S.notify({ ws: S.repoWs(m.repo), project: S.repoProject(m.repo), title: `Ревью ${m.id} отправлено`, text: outcomeLabels[pub.outcome], tone: 'ok' });
    }
    persist(); S.render();
  }
  function runStep(m, pub) {
    if (W.mrs[m.id] !== m || m.review.publication !== pub || !pub.running) return;
    const step = pub.steps.find((s) => s.state !== 'confirmed');
    if (!step) { finishPublication(m, pub); return; }
    if (step.state !== 'planned') { pub.running = false; persist(); S.render(); return; }
    if (m.head !== pub.head || !m.review.capabilities.write || pub.outcome === 'REQUEST_CHANGES' && !m.review.capabilities.requestChanges || pub.outcome === 'APPROVE' && !m.review.capabilities.approve) {
      pub.running = false; pub.error = 'Head или права изменились. Подтверждённые шаги сохранены; проверьте цель перед продолжением.'; persist(); S.render(); return;
    }
    step.state = 'in_flight'; step.history.push('in_flight'); persist(); S.render();
    setTimeout(() => {
      if (W.mrs[m.id] !== m || m.review.publication !== pub) return;
      const final = ['outcome', 'create_review', 'submit_review'].includes(step.kind);
      if (final && (m.head !== pub.head || !m.review.capabilities.write || pub.outcome === 'APPROVE' && !m.review.capabilities.approve || pub.outcome === 'REQUEST_CHANGES' && !m.review.capabilities.requestChanges)) {
        step.state = 'planned'; pub.running = false; pub.error = 'Сервер отклонил итог: head или права изменились. Проверьте текущую ревизию.'; persist(); S.render(); return;
      }
      if (m.review.failOutcome && final) {
        m.review.failOutcome = false; step.state = 'planned'; step.history.push('rejected'); pub.receipts[step.id] = { absent: true };
        pub.running = false; pub.error = 'Сервер отклонил итог. Комментарии уже опубликованы. Повторите оставшийся итог после свежей проверки.'; persist(); S.render(); return;
      }
      if (m.review.timeoutNext) {
        const absent = m.review.timeoutNext === 'absent'; m.review.timeoutNext = false;
        pub.receipts[step.id] = absent ? { absent: true } : { remoteId: uid('remote') };
        step.state = 'unknown'; step.history.push('unknown'); pub.running = false; pub.error = 'Ответ не получен. Результат неизвестен; повтор заблокирован до сверки.'; persist(); S.render(); return;
      }
      const remoteId = step.payload?.comment_id || step.payload?.draft_note_id || uid('remote'); pub.receipts[step.id] = { remoteId };
      confirmStep(m, pub, step, remoteId); setTimeout(() => runStep(m, pub), 260);
    }, 440);
  }
  function publish(m) {
    const r = m.review;
    if (r.publication?.steps.some((s) => s.state === 'unknown') || r.publication?.running) return;
    if (!r.publication || r.publication.done) r.publication = makePlan(m);
    const pub = r.publication;
    if (pub.head !== m.head) { pub.error = 'План относится к прошлому head. Завершите ревью этой ревизии в браузере; новые комментарии сохраняются локально.'; S.render(); return; }
    pub.error = ''; pub.running = true; persist(); S.render(); setTimeout(() => runStep(m, pub), 300);
  }
  function reconcile(m) {
    const pub = m?.review.publication; if (!pub || pub.running || pub.reconciling || !pub.steps.some((s) => s.state === 'unknown')) return;
    pub.reconciling = true; pub.error = 'Сверяем результат с провайдером…'; persist(); S.render();
    setTimeout(() => {
      if (W.mrs[m.id] !== m || m.review.publication !== pub) return;
      pub.steps.filter((s) => s.state === 'unknown').forEach((s) => {
        const receipt = pub.receipts[s.id];
        if (receipt?.remoteId) confirmStep(m, pub, s, receipt.remoteId);
        else if (receipt?.absent) { s.state = 'planned'; s.history.push('absence_confirmed'); }
      });
      pub.reconciling = false;
      pub.error = pub.steps.some((s) => s.state === 'unknown') ? 'Провайдер ещё не подтвердил результат. Повтор остаётся заблокирован; выполните сверку позже.' : pub.steps.some((s) => s.state === 'planned') ? 'Сверка завершена. Повтор доступен только для оставшихся неподтверждённых операций.' : '';
      finishPublication(m, pub);
    }, 850);
  }

  function manage(p, kind) {
    const m = target(p); if (!m) return;
    const titles = { merge: 'Merge', squash: 'Squash', autoMerge: 'Auto-merge', draft: 'Перевести в Draft', ready: 'Отметить Ready', closeTarget: 'Закрыть цель', reopenTarget: 'Открыть цель снова' };
    const isMerge = ['merge', 'squash', 'autoMerge'].includes(kind);
    const head = m.head, destination = m.target;
    const reason = () => {
      if (m.review.paused) return pausedText(m);
      if (!m.review.capabilities.write) return 'Нет прав на изменение цели.';
      if (isMerge && !m.review.capabilities.merge) return 'Нет прав на слияние.';
      if (head !== m.head || destination !== m.target) return 'Head или целевая ветка изменились. Откройте подтверждение заново.';
      if (isMerge && m.state !== 'opened') return 'Для слияния цель должна быть открыта.';
      if (['merge', 'squash'].includes(kind) && blockers(m).length) return 'Сначала устраните препятствия.';
      if (kind === 'closeTarget' && m.state !== 'opened' || kind === 'reopenTarget' && m.state !== 'closed' || ['draft', 'ready'].includes(kind) && m.state !== 'opened') return 'Действие недоступно в текущем состоянии.';
      return '';
    };
    const apply = () => {
      if (reason()) return;
      if (kind === 'merge' || kind === 'squash') { m.state = 'merged'; m.mergeMethod = kind; m.mergedHead = head; m.autoMerge = null; }
      if (kind === 'autoMerge') {
        if (m.autoMerge) m.autoMerge = null;
        else if (!blockers(m).length) { m.state = 'merged'; m.mergeMethod = 'auto'; m.mergedHead = head; }
        else m.autoMerge = { head, target: destination };
      }
      if (kind === 'draft' || kind === 'ready') m.draft = kind === 'draft';
      if (kind === 'closeTarget' || kind === 'reopenTarget') m.state = kind === 'closeTarget' ? 'closed' : 'opened';
      persist(); S.render();
    };
    if (!isMerge) {
      const why = reason();
      if (why) { S.notify({ title: why, tone: 'wait' }); return; }
      apply();
      const opposite = { draft: 'ready', ready: 'draft', closeTarget: 'reopenTarget', reopenTarget: 'closeTarget' }[kind];
      const done = { draft: 'Переведено в Draft', ready: 'Отмечено Ready', closeTarget: 'Закрыто', reopenTarget: 'Открыто снова' }[kind];
      S.notify({ ws: S.repoWs(m.repo), project: S.repoProject(m.repo), title: `${m.id}: ${done}`, action: { label: 'Вернуть', run: () => manage(p, opposite) } });
      return;
    }
    sheet({ icon: isMerge ? 'merge' : 'review', title: `${titles[kind]} ${m.id}`, width: 550,
      body: () => `${K.pad(K.kv([['Цель', E(m.title)], ['Исходная ветка', E(m.branch)], ['Целевая ветка', E(destination)], ['Head', E(head)], ['Состояние', status(m.state)]]))}${isMerge ? `${K.sec('Препятствия')}${blockers(m).map((b) => K.row({ icon: 'warn', title: E(b) })).join('') || K.banner('ok', 'Все условия выполнены')}${kind === 'squash' ? K.pad(`${m.commits.length} коммитов будут объединены в один.`) : ''}${kind === 'autoMerge' ? K.pad(m.autoMerge ? 'Автослияние включено. Можно отменить его или проверить условия в обзоре.' : 'Слияние произойдёт, когда проверки, одобрения и треды позволят его выполнить.') : ''}` : ''}${reason() ? K.banner('wait', E(reason())) : ''}`,
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: kind === 'autoMerge' && m.autoMerge ? 'Отключить автослияние' : titles[kind], kind: 'primary', disabled: () => !!reason(), run: apply }],
    });
  }
  function metadataSheet(p, kind) {
    const m = target(p); if (!m) return;
    const selected = kind === 'reviewers' ? m.reviewers.map((v) => v.who) : [...m[kind]];
    const options = kind === 'labels' ? [...new Set([...m.labels, ...Object.values(W.mrs).filter((v) => S.repoWs(v.repo) === S.repoWs(m.repo)).flatMap((v) => v.labels)])] : [...W.people];
    const st = { selected, custom: '', i: 0 };
    const toggle = (value) => { st.selected = st.selected.includes(value) ? st.selected.filter((v) => v !== value) : [...st.selected, value]; S.render(); };
    sheet({ icon: kind === 'labels' ? 'label' : 'user', title: { reviewers: 'Ревьюеры', assignees: 'Assignees', labels: 'Метки' }[kind], width: 480, state: st,
      body: () => `${!m.review.capabilities.write ? denied(m) : ''}${options.map((v, i) => K.row({ on: i === st.i, lead: K.check(st.selected.includes(v)), title: E(v), do: 'toggle', arg: v })).join('')}${field(kind === 'labels' ? 'Новая метка' : 'Имя участника', 'custom', st.custom)}${button('Добавить', 'add')}${K.pad(st.selected.map((v) => K.chip(E(v))).join(' '))}`,
      on: { toggle, add: () => { const v = st.custom.trim(); if (v) { if (!options.includes(v)) options.push(v); if (!st.selected.includes(v)) st.selected.push(v); st.custom = ''; } } }, onBind: () => S.render(),
      onKey: (_, tok) => { if (tok === 'j' || tok === 'k') { st.i = S.clamp(st.i + (tok === 'j' ? 1 : -1), 0, options.length - 1); S.render(); return true; } if (tok === 'x') { toggle(options[st.i]); return true; } return false; },
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Сохранить', kind: 'primary', disabled: () => !m.review.capabilities.write, run: () => { if (!m.review.capabilities.write) return; m[kind] = kind === 'reviewers' ? st.selected.map((who) => m.reviewers.find((r) => r.who === who) || { who, state: 'pending' }) : st.selected; persist(); S.render(); } }],
    });
  }
  function createSheet() {
    const st = { repo: S.repo().id, title: '', description: '', branch: S.wb().branch, target: 'main', draft: true, error: '' };
    const ws = S.st.route.ws;
    const repositories = S.project().repos.map((id) => W.repos[id]);
    const problem = () => S.repoWs(st.repo) !== ws ? 'Репозиторий больше не в этом воркспейсе.' : !st.title.trim() ? 'Напишите название.' : !st.branch.trim() || !st.target.trim() ? 'Укажите обе ветки.' : st.branch.trim() === st.target.trim() ? 'Исходная и целевая ветки совпадают.' : account({ provider: W.repos[st.repo].provider, repo: st.repo })?.state !== 'ok' ? 'Аккаунт недоступен. Проверьте настройки.' : '';
    sheet({ icon: 'plus', title: 'Создать MR или PR', width: 620, state: st,
      body: () => `${K.sec('Репозиторий')}${K.pad(repositories.length > 1 ? K.seg(repositories.map((r) => ({ label: E(r.id), on: st.repo === r.id, do: 'repo', arg: r.id }))) : E(repositories[0].id))}${field('Название', 'title', st.title, null, true)}${field('Описание', 'description', st.description, 4)}${field('Исходная ветка', 'branch', st.branch)}${field('Целевая ветка', 'target', st.target)}${K.pad(button(st.draft ? 'Draft' : 'Ready', 'draft'))}${K.pad(W.repos[st.repo].provider === 'gitlab' ? 'Будет создан merge request.' : 'Будет создан pull request.')}${st.error ? K.banner('err', E(st.error)) : ''}`,
      on: { repo: (id) => { st.repo = id; st.branch = W.repos[id].branch; }, draft: () => { st.draft = !st.draft; } }, onBind: () => S.render(),
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Создать', kind: 'primary', close: false, run: (o) => {
        st.error = problem(); if (st.error) { S.render(); return; }
        const repo = W.repos[st.repo], nativeId = String(1000 + W.review.next++), prefix = repo.provider === 'gitlab' ? '!' : '#', id = prefix + nativeId;
        const existing = Object.values(W.mrs).find((m) => m.repo === repo.id), root = existing ? new URL(existing.url).origin + (repo.provider === 'gitlab' ? new URL(existing.url).pathname.split('/-/merge_requests/')[0] : new URL(existing.url).pathname.split('/pull/')[0]) : `https://${repo.host}/${account({ repo: repo.id, provider: repo.provider })?.user || 'user'}/${repo.id}`;
        W.mrs[id] = { id, nativeId, provider: repo.provider, repo: repo.id, title: st.title.trim(), description: st.description, author: account({ repo: repo.id, provider: repo.provider })?.user || 'Вы', branch: st.branch.trim(), target: st.target.trim(), state: 'opened', draft: st.draft, head: repo.head, base: existing?.base || repo.head, commits: [repo.head], files: existing ? [...existing.files] : [], viewed: {}, threads: [], drafts: [], reviewers: [], assignees: [], labels: [], pipeline: null, url: `${root}${repo.provider === 'gitlab' ? '/-/merge_requests/' : '/pull/'}${nativeId}` };
        ensure(W.mrs[id]); W.mrList.unshift(id); persist(); S.ov.pop(o); openTarget(id);
      } }],
    });
  }
  function checkoutSheet(p) {
    const m = target(p); if (!m) return;
    const choices = Object.values(W.workbenches).filter((w) => w.repo === m.repo), st = { selected: 'new', path: `${W.repos[m.repo].path}-review-${m.id.slice(1)}`, error: '' };
    sheet({ icon: 'branch', title: `Checkout ${m.branch}`, width: 580, state: st,
      body: () => `${K.pad(K.kv([['Ветка', E(m.branch)], ['Репозиторий', E(m.repo)], ['Head', E(m.head)]]))}${K.row({ on: st.selected === 'new', lead: K.radio(st.selected === 'new'), title: 'Новый worktree', do: 'select', arg: 'new' })}${choices.map((w) => K.row({ on: st.selected === w.id, lead: K.radio(st.selected === w.id), title: E(w.title), sub: `${E(w.worktree)} / ${E(w.branch)}`, do: 'select', arg: w.id })).join('')}${st.selected === 'new' ? field('Путь worktree', 'path', st.path) : K.banner('wait', 'Ветка выбранной рабочей области изменится', 'При незакоммиченных изменениях checkout будет заблокирован.')}${st.error ? K.banner('err', E(st.error)) : ''}`,
      on: { select: (id) => { st.selected = id; st.error = ''; } }, onBind: () => S.render(),
      onKey: (_, tok) => { if (tok === 'j' || tok === 'k') { const ids = ['new', ...choices.map((w) => w.id)]; st.selected = ids[S.clamp(ids.indexOf(st.selected) + (tok === 'j' ? 1 : -1), 0, ids.length - 1)]; S.render(); return true; } return false; },
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Checkout', kind: 'primary', close: false, run: (o) => {
        if (st.selected !== 'new' && st.selected === S.st.route.wb && W.changes.files.length) { st.error = 'В рабочей копии есть изменения. Создайте новый worktree или сначала сохраните их.'; S.render(); return; }
        if (st.selected === 'new' && (!st.path.trim() || Object.values(W.workbenches).some((w) => w.worktree === st.path.trim()))) { st.error = 'Укажите новый, ещё не занятый путь worktree.'; S.render(); return; }
        let wb = W.workbenches[st.selected];
        if (!wb) {
          const project = Object.values(W.projects).find((v) => v.repos.includes(m.repo)), id = uid('review-wt');
          wb = { id, title: `review-${m.id}`, project: project.id, repo: m.repo, branch: m.branch, worktree: st.path.trim() };
          W.workbenches[id] = wb; project.workbenches.push(id);
        } else wb.branch = m.branch;
        m.review.checkout = { workbench: wb.id, worktree: wb.worktree, branch: m.branch, head: m.head }; persist(); S.ov.pop(o);
        S.notify({ ...S.owner(wb.id), title: 'Ветка открыта в worktree', text: `${m.branch}, ${wb.worktree}`, action: { label: 'Перейти', run: () => S.openWb(wb.id) } });
      } }],
    });
  }
  function checksSheet(p, id) {
    const m = target(p), run = W.pipelines.find((r) => String(r.id) === String(id || m?.pipeline)); if (!run) return;
    sheet({ icon: 'pipeline', title: `Проверки ${run.id}`, width: 580,
      body: () => `${K.pad(K.kv([['SHA', E(run.sha)], ['Ветка', E(run.ref)], ['Состояние', status(run.state)], ['Попытка', run.attempt]]))}${(W.jobs[run.id] || []).map((s) => K.sec(E(s.stage)) + s.jobs.map((j) => K.row({ title: E(j.name), st: tone(j.state), trail: E(j.dur) })).join('')).join('')}`,
      actions: [{ label: 'Закрыть', key: 'Esc' }, { label: 'Открыть пайплайны', kind: 'primary', run: () => { if (S.plugins.pipelines) S.open('pipelines', { run: run.id, pipeline: run.id }); } }],
    });
  }
  function commitSheet(p, sha) {
    const m = target(p), c = W.commits[sha]; if (!m) return;
    const list = c?.files || m.files, st = { cur: 0 };
    const open = (file, o) => { if (!file) return; S.ov.pop(o); p.filter = ''; openFile(p, file); };
    sheet({ icon: 'commit', title: c?.title || sha, width: 600, state: st,
      body: () => `${K.pad(K.kv([['SHA', E(sha)], ['Автор', E(c?.author || m.author)], ['Родитель', E(c?.parent || m.base)]]))}${K.pad(paragraph(c?.body || m.description))}${list.map((file, i) => K.row({ on: i === st.cur, title: E(file), icon: 'file', do: 'file', arg: file })).join('')}`,
      on: { file: open },
      onKey: (_, tok) => { if (tok === 'j' || tok === 'k') { st.cur = S.clamp(st.cur + (tok === 'j' ? 1 : -1), 0, list.length - 1); S.render(); return true; } return false; },
      actions: [{ label: 'Закрыть', key: 'Esc' }, { label: 'Открыть файл', kind: 'primary', close: false, disabled: () => !list.length, run: (o) => open(list[st.cur], o) }],
    });
  }
  function editorFile(p) {
    const m = target(p), file = currentFile(p), l = flat(p)[p.state.line]; if (!file) return;
    const revision = m.review.revision, old = p.state.side === 'old', sha = old ? revision.base_sha : revision.head_sha;
    const line = (old ? l?.o : l?.n) || 1;
    const buffer = W.buffers[file] || (W.buffers[file] = { lines: [], origin: [], lang: null });
    const contents = [...buffer.lines];
    const canonical = m.review.revisions.find((r) => r.diffs[file]?.length)?.diffs[file] || [];
    const hunks = old ? canonical : revision.diffs[file] || [];
    if (old || !hunks.length) {
      [...canonical].reverse().forEach((h) => {
        const start = h.lines.find((v) => v.n != null)?.n || 1;
        const before = h.lines.filter((v) => v.t !== '+').map((v) => v.s);
        contents.splice(start - 1, h.lines.filter((v) => v.t !== '-').length, ...before);
      });
    } else hunks.forEach((h) => h.lines.forEach((v) => { if (v.n != null) contents[v.n - 1] = v.s; }));
    buffer.revisions = buffer.revisions || {}; buffer.revisions[sha] = Array.from(contents, (text) => text || '');
    S.openFile(file, line);
    const editor = S.focused();
    if (editor?.plugin === 'editor') { S.plugins.editor.reopen(editor, { file, line, rev: sha }); S.render(); }
  }
  function sheet(spec) {
    return S.sheet({ ...spec, on: { settings: (_, overlay) => { S.ov.pop(overlay); actions.settings.run(S.focused()); }, ...(spec.on || {}) } });
  }

  const actions = {
    down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
    up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
    enter: { title: 'Открыть выбранное', key: ['Enter', 'l'], hint: 'Открыть', run: (p) => { if (!target(p)) openTarget(targets(p)[p.state.cur]?.id); else if (p.state.section === 'threads') openThread(p, threads(p)[p.state.cur]?.id); else if (p.state.area !== 'diff') openFile(p); } },
    openTarget: { title: '', menu: false, run: (_, id) => openTarget(id) },
    openURL: { title: 'Открыть ревью по URL', key: 'o', hint: 'По ссылке', run: urlSheet },
    create: { title: 'Создать MR или PR', key: 'C', run: createSheet },
    list: { title: 'Список ревью', key: 'g r', run: () => S.open('review', { key: 'list' }) },
    submit: { title: 'Отправить ревью', when: (p) => hasTarget(p) && !submitMuted(target(p)), run: submitSheet },
    openFile: { title: '', menu: false, run: openFile },
    openThread: { title: '', menu: false, run: openThread },
    back: { title: 'Назад к списку', key: 'h', when: hasTarget, run: (p) => { p.state.area = 'list'; p.state.selection = null; S.render(); } },
    details: { title: 'Описание и проверки', when: hasTarget, run: (p) => section(p, 'overview') },
    nextFile: { title: 'Следующий непросмотренный файл', key: 'n', hint: 'Следующий', when: hasTarget, run: (p) => nextFile(p, 1) },
    previousFile: { title: 'Предыдущий непросмотренный файл', key: 'N', when: hasTarget, run: (p) => nextFile(p, -1) },
    nextChange: { title: 'Следующее изменение', key: '] c', when: isDiff, run: (p) => changeHunk(p, 1) },
    previousChange: { title: 'Предыдущее изменение', key: '[ c', when: isDiff, run: (p) => changeHunk(p, -1) },
    viewed: { title: 'Отметить файл просмотренным', key: 'v', when: hasTarget, run: (p, path) => {
      const m = target(p), file = path || currentFile(p); if (!file) return;
      if (m.provider === 'github' && !m.review.capabilities.write) { p.state.error = 'Нет прав на markFileAsViewed. Файл не отмечен.'; S.render(); return; }
      const head = m.review.revision.head_sha, map = m.review.viewedByRevision[head] || (m.review.viewedByRevision[head] = {}); map[file] = !map[file];
      if (m.provider === 'github') m.review.serverViewed[`${head}:${file}`] = { viewed: map[file], operation: map[file] ? 'markFileAsViewed' : 'unmarkFileAsViewed', remoteId: uid('viewed') };
      if (head === m.head) m.viewed = map; persist(); S.render();
    } },
    comment: { title: 'Черновик к строке', key: 'c', hint: 'Комментарий', when: (p) => isDiff(p) || (p.state.section === 'files' && !!target(p) && !!p.geo && p.geo.size !== 'Narrow'), run: (p, a) => {
      if (!isDiff(p)) { p.state.area = 'diff'; const first = flat(p).findIndex((l) => l.t === '+' || l.t === '-'); p.state.line = Math.max(0, first); }
      addDraft(p, a);
    } },
    selectRange: { title: 'Выделить диапазон строк', key: 'V', when: isDiff, run: (p) => { p.state.selection = p.state.selection == null ? p.state.line : null; S.render(); } },
    side: { title: '', menu: false, run: (p, side) => { p.state.side = side; const lines = flat(p), row = p.state.renderRows?.find((r) => r.indices.includes(p.state.line)); const i = row?.indices.find((n) => side === 'old' ? lines[n].o != null : lines[n].n != null); if (i != null) p.state.line = i; S.render(); } },
    oldSide: { title: 'Старая сторона diff', key: 'g o', when: isDiff, run: (p) => actions.side.run(p, 'old') },
    newSide: { title: 'Новая сторона diff', key: 'g n', when: isDiff, run: (p) => actions.side.run(p, 'new') },
    lineClick: { title: '', menu: false, run: (p, arg) => {
      const [index, clicked] = String(arg).split(':');
      const lines = flat(p), row = p.state.renderRows?.find((r) => r.key === +index), indices = row?.indices || [+index];
      const has = (i, side) => (side === 'old' ? lines[i]?.o : lines[i]?.n) != null;
      const side = clicked || p.state.side, exact = indices.find((i) => has(i, side));
      p.state.area = 'diff';
      p.state.line = exact ?? indices[0];
      p.state.side = exact != null ? side : has(indices[0], 'old') ? 'old' : 'new';
      S.render();
    } },
    diffMode: { title: 'Переключить вид diff', key: 's', when: isDiff, run: (p) => { p.state.sideBySide = !p.state.sideBySide; S.render(); } },
    editDraft: { title: 'Изменить черновик', when: (p) => !!target(p)?.drafts.length, run: (p, id) => editDraft(p, id || target(p).drafts[0]?.id) },
    deleteDraft: { title: 'Удалить выбранный черновик', when: (p) => !!target(p)?.drafts.length, run: (p, id) => { const m = target(p); if (!startAllowed(p)) return; const draftId = id || p.state.editing || m.drafts[0]?.id; m.drafts = m.drafts.filter((d) => d.id !== draftId); if (p.state.editing === draftId) p.state.editing = null; persist(); S.render(); } },
    saveDraft: { title: 'Закончить черновик', key: 'Cmd-Enter', when: (p) => !!p.state.editing, run: saveDraft },
    stopEditing: { title: 'Закрыть редактор черновика', when: (p) => !!p.state.editing, run: (p) => { p.state.editing = null; S.render(); } },
    suggestion: { title: 'Добавить suggestion', when: (p) => !!p.state.editing, run: (p) => { const d = target(p).drafts.find((v) => v.id === p.state.editing); if (d) { const lines = flat(p).filter((l) => l.n >= d.start && l.n <= d.line && l.n != null).map((l) => l.s).join('\n'); d.body += `${d.body ? '\n\n' : ''}\x60\x60\x60suggestion\n${lines}\n\x60\x60\x60`; d.saved = false; persist(); focusDraft(p); } } },
    gf: { title: 'Открыть файл в nvim', key: 'g f', when: isDiff, run: editorFile },
    reply: { title: 'Ответить в тред', key: 'r', when: (p) => hasTarget(p) && p.state.section === 'threads', run: replySheet },
    resolve: { title: 'Разрешить или открыть тред', key: 'x', when: (p) => hasTarget(p) && p.state.section === 'threads', run: resolveSheet },
    stayHead: { title: 'Остаться на ревизии', when: (p) => hasTarget(p) && target(p).review.revision.head_sha !== target(p).head, run: (p) => { target(p).review.stay = true; persist(); S.render(); } },
    followHead: { title: 'Перейти на новый head', when: hasTarget, run: followHead },
    reanchor: { title: 'Перепривязать черновики', when: hasTarget, run: reanchorSheet },
    browserDraft: { title: 'Браузерный черновик', when: (p) => !!target(p)?.review.browser, run: browserSheet },
    reconcile: { title: 'Сверить результат публикации', when: (p) => !!target(p)?.review.publication?.steps.some((s) => s.state === 'unknown'), run: (p) => reconcile(target(p)) },
    compareFrom: { title: '', menu: false, run: (p, sha) => { p.state.from = sha; S.render(); } },
    compareTo: { title: '', menu: false, run: (p, sha) => { p.state.to = sha; S.render(); } },
    sinceReview: { title: 'Изменения с прошлого ревью', when: hasTarget, run: (p) => { section(p, 'versions'); const m = target(p); p.state.from = m.review.lastReviewed; p.state.to = m.head; syncHead(m); S.render(); } },
    checkout: { title: 'Checkout ветки в worktree', when: hasTarget, run: checkoutSheet },
    checks: { title: 'Проверки текущего head', when: hasTarget, run: checksSheet },
    commit: { title: '', menu: false, run: commitSheet },
    browser: { title: 'Открыть цель в браузере', when: hasTarget, run: (p) => { window.open(target(p).url, '_blank', 'noopener,noreferrer'); } },
    settings: { title: 'Проверить аккаунт и права', run: () => { if (S.plugins.settings) S.open('settings', { section: 'accounts' }); } },
    clearError: { title: '', menu: false, run: (p) => { p.state.error = ''; S.render(); } },
    clearFilter: { title: 'Сбросить фильтр', run: (p) => { p.filter = ''; S.render(); } },
    expandFiles: { title: 'Развернуть файлы ревью', when: hasTarget, run: (p) => { nextFile(p, 1); S.move(p.id, 'center'); S.render(); } },
  };
  Object.entries(labels).forEach(([name, title]) => { actions[name] = { title: `Ревью: ${title.toLowerCase()}`, key: { overview: 'g d', files: 'g i', threads: 'g t', versions: 'g v' }[name], when: hasTarget, run: (p) => section(p, name) }; });
  ['merge', 'squash', 'autoMerge', 'draft', 'ready', 'closeTarget', 'reopenTarget'].forEach((name) => { actions[name] = { title: { merge: 'Merge', squash: 'Squash', autoMerge: 'Auto-merge', draft: 'Перевести в Draft', ready: 'Отметить Ready', closeTarget: 'Закрыть MR или PR', reopenTarget: 'Открыть MR или PR снова' }[name], when: hasTarget, run: (p) => manage(p, name) }; });
  ['reviewers', 'assignees', 'labels'].forEach((name) => { actions[name] = { title: { reviewers: 'Изменить ревьюеров', assignees: 'Изменить assignees', labels: 'Изменить метки' }[name], when: hasTarget, run: (p) => metadataSheet(p, name) }; });

  S.plugin({
    id: 'review', title: 'Ревью', icon: 'review', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'multi', minSize: { w: 220, h: 90 }, inputDefault: false,
    init(p, opts) { const id = opts.target || opts.key || 'list'; p.key = id; Object.assign(p.state, { target: id, section: 'overview', area: 'list', cur: 0, file: null, thread: null, line: 0, side: 'new', sideBySide: true, selection: null, editing: null, error: '' }); ensure(W.mrs[id]); },
    reopen(p, opts) { if (opts.target && opts.target !== p.state.target) { p.state.target = opts.target; p.key = opts.target; p.state.area = 'list'; p.state.section = 'overview'; p.state.file = null; p.state.cur = 0; } },
    tabTitle: (p) => target(p)?.id || 'Ревью',
    tabStatus: () => null,
    meter: () => ({ kind: 'estimate', text: '≈ 0,1% CPU · 8,6 МБ' }),
    context: (p) => target(p) ? [{ label: `${target(p).id} ${target(p).review.revision.head_sha}`, icon: 'review' }, ...(isDiff(p) ? [{ label: `${currentFile(p)}:${flat(p)[p.state.line]?.[p.state.side === 'old' ? 'o' : 'n'] || 1}`, icon: 'file' }] : [])] : [],
    location: (p) => isDiff(p) ? { file: currentFile(p), line: flat(p)[p.state.line]?.[p.state.side === 'old' ? 'o' : 'n'] || 1 } : {},
    onBind(p, name, value) { if (name === 'draftBody') { const d = target(p)?.drafts.find((v) => v.id === p.state.editing); if (d) { d.body = value; d.saved = false; persist(); } } },
    onFocus(p) { const m = target(p); if (m?.review.browser && !m.review.browserChoice && !S.st.overlays.some((o) => o.type === 'sheet')) setTimeout(() => { if (S.focused() === p && !m.review.browserChoice && !S.st.overlays.some((o) => o.type === 'sheet')) browserSheet(p); }, 0); },
    view: (p, geo) => K.adapt(geo, { left: () => geo.size === 'ShortBar' ? bottom(p, geo) : targetView(p, geo, true), right: () => geo.size === 'ShortBar' ? bottom(p, geo) : targetView(p, geo, true), center: () => geo.size === 'ShortBar' ? bottom(p, geo) : targetView(p, geo, geo.size === 'Narrow' || geo.width < 620), bottom: () => bottom(p, geo) }),
    actions,
  });

  const initial = seedTargets;
  function scenario(id, title, setup) {
    S.scenario({ id: `review.${id}`, group: 'Ревью', title, run() {
      const targetId = id.startsWith('github') ? '#87' : '!142';
      W.mrs[targetId] = copy(initial[targetId]); const m = ensure(W.mrs[targetId], false);
      m.review.browser = null; m.review.browserChoice = null; m.review.pendingReviewId = null; m.review.failOutcome = false; m.review.timeoutNext = false;
      m.review.capabilities = { requestChanges: true, approve: true, merge: true, write: true };
      const drawer = S.ov.has('drawer'); if (drawer) S.ov.pop(drawer);
      const p = openTarget(targetId); p.state.editing = null; p.state.error = ''; p.state.section = 'overview'; p.state.area = 'list';
      setup(m, p); persist(); S.render();
    } });
  }
  function browserFixture(m) {
    const file = m.files[0], l = K.flat({ hunks: W.diffs[file] || [] }).find((v) => v.n != null), line = l?.n || 1;
    m.review.browser = { id: uid(m.provider === 'github' ? 'pending' : 'draft-notes'), summary: 'Проверить поведение при истёкшей сессии.', comments: [{ id: uid('browser'), remoteId: uid('note'), file, line, start: line, range: String(line), side: 'new', head: m.head, saved: true, body: 'Этот комментарий был написан в браузере и ещё не опубликован.', anchor: anchor(m, m.review.revision, file, line), by: null }] };
    m.review.browserChoice = null;
  }
  scenario('head', 'Head изменился во время ревью', (m, p) => {
    const old = m.head; m.head = 'b4e82d0'; m.start = old; W.commits[m.head] = { sha: m.head, title: 'Обработать повторный callback', author: m.author, parent: old, files: [m.files[0]], body: 'Повторный callback возвращает ошибку до обмена токена.' };
    m.commits.unshift(m.head); const rev = freshRevision(m); rev.diffs[m.files[0]][0].lines.push({ t: '+', o: null, n: 22, s: '    session.reject_reused_state().await?;' });
    m.review.revisions.push(rev); p.state.section = 'files'; p.state.area = 'diff';
  });
  scenario('browser', 'GitLab: браузерные draft notes перед работой', (m, p) => { browserFixture(m); browserSheet(p); });
  scenario('github_browser', 'GitHub: существующий pending review', (m, p) => { browserFixture(m); browserSheet(p); });
  scenario('partial', 'Комментарии опубликованы, итог отклонён', (m, p) => {
    m.review.outcome = 'APPROVE'; m.review.summary = 'Комментарии опубликованы, требуется повторить одобрение.'; const pub = m.review.publication = makePlan(m);
    pub.steps.filter((s) => s.kind !== 'outcome').forEach((s) => { s.history.push('in_flight'); confirmStep(m, pub, s, uid('remote')); });
    pub.error = 'Сервер отклонил одобрение. Комментарии и сводка опубликованы. Повторите только итог.'; submitSheet(p);
  });
  scenario('timeout', 'Таймаут: результат unknown, нужна сверка', (m, p) => {
    m.review.outcome = 'APPROVE'; const pub = m.review.publication = makePlan(m), step = pub.steps[0]; step.state = 'unknown'; step.history.push('in_flight', 'unknown'); pub.receipts[step.id] = { remoteId: uid('remote') }; pub.error = 'Ответ не получен. Проверьте результат перед продолжением.'; submitSheet(p);
  });
  scenario('timeout_absent', 'Сверка подтверждает отсутствие результата', (m, p) => {
    const pub = m.review.publication = makePlan(m), step = pub.steps[0]; step.state = 'unknown'; step.history.push('in_flight', 'unknown'); pub.receipts[step.id] = { absent: true }; pub.error = 'Результат неизвестен. Повтор доступен после сверки.'; submitSheet(p);
  });
  scenario('no_request_changes', 'GitLab: Request changes недоступен', (m, p) => { m.review.capabilities.requestChanges = false; submitSheet(p); });
  scenario('no_rights', 'Нет прав на изменение цели', (m, p) => { m.review.capabilities = { write: false, approve: false, requestChanges: false, merge: false }; submitSheet(p); });
  scenario('merge_ready', 'Все препятствия для слияния устранены', (m) => { m.threads.forEach((t) => { t.resolved = true; }); const run = pipeline(m); if (run) { run.sha = m.head; run.state = 'ok'; (W.jobs[run.id] || []).forEach((s) => s.jobs.forEach((j) => { j.state = 'ok'; })); } m.draft = false; m.reviewers[0].state = 'approved'; });
})();
