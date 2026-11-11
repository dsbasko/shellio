// nvim рабочей области: один управляемый экземпляр, запускается по необходимости. Файлы и буферы показывает сам nvim.
(() => {
  const { esc } = S;
  const session = (p) => W.sessions[p.state.sid];
  // Без связи с RPC поверхность nvim остаётся рабочей: недоступно только управление из shellio.
  const live = (p) => session(p).host === 'ok';
  const typing = (p) => live(p) && p.mode === 'input';
  const normal = (p) => typing(p) && p.state.mode !== 'Insert' && p.state.command == null;

  function buffer(file) {
    const b = W.buffers[file] || (W.buffers[file] = { lang: null, lines: [''], origin: [] });
    if (!b.lines.length) b.lines.push('');
    if (!b.savedLines) b.savedLines = b.lines.slice();
    if (!b.revisions) {
      b.revisions = {};
      Object.keys(W.commits).forEach((rev) => { b.revisions[rev] = b.savedLines.slice(); });
    }
    return b;
  }

  function lines(p) {
    const b = buffer(p.state.file);
    if (!p.state.rev) return b.lines;
    return b.revisions[p.state.rev] || (b.revisions[p.state.rev] = b.savedLines.slice());
  }

  function clamp(p) {
    const t = p.state; const ls = lines(p);
    t.line = S.clamp(t.line, 0, ls.length - 1);
    t.col = S.clamp(t.col, 0, Math.max(0, ls[t.line].length - (t.mode === 'Insert' ? 0 : 1)));
  }

  function openBuffer(p, opts = {}) {
    const t = p.state;
    if (!opts.file && opts.line == null && !Object.prototype.hasOwnProperty.call(opts, 'rev')) return;
    t.file = opts.file || t.file;
    t.rev = opts.rev || null;
    t.line = Math.max(0, (Number(opts.line) || 1) - 1);
    t.col = Number(opts.col) || 0; t.mode = 'Normal'; t.anchor = null; t.command = null; t.pending = ''; t.message = '';
    buffer(t.file); clamp(p); p.mode = 'input'; p._cur = null;
  }

  function writable(p) {
    if (!live(p)) return false;
    if (!p.state.rev) return true;
    p.state.message = 'E21: Буфер только для чтения. Откройте рабочую копию.';
    p.state.pending = ''; S.render(); return false;
  }

  function changed(p) {
    const b = buffer(p.state.file);
    b.modified = b.lines.length !== b.savedLines.length || b.lines.some((s, i) => s !== b.savedLines[i]);
    p.state.message = ''; clamp(p); S.render();
  }

  function insert(p, text) {
    if (!writable(p) || p.state.mode !== 'Insert') return;
    const t = p.state; const b = buffer(t.file); const parts = text.replace(/\r\n?/g, '\n').split('\n');
    const before = b.lines[t.line].slice(0, t.col); const after = b.lines[t.line].slice(t.col);
    if (parts.length === 1) { b.lines[t.line] = before + text + after; t.col += text.length; }
    else {
      b.lines.splice(t.line, 1, before + parts[0], ...parts.slice(1, -1), parts[parts.length - 1] + after);
      if (b.origin) b.origin.splice(t.line + 1, 0, ...Array(parts.length - 1).fill(0));
      t.line += parts.length - 1; t.col = parts[parts.length - 1].length;
    }
    changed(p);
  }

  function move(p, kind) {
    const t = p.state; const ls = lines(p);
    t.pending = ''; t.message = '';
    if (kind === 'left') t.col--;
    if (kind === 'right') t.col++;
    if (kind === 'down') t.line++;
    if (kind === 'up') t.line--;
    if (kind === 'start') t.col = 0;
    if (kind === 'end') t.col = Math.max(0, ls[t.line].length - 1);
    if (kind === 'first') { t.line = 0; t.col = 0; }
    if (kind === 'last') { t.line = ls.length - 1; t.col = 0; }
    if (kind === 'word' || kind === 'backWord') {
      const text = ls.join('\n');
      let offset = ls.slice(0, t.line).reduce((n, s) => n + s.length + 1, 0) + t.col;
      const type = (c) => c == null || /\s/u.test(c) ? 0 : /[\p{L}\p{N}_]/u.test(c) ? 1 : 2;
      if (kind === 'word') {
        const group = type(text[offset]);
        while (offset < text.length && type(text[offset]) === group) offset++;
        while (offset < text.length && !type(text[offset])) offset++;
      } else {
        offset = Math.max(0, offset - 1);
        while (offset > 0 && !type(text[offset])) offset--;
        const group = type(text[offset]);
        while (offset > 0 && type(text[offset - 1]) === group) offset--;
      }
      const before = text.slice(0, offset).split('\n');
      t.line = before.length - 1; t.col = before[before.length - 1].length;
    }
    clamp(p); S.render();
  }

  function enterInsert(p, kind) {
    if (!writable(p)) return;
    const t = p.state; const b = buffer(t.file);
    t.mode = 'Insert'; t.anchor = null; t.pending = ''; t.command = null; t.message = ''; p.mode = 'input';
    if (kind === 'append') t.col = Math.min(b.lines[t.line].length, t.col + 1);
    if (kind === 'below') {
      b.lines.splice(++t.line, 0, '');
      if (b.origin) b.origin.splice(t.line, 0, 0);
      t.col = 0; changed(p);
    }
    S.render();
  }

  function save(p) {
    if (!writable(p)) return;
    const b = buffer(p.state.file);
    b.savedLines = b.lines.slice(); b.modified = false;
    p.state.message = `"${p.state.file}" ${S.fmt.plural(b.lines.length, 'строка записана', 'строки записаны', 'строк записано')}`;
    S.render();
  }

  function execute(p) {
    const t = p.state; const cmd = (t.command || '').trim();
    t.command = null; t.pending = '';
    if (cmd === 'w') actions.save.run(p);
    else if (cmd === 'q' || cmd === 'qa') { if (W.dirtyBuffers(S.repo().id)) t.message = 'E37: Изменения не записаны (добавьте ! для выхода без записи)'; else quit(p); }
    else if (cmd === 'q!' || cmd === 'qa!') { discard(); quit(p); }
    else if (cmd === 'wq' || cmd === 'x') { save(p); quit(p); }
    else if (/^\d+$/.test(cmd)) { t.line = Math.max(0, Number(cmd) - 1); t.col = 0; clamp(p); }
    else if (cmd) t.message = `E492: Неизвестная команда: ${cmd}. Используйте :w, :q или номер строки.`;
    S.render();
  }

  const discard = () => Object.values(W.buffers).forEach((b) => { if (b.modified) { b.lines = b.savedLines.slice(); b.modified = false; } });
  // Выход из nvim с кодом 0 завершает экземпляр и убирает вкладку.
  function quit(p) {
    const s = session(p);
    delete W.sessions[s.id];
    S.close(p.id, { silent: true, force: true });
  }

  // Переход, пришедший во время запуска или без связи с RPC, ждёт готовности и не создаёт второй nvim.
  // Очередь принадлежит сессии, а не виду: закрытие и повторное открытие вкладки её не теряют.
  function apply(p, opts) {
    const s = session(p);
    const jump = opts.file || opts.line != null || Object.prototype.hasOwnProperty.call(opts, 'rev');
    if (s.host !== 'ok' || s.rpcDown) { if (jump) s.queued = { file: opts.file, line: opts.line, col: opts.col, rev: opts.rev }; return; }
    if (jump) delete s.queued;
    openBuffer(p, opts);
  }
  S.nvimDrain = (s) => drain(s);
  function drain(s) {
    if (!s.queued || s.host !== 'ok' || s.rpcDown) return;
    const p = S.panelOf(s); if (!p) return;
    const q = s.queued; delete s.queued; openBuffer(p, q);
  }

  const actions = {
    ...S.sessionActions,
    left: { title: 'Курсор влево', key: ['h', 'Left'], when: normal, run: (p) => move(p, 'left') },
    down: { title: 'Курсор вниз', key: ['j', 'Down'], when: normal, run: (p) => move(p, 'down') },
    up: { title: 'Курсор вверх', key: ['k', 'Up'], when: normal, run: (p) => move(p, 'up') },
    right: { title: 'Курсор вправо', key: ['l', 'Right'], when: normal, run: (p) => move(p, 'right') },
    word: { title: 'Следующее слово', key: 'w', when: normal, run: (p) => move(p, 'word') },
    backWord: { title: 'Предыдущее слово', key: 'b', when: normal, run: (p) => move(p, 'backWord') },
    start: { title: 'Начало строки', key: '0', when: normal, run: (p) => move(p, 'start') },
    end: { title: 'Конец строки', key: '$', when: normal, run: (p) => move(p, 'end') },
    first: { title: 'Первая строка', key: 'g g', when: normal, run: (p) => move(p, 'first') },
    last: { title: 'Последняя строка', key: 'G', when: normal, run: (p) => move(p, 'last') },
    insert: { title: 'Вставить текст', key: 'i', hint: 'Вставить', when: normal, run: (p) => enterInsert(p, 'at') },
    append: { title: 'Вставить после курсора', key: 'a', when: normal, run: (p) => enterInsert(p, 'append') },
    below: { title: 'Новая строка ниже', key: 'o', when: normal, run: (p) => enterInsert(p, 'below') },
    removeChar: { title: 'Удалить символ', key: 'x', when: normal, run: (p) => {
      if (!writable(p)) return;
      const t = p.state; const b = buffer(t.file); const row = b.lines[t.line];
      b.lines[t.line] = row.slice(0, t.col) + row.slice(t.col + (row.codePointAt(t.col) > 0xffff ? 2 : 1)); changed(p);
    } },
    removeLine: { title: 'Удалить строку', key: 'd d', when: normal, run: (p) => {
      if (!writable(p)) return;
      const t = p.state; const b = buffer(t.file);
      const from = t.anchor == null ? t.line : Math.min(t.line, t.anchor);
      const count = t.anchor == null ? 1 : Math.abs(t.line - t.anchor) + 1;
      b.lines.splice(from, count); if (b.origin) b.origin.splice(from, count);
      if (!b.lines.length) b.lines.push('');
      t.line = Math.min(from, b.lines.length - 1); t.col = 0; t.anchor = null; t.mode = 'Normal'; changed(p);
    } },
    visual: { title: 'Выделить строки', key: 'v', hint: 'Выделить', when: normal, run: (p) => {
      const t = p.state; t.mode = t.mode === 'Visual' ? 'Normal' : 'Visual'; t.anchor = t.mode === 'Visual' ? t.line : null; t.pending = ''; S.render();
    } },
    command: { title: 'Команда nvim', key: ':', hint: 'Команда', when: normal, run: (p) => { if (!live(p)) return; p.state.mode = 'Normal'; p.state.anchor = null; p.state.command = ''; p.state.pending = ''; p.state.message = ''; p.mode = 'input'; clamp(p); S.render(); } },
    save: { title: 'Сохранить буфер', when: live, run: save },
    close: { title: 'Закрыть вкладку, nvim продолжит работу', run: (p) => S.close(p.id) },
    reconnect: { title: 'Переподключиться к nvim', when: (p) => !!session(p).rpcDown, run: (p) => { const s = session(p); s.reconnecting = true; S.render(); setTimeout(() => { if (W.sessions[s.id] !== s) return; s.rpcDown = false; s.reconnecting = false; drain(s); S.render(); }, 900); } },
    escape: { title: 'Режим Normal', key: 'Esc', when: typing, run: (p) => {
      const t = p.state; if (t.mode === 'Insert') t.col = Math.max(0, t.col - 1);
      t.mode = 'Normal'; t.anchor = null; t.command = null; t.pending = ''; t.message = ''; clamp(p); S.render();
    } },
    text: { title: '', menu: false, run: (p, text) => insert(p, text) },
    newline: { title: 'Разбить строку', when: (p) => typing(p) && p.state.mode === 'Insert', run: (p) => insert(p, '\n') },
    backspace: { title: 'Удалить перед курсором', when: (p) => typing(p) && p.state.mode === 'Insert', run: (p) => {
      if (!writable(p)) return;
      const t = p.state; const b = buffer(t.file);
      if (t.col > 0) {
        const row = b.lines[t.line]; const before = Array.from(row.slice(0, t.col)); before.pop();
        const text = before.join(''); b.lines[t.line] = text + row.slice(t.col); t.col = text.length;
      } else if (t.line > 0) {
        t.col = b.lines[t.line - 1].length; b.lines[t.line - 1] += b.lines[t.line];
        b.lines.splice(t.line, 1); if (b.origin) b.origin.splice(t.line, 1); t.line--;
      }
      changed(p);
    } },
    commandText: { title: '', menu: false, run: (p, text) => { p.state.command += text; S.render(); } },
    commandErase: { title: '', menu: false, run: (p) => { p.state.command = p.state.command ? Array.from(p.state.command).slice(0, -1).join('') : null; S.render(); } },
    execute: { title: 'Выполнить команду nvim', when: (p) => typing(p) && p.state.command != null, run: execute },
    prefix: { title: '', menu: false, run: (p, value) => { p.state.pending = value; S.render(); } },
    lineClick: { title: '', menu: false, run: (p, value) => { if (!live(p)) return; p.state.line = Number(value); p.state.col = 0; p.state.command = null; p.state.pending = ''; p.mode = 'input'; clamp(p); S.render(); } },
    workingCopy: { title: 'Открыть рабочую копию', when: (p) => !!p.state.rev, run: (p) => { openBuffer(p, { file: p.state.file, line: p.state.line + 1 }); S.render(); } },
    noop: { title: 'Установить nvim', when: (p) => session(p).host === 'missing', run: (p) => { S.sessionActions.restart.run(p); S.notify({ ...S.owner(p.wb), title: 'nvim установлен', text: 'Сессия готова к работе.', tone: 'ok' }); } },
  };

  function surface(p, geo) {
    const s = session(p); const status = S.sessionView(p, s);
    if (status) return status;
    clamp(p);
    const t = p.state; const b = buffer(t.file); const ls = lines(p);
    const focused = S.focused() === p && p.mode === 'input';
    const selected = t.anchor == null ? null : [Math.min(t.anchor, t.line), Math.max(t.anchor, t.line)];
    const parts = t.file.split('/');
    const name = parts.pop();
    const left = t.command != null
      ? `<span class="here">:${esc(t.command)}${focused ? '<span class="cur"></span>' : ''}</span>`
      : t.message || t.pending ? `<span class="here">${esc(t.message || t.pending)}</span>` : '';
    const modeTag = t.mode === 'Normal' ? '' : `<span class="here" data-do="escape">-- ${t.mode === 'Insert' ? 'INSERT' : 'VISUAL'} --</span>`;
    const sign = (i) => (!t.rev && b.origin && b.origin[i] === 3 ? '~' : '');
    return `<div class="nv">
      ${s.rpcDown ? K.banner('wait', 'Нет связи с nvim', 'Управление из shellio недоступно, сам nvim работает. Переходы к файлам дождутся переподключения, второй nvim не запустится.', K.btn(s.reconnecting ? 'Подключаемся…' : 'Переподключиться', { do: 'reconnect', small: true, disabled: s.reconnecting })) : ''}
      ${t.rev ? K.banner('info', `Ревизия ${esc(t.rev)} только для чтения`, '', K.btn('Открыть рабочую копию', { do: 'workingCopy', small: true })) : ''}
      ${K.nv(ls, { lang: b.lang, cursor: t.line, curCol: focused && t.command == null ? t.col : null, curKind: t.mode === 'Insert' ? 'bar' : 'block', sel: selected, sign, onLine: 'lineClick', tail: Math.max(0, geo.height - 62) })}
      <div class="nv-st" data-k="nvim-status" data-do="command">${modeTag}${left}<span class="here">${esc(name)}${b.modified ? ' [+]' : ''}</span><span>${esc(parts.join('/'))}</span>${t.rev ? `<span>[${esc(t.rev)}] только чтение</span>` : ''}<span class="end">${b.lang || 'text'}</span><span>utf-8</span><span>${t.line + 1}:${t.col + 1}</span></div>
    </div>`;
  }

  S.plugin({
    id: 'editor', title: 'nvim', icon: 'file', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'workbench', minSize: { w: 360, h: 150 }, inputDefault: true,
    surfaceless: (p) => !live(p),
    inputTarget: () => 'nvim',
    init(p, opts = {}) {
      const named = opts.sid && W.sessions[opts.sid];
      const own = (named && named.wb === p.wb && named) || Object.values(W.sessions).find((x) => x.kind === 'editor' && x.wb === p.wb);
      const id = own ? own.id : S.uid('nvim');
      // Первый запуск в рабочей области: nvim стартует в её worktree, переход выполнится после готовности.
      const s = own || (W.sessions[id] = { id, kind: 'editor', title: 'nvim', wb: p.wb, state: 'idle', host: 'starting', gen: 1, cpu: 0.4, mem: 92, cwd: W.workbenches[p.wb].worktree });
      s.background = false;
      Object.assign(p.state, { sid: id, file: Object.keys(W.buffers)[0], rev: null, line: 0, col: 0, mode: 'Normal', anchor: null, pending: '', command: null, message: '' }, s.editorState || {});
      s.editorState = p.state; buffer(p.state.file); apply(p, opts); drain(s);
      if (s.host === 'starting') setTimeout(() => { if (W.sessions[id] !== s || s.host !== 'starting') return; s.host = 'ok'; drain(s); S.render(); }, 900);
    },
    reopen(p, opts = {}) { const s = session(p); s.background = false; apply(p, opts); drain(s); },
    tabTitle: () => 'nvim',
    tabStatus: (p) => { const s = session(p); return s.rpcDown ? 'wait' : s.host === 'ok' ? null : s.host === 'missing' ? 'err' : S.agentSt(s); },
    // Остановка — обычный выход nvim: он сам не отпустит несохранённые буферы.
    stop(p) {
      const s = session(p);
      if (s.rpcDown) {
        S.sheet({ icon: 'stop', title: 'Нет связи с nvim', width: 460, sub: 'Выйти можно в самом nvim. Принудительное завершение остановит процесс без проверки буферов: что в них не сохранено, неизвестно.', actions: [{ label: 'Вернуться к nvim', key: 'Esc', run: () => { p.mode = 'input'; S.focus(p.id); } }, { label: 'Завершить принудительно', kind: 'danger', key: 'k', run: () => quit(p) }] });
        return;
      }
      const dirty = W.dirtyBuffers(S.repo().id);
      S.sheet({
        icon: 'stop', title: 'Остановить nvim?', width: 460,
        sub: dirty ? `Несохранённых буферов: ${dirty}. nvim не завершится, пока вы их не сохраните или не отмените.` : 'nvim завершится, вкладка закроется. Если нужно только убрать вкладку, закройте её: nvim продолжит работу.',
        actions: [{ label: 'Закрыть вкладку', key: 'c', run: () => S.close(p.id) }, { label: 'Отменить', key: 'Esc' }, { label: 'Остановить', kind: 'danger', key: 'Enter', run: () => {
          if (W.dirtyBuffers(S.repo().id)) { p.state.message = 'E37: Изменения не записаны (добавьте ! для выхода без записи)'; p.mode = 'input'; S.focus(p.id); return; }
          quit(p);
        } }],
      });
    },
    meter: (p) => { const s = session(p); return s.host === 'finished' ? null : { kind: 'measured', text: `${S.fmt.num(s.cpu)}% CPU · ${S.fmt.mem(s.mem)}` }; },
    context: (p) => { const t = p.state; return [{ label: `${t.file}:${t.anchor == null ? t.line + 1 : `${Math.min(t.anchor, t.line) + 1}–${Math.max(t.anchor, t.line) + 1} (выделение)`}${t.rev ? ` @ ${t.rev}` : ''}`, icon: 'file' }]; },
    location: (p) => ({ file: p.state.file, line: p.state.line + 1 }),
    view: (p, geo) => K.adapt(geo, { left: () => surface(p, geo), right: () => surface(p, geo), center: () => surface(p, geo), bottom: () => surface(p, geo) }),
    actions,
    onKey(p, e, tok) {
      if (!live(p)) {
        const action = Object.values(S.sessionActions).find((a) => a.key && [].concat(a.key).includes(tok) && (!a.when || a.when(p)));
        if (action) action.run(p); return;
      }
      if (tok === 'Ctrl-\\' || e.metaKey || e.ctrlKey || e.altKey) return;
      const t = p.state;
      if (tok === 'Esc') { actions.escape.run(p); return; }
      if (t.command != null) {
        if (tok === 'Enter') actions.execute.run(p);
        else if (tok === 'Backspace') actions.commandErase.run(p);
        else if (Array.from(e.key).length === 1) actions.commandText.run(p, e.key);
        return;
      }
      if (t.mode === 'Insert') {
        if (tok === 'Enter') actions.newline.run(p);
        else if (tok === 'Backspace') actions.backspace.run(p);
        else if (Array.from(e.key).length === 1) actions.text.run(p, e.key);
        else { const dir = { Left: 'left', Right: 'right', Up: 'up', Down: 'down' }[tok]; if (dir) actions[dir].run(p); }
        return;
      }
      if (t.pending === 'g' && e.key === 'g') { actions.first.run(p); return; }
      if (t.pending === 'd' && e.key === 'd') { t.pending = ''; actions.removeLine.run(p); return; }
      t.pending = '';
      if (e.key === 'g' || e.key === 'd') { actions.prefix.run(p, e.key); return; }
      const key = { h: 'left', j: 'down', k: 'up', l: 'right', w: 'word', b: 'backWord', '0': 'start', '$': 'end', G: 'last', i: 'insert', a: 'append', o: 'below', x: 'removeChar', v: 'visual', ':': 'command', ArrowLeft: 'left', ArrowDown: 'down', ArrowUp: 'up', ArrowRight: 'right' }[e.key];
      if (key) actions[key].run(p);
      else S.render();
    },
    destroy: (p, opts) => S.sessionDestroy(p, opts),
    onRestart: (p, s) => { s.cpu = 0.4; s.mem = 92; p.state.mode = 'Normal'; p.state.command = null; p.state.pending = ''; p.state.anchor = null; p.state.message = ''; clamp(p); drain(s); },
  });

  S.scenario({ id: 'editor-cold', group: 'nvim', title: 'nvim не запущен: переход к файлу запускает его', run: () => {
    if (!S.wb()) S.go('feature-sso');
    const s = Object.values(W.sessions).find((x) => x.kind === 'editor' && x.wb === S.wb().id);
    if (s) { const p = S.panelOf(s); delete W.sessions[s.id]; if (p) S.close(p.id, { silent: true, force: true }); }
    S.notify({ title: 'nvim не запущен', text: 'Через секунду придёт переход к src/auth/session.rs:14, вкладка nvim появится и подождёт запуска.' });
    setTimeout(() => { S.openFile('src/auth/session.rs', 14); setTimeout(() => S.openFile('src/auth/session.rs', 16), 300); }, 1000);
  } });
  S.scenario({ id: 'editor-rpc', group: 'nvim', title: 'nvim работает, RPC не отвечает', run: () => { const p = S.open('editor', {}); session(p).rpcDown = true; S.render(); } });
  S.scenario({ id: 'editor-revision', group: 'nvim', title: 'Исторический буфер только для чтения', run: () => S.open('editor', { file: 'src/auth/sso.rs', line: 16, rev: '7d10e4b' }) });
  S.scenario({ id: 'editor-missing', group: 'nvim', title: 'nvim не установлен', run: () => { const p = S.open('editor', {}); session(p).host = 'missing'; session(p).program = 'nvim'; S.render(); } });
  S.scenario({ id: 'editor-lost', group: 'nvim', title: 'Хост nvim потерян, процесс жив', run: () => { const p = S.open('editor', {}); const s = session(p); s.host = 'lost'; s.procs = [['nvim', 48221, '14:18:03']]; s.snapshot = lines(p).slice(p.state.line, p.state.line + 7).map(esc); S.render(); } });
  S.scenario({ id: 'editor-reconnecting', group: 'nvim', title: 'Подключение к сессии nvim', run: () => {
    const p = S.open('editor', {}); const s = session(p); s.host = 'reconnecting'; s.snapshot = lines(p).slice(p.state.line, p.state.line + 7).map(esc); S.render();
    setTimeout(() => { if (W.sessions[s.id] === s && s.host === 'reconnecting') { s.host = 'ok'; drain(s); S.render(); } }, 1800);
  } });
})();
