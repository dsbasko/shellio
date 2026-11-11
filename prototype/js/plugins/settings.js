(() => {
  const { esc } = S;
  const sections = [
    ['general', 'Общие', 'sliders'], ['appearance', 'Оформление', 'sun'], ['agents', 'Агенты', 'agent'],
    ['prompts', 'Шаблоны prompt', 'draft'], ['keys', 'Клавиши', 'keys'], ['accounts', 'Аккаунты', 'user'], ['terminal', 'Терминал', 'term'],
  ];
  W.settings = W.settings || {};
  W.settings.promptDraft = W.settings.promptDraft == null ? W.commitPrompt : W.settings.promptDraft;
  W.settings.terminal = W.settings.terminal || {
    path: 'ghostty.conf',
    config: 'font-family = Menlo\nfont-size = 12\nscrollback-limit = 10000000',
    font: 'Menlo', size: 12, history: 10000000,
  };
  const section = (p) => sections.find((s) => s[0] === p.state.section) || sections[0];
  const inSection = (p, id) => section(p)[0] === id && (p.state.area === 'content' || p.state.wide);
  const match = (p, text) => S.match(text, p.filter || '') > 0;
  const sectionList = (p) => p.state.area === 'list' ? sections.filter((s) => match(p, s[1])) : sections;
  const project = (p) => { const own = W.projects[p.state.project]; return own && own.ws === S.st.route.ws ? own : S.project() || S.projectsIn(S.st.route.ws).find((x) => !x.removedAt) || null; };
  const own = (o, key) => Object.prototype.hasOwnProperty.call(o, key);
  const effective = (profile, pr) => own(profile.override || {}, pr) ? profile.override[pr] : profile.flags;
  // Пустые флаги проекта тоже переопределяют глобальный профиль при запуске.
  const launchAgent = S.launchAgent;
  if (launchAgent) S.launchAgent = (profile, ...args) => launchAgent({ ...profile, flags: effective(profile, S.project().id), override: profile.override || {} }, ...args);
  const field = (label, options, autofocus = false) => `<label class="form-l">${esc(label)}${K.field(options).replace(autofocus ? '<input' : '<unused', '<input data-autofocus')}</label>`;
  const note = (text) => `<p class="row-s">${esc(text)}</p>`;
  const focusField = (p, bind) => requestAnimationFrame(() => {
    const el = document.querySelector(`[data-panel="${p.id}"] [data-bind="${bind}"]`);
    if (el) { el.focus(); if (el.select) el.select(); }
  });
  const promptError = (text) => !text.trim() ? 'Шаблон пуст. Опишите, какое сообщение коммита нужно получить.' : text.length > 16000 ? 'Шаблон длиннее 16 000 символов. Сократите его.' : /\{\{|\}\}/.test(text) ? 'Переменные в двойных скобках не поддерживаются. Замените их обычным текстом.' : '';
  const commandList = (p) => S.commands.filter((c) => c.title && match(p, `${c.title} ${c.id} ${c.grp || ''} ${S.keysOf(c).join(' ')}`) && S.match(`${c.title} ${c.id} ${c.grp || ''} ${S.keysOf(c).join(' ')}`, p.state.keyFilter || '') > 0);

  function navigate(p, id) {
    if (!sections.some((s) => s[0] === id)) return;
    const old = { area: p.state.area, section: p.state.section, row: p.state.row, cur: p.state.cur, filter: p.filter };
    S.mark(p, () => { Object.assign(p.state, { area: old.area, section: old.section, row: old.row, cur: old.cur }); p.filter = old.filter; });
    p.state.section = id; p.state.cur = sections.findIndex((s) => s[0] === id); p.state.row = 0; p.state.area = 'content'; p.filter = '';
    S.render();
  }
  function rows(p) {
    const pr = project(p);
    const prefs = S.st.prefs;
    let out = [];
    switch (section(p)[0]) {
      case 'general':
        out = [
          ...(S.wb() ? [{ id: 'workbench', title: 'Рабочая область', sub: `${S.project().title} / ${S.wb().title}`, icon: 'grid', do: 'switchWorkbench' }] : []),
          ...[['left', 'Левая зона'], ['right', 'Правая зона'], ['bottom', 'Нижняя зона']].map(([zone, title]) => ({ id: `zone-${zone}`, title, sub: S.layout().hidden[zone] ? 'Скрыта' : 'Показана', trail: K.toggle(!S.layout().hidden[zone]), do: 'toggleZone', arg: zone })),
          { id: 'quit', title: 'Спрашивать при выходе', sub: 'Сессии всегда продолжают работу после выхода', trail: K.toggle(!S.st.prefs.quitSilent), do: 'toggleQuit' },
          { id: 'cross', title: 'Показывать внимание других воркспейсов', sub: 'Число у переключателя: агент ждёт ответа или упал запуск. Названия только в его списке', trail: K.toggle(S.st.prefs.crossAttention), do: 'toggleCross' },
          { id: 'keys', title: 'Клавиши навигации', sub: 'Настроить команды и последовательности', icon: 'keys', do: 'section', arg: 'keys' },
        ]; break;
      case 'appearance':
        out = [
          { id: 'dark', title: 'Тёмная тема', icon: 'moon', trail: K.radio(prefs.theme === 'dark'), do: 'theme', arg: 'dark' },
          { id: 'light', title: 'Светлая тема', icon: 'sun', trail: K.radio(prefs.theme === 'light'), do: 'theme', arg: 'light' },
          { id: 'contrast', title: 'Увеличить контраст', sub: 'Более заметные границы и вторичный текст', trail: K.toggle(prefs.contrast), do: 'contrast' },
          { id: 'motion', title: 'Уменьшить движение', sub: 'Отключить анимацию и переходы', trail: K.toggle(prefs.motion), do: 'motion' },
        ]; break;
      case 'agents':
        out = [...(pr ? [{ id: 'project', title: 'Проект для переопределения', sub: pr.title, icon: 'folder', do: 'nextProject' }] : []), ...W.profiles.map((profile) => ({ id: profile.id, title: profile.title, sub: `${profile.command} ${effective(profile, pr ? pr.id : '') || ''}`.trim(), trail: profile.default ? K.chip('По умолчанию') : '', icon: 'agent', do: 'editProfile', arg: profile.id }))]; break;
      case 'prompts':
        out = [
          { id: 'edit', title: 'Сообщение коммита', sub: 'Редактировать шаблон', icon: 'draft', do: 'editPrompt' },
          { id: 'check', title: 'Проверить шаблон', icon: 'check', do: 'validatePrompt' },
          { id: 'save', title: 'Сохранить шаблон', sub: W.settings.promptDraft === W.commitPrompt ? 'Изменений нет' : 'Есть несохранённые изменения', do: 'savePrompt' },
          { id: 'revert', title: 'Вернуть сохранённый текст', icon: 'undo', do: 'revertPrompt' },
        ]; break;
      case 'keys':
        out = commandList(p).map((c) => ({ id: c.id, title: c.title, sub: c.grp || c.id, icon: c.icon || 'keys', trail: S.keysOf(c).length ? S.keysOf(c).map((key) => K.kbd(key)).join(' ') : 'Не назначено', do: 'recordKey', arg: c.id })); break;
      case 'accounts':
        out = W.accounts.filter((a) => a.ws === S.st.route.ws).map((a) => ({ id: a.id, title: a.host, sub: `${a.user || 'Не подключён'}, ${a.note || ''}`, icon: a.provider, trail: K.status(a.state === 'ok' ? 'ok' : 'err', a.state === 'ok' ? 'Подключён' : 'Токен истёк'), do: 'editAccount', arg: a.id })); break;
      case 'terminal':
        out = [
          { id: 'config', title: 'ghostty.conf', sub: 'Конфигурация только для чтения', icon: 'file', do: 'readConfig' },
          ...Object.values(W.sessions).filter((s) => s.kind === 'term' && S.wsOf(s.wb) === S.st.route.ws).map((s) => ({ id: s.id, title: s.title, sub: 'Открыть историю сессии', icon: 'term', do: 'terminalHistory', arg: s.id })),
        ]; break;
    }
    return section(p)[0] === 'keys' ? out : out.filter((r) => match(p, `${r.title} ${r.sub || ''}`));
  }
  const currentRow = (p) => { const list = rows(p); return list[S.clamp(p.state.row, 0, Math.max(0, list.length - 1))]; };
  function activate(p, id) {
    const list = rows(p); const i = id == null ? S.clamp(p.state.row, 0, Math.max(0, list.length - 1)) : list.findIndex((r) => r.id === id);
    const r = list[i]; if (!r) return;
    p.state.row = i; p.state.area = 'content';
    S.plugins.settings.actions[r.do].run(p, r.arg);
  }
  function move(p, delta) {
    if (p.state.area === 'list') {
      const list = sectionList(p); const index = list.findIndex((s) => s[0] === p.state.section);
      const next = list[S.clamp(index + delta, 0, Math.max(0, list.length - 1))];
      if (next) { p.state.section = next[0]; p.state.cur = sections.findIndex((s) => s[0] === next[0]); p.state.row = 0; }
    } else p.state.row = S.clamp(p.state.row + delta, 0, Math.max(0, rows(p).length - 1));
    S.render();
  }

  function editProfile(p, id) {
    const profile = W.profiles.find((x) => x.id === id); if (!profile) return;
    const pr = project(p);
    const error = (st) => !st.command.trim() ? 'Укажите команду запуска.' : /[\r\n\0]/.test(st.command + st.flags + st.projectFlags) ? 'Команда и флаги должны занимать одну строку.' : '';
    S.sheet({
      title: `Профиль ${esc(profile.title)}`, icon: 'agent', width: 520, sub: `Настройки проекта ${esc(pr.title)} имеют приоритет над глобальными. Уже запущенные сессии сохраняют свои параметры.`,
      state: { command: profile.command, flags: profile.flags || '', override: own(profile.override || {}, pr.id), projectFlags: effective(profile, pr.id) || '', isDefault: !!profile.default, error: '' },
      body: (o) => `${field('Команда', { bind: 'command', value: o.state.command, mono: true }, true)}${field('Глобальные флаги', { bind: 'flags', value: o.state.flags, mono: true, placeholder: '--model opus' })}
        ${K.row({ title: `Переопределить для ${esc(pr.title)}`, lead: K.check(o.state.override), do: 'override', trail: K.kbd('Alt-o') })}
        ${o.state.override ? field('Флаги проекта', { bind: 'projectFlags', value: o.state.projectFlags, mono: true, placeholder: 'Пустое значение отключает глобальные флаги' }) : ''}
        ${K.row({ title: 'Профиль по умолчанию', lead: K.radio(o.state.isDefault), do: 'default', trail: K.kbd('Alt-d') })}
        ${K.sec('Команда при следующем запуске')}${K.pad(`<div class="mono">${esc(`${o.state.command} ${o.state.override ? o.state.projectFlags : o.state.flags}`.trim())}</div>`)}
        ${o.state.error ? K.banner('err', 'Профиль не сохранён', esc(o.state.error)) : ''}`,
      on: { override: (_, o) => { o.state.override = !o.state.override; }, default: (_, o) => { o.state.isDefault = !o.state.isDefault; } },
      onBind: () => S.render(),
      actions: [
        { label: 'Отменить', key: 'Esc' },
        { label: 'Приоритет проекта', key: 'Alt-o', close: false, run: (o) => { o.state.override = !o.state.override; S.render(); } },
        { label: 'По умолчанию', key: 'Alt-d', close: false, run: (o) => { o.state.isDefault = !o.state.isDefault; S.render(); } },
        { label: 'Сохранить', kind: 'primary', key: 'Enter', close: false, run: (o) => {
          o.state.error = error(o.state); if (o.state.error) { S.render(); return; }
          profile.command = o.state.command.trim(); profile.flags = o.state.flags.trim(); profile.override = profile.override || {};
          if (o.state.override) profile.override[pr.id] = o.state.projectFlags.trim(); else delete profile.override[pr.id];
          if (o.state.isDefault) W.profiles.forEach((x) => { x.default = x === profile; });
          else if (profile.default && W.profiles.length > 1) { profile.default = false; W.profiles.find((x) => x !== profile).default = true; }
          S.ov.pop(o); p.state.notice = 'Профиль сохранён. При запуске используются флаги проекта, если задано переопределение.'; S.render();
        } },
      ],
    });
  }

  function editAccount(p, id) {
    const account = W.accounts.find((x) => x.id === id); if (!account) return;
    S.sheet({
      title: account.state === 'ok' ? `Токен ${esc(account.host)}` : `Обновить токен ${esc(account.host)}`, icon: account.provider, width: 460,
      sub: account.state === 'ok' ? 'Новый токен заменит сохранённый в прототипе.' : 'Срок токена истёк. Введите новый токен, чтобы восстановить доступ.',
      state: { token: '', error: '' },
      body: (o) => `${K.kv([['Сервер', esc(account.host)], ['Пользователь', esc(account.user || 'Не указан')], ['Состояние', esc(account.note || '')]])}${field('Новый токен', { bind: 'token', value: o.state.token, placeholder: 'Вставьте токен' }, true).replace('<input ', '<input type="password" autocomplete="off" ')}${o.state.error ? K.banner('err', 'Токен не сохранён', esc(o.state.error)) : ''}`,
      onBind: () => S.render(),
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Сохранить токен', kind: 'primary', key: 'Enter', close: false, run: (o) => {
        const token = o.state.token.trim();
        if (token.length < 8 || /\s/.test(token)) { o.state.error = 'Нужен токен длиной от 8 символов без пробелов. Проверьте значение.'; S.render(); return; }
        account.token = token; account.state = 'ok'; account.note = 'Токен обновлён в прототипе';
        o.state.token = ''; S.ov.pop(o); p.state.notice = `Токен ${account.host} обновлён.`; S.render();
      } }],
    });
  }

  const overlaps = (a, b) => a === b || a.startsWith(`${b} `) || b.startsWith(`${a} `);
  function conflicts(seq, id) {
    const exact = S.conflict(seq, id);
    return [...new Set([exact, ...S.commands.filter((c) => c.id !== id && S.keysOf(c).some((k) => overlaps(k, seq)))].filter(Boolean))];
  }
  function invalidKey(seq) {
    const first = seq.split(' ')[0];
    if (['Cmd-k', 'Ctrl-Space', 'Cmd-Space', 'Ctrl-\\'].includes(first)) return 'Сочетание занято системой или переключением режима. Запишите другое.';
    if (['j', 'k', 'h', 'l', 'Enter', 'Esc', 'i', 'Up', 'Down'].includes(first)) return 'Первую клавишу используют панели или режим ввода. Начните с сочетания с модификатором или Space.';
    if (seq.split(' ').length > 8) return 'Последовательность длиннее восьми клавиш. Сократите её.';
    return '';
  }
  function applyKey(p, c, seq, replace) {
    if (replace) conflicts(seq, c.id).forEach((other) => { S.st.keymap[other.id] = S.keysOf(other).filter((key) => !overlaps(key, seq)); });
    S.st.keymap[c.id] = [seq]; S.clearPending();
    p.state.notice = `Команда «${c.title}»: ${seq}. Привязка уже действует.`; S.render();
  }
  function recordKey(p, id) {
    const c = S.commands.find((x) => x.id === id); if (!c) return;
    let o; let capture;
    const clearCapture = () => { if (S.kb === capture) S.kb = null; };
    const save = () => {
      const seq = o.state.tokens.join(' ');
      o.state.error = seq ? invalidKey(seq) : 'Нажмите сочетание или последовательность клавиш.';
      if (o.state.error) { S.render(); return; }
      const collisions = conflicts(seq, c.id);
      clearCapture(); S.ov.pop(o);
      if (!collisions.length) { applyKey(p, c, seq, false); return; }
      S.sheet({
        title: 'Конфликт клавиш', icon: 'keys', width: 480,
        sub: `Последовательность ${esc(seq)} пересекается с существующими привязками.`,
        body: () => `${K.sec('Изменяемая команда')}${K.pad(esc(c.title))}${K.sec('Будут сняты пересекающиеся привязки')}${collisions.map((x) => K.row({ title: esc(x.title), sub: S.keysOf(x).filter((k) => overlaps(k, seq)).map(esc).join(', ') })).join('')}`,
        actions: [
          { label: 'Отменить', key: 'Esc' },
          { label: 'Записать другое', key: 'r', run: () => recordKey(p, c.id) },
          { label: 'Заменить', kind: 'primary', key: 'Enter', run: () => applyKey(p, c, seq, true) },
        ],
      });
    };
    o = S.sheet({
      title: 'Записать сочетание', icon: 'keys', width: 480, sub: esc(c.title), state: { tokens: [], error: '' },
      body: (x) => `${K.pad(x.state.tokens.length ? K.kbd(x.state.tokens.join(' ')) : 'Нажмите клавиши по порядку.')}${note('Enter сохраняет запись, Backspace удаляет последнюю клавишу. Ctrl-Enter добавляет Enter в последовательность.')}${x.state.error ? K.banner('err', 'Сочетание не записано', esc(x.state.error)) : ''}${K.btn('Добавить Enter', { do: 'enter', small: true, key: 'Ctrl-Enter' })} ${K.btn('Очистить запись', { do: 'clear', small: true })}`,
      on: { enter: () => { o.state.tokens.push('Enter'); o.state.error = ''; }, clear: () => { o.state.tokens = []; o.state.error = ''; } },
      actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Сохранить', kind: 'primary', key: 'Enter', close: false, run: save }],
    });
    capture = { owner: p.id, capture(tok, e) {
      if (!S.st.overlays.includes(o)) { clearCapture(); return; }
      if (e && (e.isComposing || e.repeat)) return;
      if (tok === 'Esc') { S.ov.pop(o); return; }
      if (tok === 'Enter') { save(); return; }
      if (tok === 'Ctrl-Enter') { o.state.tokens.push('Enter'); o.state.error = ''; S.render(); return; }
      if (tok === 'Backspace') o.state.tokens.pop(); else o.state.tokens.push(tok);
      o.state.error = ''; S.render();
    } };
    S.kb = capture; o.onClose = clearCapture;
  }

  function terminalHistory(p, id) {
    const session = W.sessions[id]; if (!session) return;
    const panel = Object.values(S.st.panels).find((x) => x.plugin === 'terminal' && x.state.sid === id);
    const hist = panel ? panel.state.hist || [] : [];
    S.sheet({
      title: `История ${esc(session.title)}`, icon: 'term', width: 520,
      body: () => hist.length ? K.code(hist) : K.empty({ title: 'История команд пуста', text: 'Откройте терминал и выполните команду.' }),
      actions: [{ label: 'Закрыть', key: 'Esc' }, { label: 'Открыть терминал', kind: 'primary', key: 'Enter', run: () => S.openSession(id) }],
    });
  }
  function readConfig() {
    const terminal = W.settings.terminal;
    S.sheet({ title: 'ghostty.conf', icon: 'file', width: 520, sub: `${terminal.path}. Только чтение.`, body: () => K.field({ bind: 'config', value: terminal.config, rows: 10, mono: true, ro: true }), actions: [{ label: 'Закрыть', kind: 'primary', key: 'Enter' }] });
  }

  function sidebar(p) {
    const list = sectionList(p);
    return `${K.ctx('Разделы')}${K.scroll(list.map(([id, title, icon]) => K.row({ k: id, title, icon, on: p.state.section === id, quiet: p.state.area !== 'list' || S.focused() !== p, do: 'section', arg: id })).join('') || K.empty({ title: 'Раздел не найден', actions: K.btn('Сбросить фильтр', { do: 'clearFilter' }) }))}`;
  }
  function content(p, geo) {
    const [id, title] = section(p);
    const list = rows(p);
    p.state.row = S.clamp(p.state.row, 0, Math.max(0, list.length - 1));
    const rowHtml = list.map((r, i) => K.row({ k: r.id, title: esc(r.title), sub: r.sub ? esc(r.sub) : '', icon: r.icon, trail: r.trail, on: i === p.state.row && p.state.area === 'content', quiet: S.focused() !== p, do: 'activate', arg: r.id, tall: geo.size !== 'ShortBar' })).join('');
    let before = ''; let after = '';
    if (id === 'appearance') after = `${K.sec('Предпросмотр темы')}${K.pad(`${K.chip('Выбрано', { tone: 'info' })} ${K.chip('В работе', { tone: 'wait' })} ${K.chip('Готово', { tone: 'ok' })}`)}${K.row({ title: 'Строка в фокусе', sub: 'Тема применяется сразу ко всему приложению', on: true, quiet: true })}`;
    if (id === 'agents') before = K.pad('Флаги проекта заменяют глобальные целиком. Пустое переопределение запускает агента без глобальных флагов.');
    if (id === 'prompts') {
      before = K.pad(`${field('Шаблон сообщения коммита', { bind: 'prompt', value: W.settings.promptDraft, rows: geo.size === 'ShortBar' ? 4 : 10 })}${note('Diff из index передаётся вместе с шаблоном. После проверки сохраните изменения.')}`);
      if (W.settings.promptResult) after = K.banner(W.settings.promptResult.error ? 'err' : 'ok', W.settings.promptResult.error ? 'Проверьте шаблон' : 'Шаблон проверен', esc(W.settings.promptResult.text));
    }
    if (id === 'keys') before = K.pad(field('Найти команду', { bind: 'keyFilter', value: p.state.keyFilter || '', placeholder: 'Название, команда или клавиши' }));
    if (id === 'accounts') before = W.accounts.filter((a) => a.ws === S.st.route.ws && a.state !== 'ok').map((a) => K.banner('err', `Токен ${esc(a.host)} истёк`, 'Обновите токен, чтобы восстановить доступ.', K.btn('Обновить', { do: 'editAccount', arg: a.id, small: true }))).join('');
    if (id === 'terminal') {
      const t = W.settings.terminal;
      before = K.pad(`${K.kv([['Шрифт', esc(t.font)], ['Размер', `${t.size} пт`], ['История вывода', `${S.fmt.num(t.history / 1000000)} МБ`]])}${note('Параметры терминала читаются из ghostty.conf. Конфигурация в этой панели доступна только для чтения.')}`);
      after = K.pad(K.field({ bind: 'terminalConfig', value: t.config, rows: 4, mono: true, ro: true }));
    }
    return `${K.ctx(title, geo.size === 'Narrow' ? K.btn('К разделам', { do: 'back', small: true, kind: 'flat' }) : '')}${K.scroll(`${p.state.notice ? K.banner('ok', 'Сохранено', esc(p.state.notice), K.btn('Закрыть', { do: 'dismiss', small: true })) : ''}${before}${rowHtml || K.empty({ title: 'Ничего не найдено', actions: K.btn('Сбросить фильтр', { do: 'clearFilter' }) })}${after}`)}`;
  }
  function savePrompt(p, checkOnly) {
    const error = promptError(W.settings.promptDraft);
    W.settings.promptResult = { error: !!error, text: error || 'Шаблон заполнен, не содержит неподдерживаемых переменных и готов к использованию.' };
    if (!error && !checkOnly) { W.commitPrompt = W.settings.promptDraft; p.state.notice = 'Шаблон сообщения коммита сохранён.'; }
    S.render();
  }

  S.plugin({
    id: 'settings', title: 'Настройки', icon: 'gear', cardinality: 'app', defaultZone: 'center', placements: ['left', 'right', 'center', 'bottom'], minSize: { w: 300, h: 200 }, inputDefault: false,
    init(p, opts = {}) { Object.assign(p.state, { area: 'list', section: sections.some((s) => s[0] === opts.section) ? opts.section : 'general', cur: 0, row: 0, project: W.workbenches[p.wb] ? W.workbenches[p.wb].project : null, keyFilter: '', notice: '' }); if (opts.section) p.state.area = 'content'; },
    reopen(p, opts = {}) { if (opts.moved && W.workbenches[p.wb]) p.state.project = W.workbenches[p.wb].project; if (opts.section) navigate(p, opts.section); },
    meter: () => ({ kind: 'estimate', text: '≈ 0% CPU · 1,8 МБ' }),
    context: (p) => [{ label: `Настройки: ${section(p)[1]}`, icon: 'gear' }],
    view(p, geo) {
      p.state.wide = ['center', 'bottom'].includes(geo.zone) && geo.size !== 'Narrow' && geo.width >= 620;
      const narrow = () => p.state.area === 'list' ? sidebar(p) : content(p, geo);
      const wide = () => geo.size === 'Narrow' || geo.width < 620 ? narrow() : K.cols([{ w: geo.size === 'Wide' ? '210px' : '180px', html: sidebar(p) }, { html: content(p, geo) }]);
      return K.adapt(geo, { left: narrow, right: narrow, center: wide, bottom: wide });
    },
    onBind(p, name, value) {
      if (name === 'prompt') { W.settings.promptDraft = value; W.settings.promptResult = null; p.state.notice = ''; }
      if (name === 'keyFilter') { p.state.keyFilter = value; p.state.row = 0; }
      S.render();
    },
    destroy(p) { if (S.kb && S.kb.owner === p.id) S.kb = null; },
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
      open: { title: 'Открыть или изменить', key: ['Enter', 'l'], hint: 'Открыть', run: (p) => { if (p.state.area === 'list') { const s = sectionList(p).find((x) => x[0] === p.state.section) || sectionList(p)[0]; if (s) navigate(p, s[0]); } else activate(p); } },
      back: { title: 'К разделам', key: 'h', hint: 'Назад', when: (p) => p.state.area === 'content', run: (p) => { p.state.area = 'list'; p.filter = ''; S.render(); } },
      section: { title: '', menu: false, run: navigate },
      toggleCross: { title: '', menu: false, run: () => { S.st.prefs.crossAttention = !S.st.prefs.crossAttention; S.render(); } },
      toggleQuit: { title: '', menu: false, run: () => { S.st.prefs.quitSilent = !S.st.prefs.quitSilent; S.render(); } },
      activate: { title: '', menu: false, run: activate },
      switchWorkbench: { title: 'Переключить рабочую область', when: (p) => inSection(p, 'general'), run: () => S.switcher() },
      toggleZone: { title: '', menu: false, run: (p, zone) => S.toggleZone(zone) },
      theme: { title: '', menu: false, run: (p, theme) => { S.setTheme(theme); } },
      contrast: { title: 'Переключить контраст', when: (p) => inSection(p, 'appearance'), run: () => S.setPref('contrast', !S.st.prefs.contrast) },
      motion: { title: 'Переключить уменьшение движения', when: (p) => inSection(p, 'appearance'), run: () => S.setPref('motion', !S.st.prefs.motion) },
      nextProject: { title: 'Следующий проект для переопределения', when: (p) => inSection(p, 'agents'), run: (p) => { const ids = S.projectsIn(S.st.route.ws).filter((x) => !x.removedAt).map((x) => x.id); p.state.project = ids[(ids.indexOf(project(p).id) + 1) % ids.length]; S.render(); } },
      editProfile: { title: 'Изменить профиль агента', when: (p) => inSection(p, 'agents') && currentRow(p) && currentRow(p).id !== 'project', run: (p, id) => editProfile(p, id || (currentRow(p) || {}).id) },
      editPrompt: { title: 'Редактировать шаблон', key: 'e', when: (p) => inSection(p, 'prompts'), hint: 'Изменить', run: (p) => { p.state.area = 'content'; S.render(); focusField(p, 'prompt'); } },
      validatePrompt: { title: 'Проверить шаблон', key: 'v', when: (p) => inSection(p, 'prompts'), run: (p) => savePrompt(p, true) },
      savePrompt: { title: 'Сохранить шаблон', key: 'Cmd-Enter', when: (p) => inSection(p, 'prompts'), run: (p) => savePrompt(p, false) },
      revertPrompt: { title: 'Вернуть сохранённый шаблон', when: (p) => inSection(p, 'prompts'), run: (p) => { W.settings.promptDraft = W.commitPrompt; W.settings.promptResult = null; S.render(); } },
      recordKey: { title: 'Записать сочетание', key: 'r', hint: 'Записать', when: (p) => inSection(p, 'keys') && !!currentRow(p), run: (p, id) => recordKey(p, id || currentRow(p).id) },
      removeKey: { title: 'Убрать привязку команды', key: 'x', when: (p) => inSection(p, 'keys') && !!currentRow(p), run: (p) => { const row = currentRow(p); if (!row) return; S.st.keymap[row.id] = []; p.state.notice = `Привязка команды «${row.title}» снята.`; S.clearPending(); S.render(); } },
      editAccount: { title: 'Обновить токен', key: 'e', when: (p) => inSection(p, 'accounts') && !!currentRow(p), run: (p, id) => editAccount(p, id || currentRow(p).id) },
      readConfig: { title: 'Открыть ghostty.conf', when: (p) => inSection(p, 'terminal'), run: readConfig },
      terminalHistory: { title: '', menu: false, run: terminalHistory },
      dismiss: { title: '', menu: false, run: (p) => { p.state.notice = ''; S.render(); } },
      clearFilter: { title: 'Сбросить фильтр', when: (p) => !!p.filter || !!p.state.keyFilter, run: (p) => { p.filter = ''; p.state.keyFilter = ''; p.state.row = 0; S.render(); } },
    },
  });
})();
