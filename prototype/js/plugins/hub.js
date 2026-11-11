// Переписка: треды между вами, агентами и терминалами. Агенты перечислены в «Сессиях» навигатора.
(() => {
  const { esc } = S;
  const states = {
    queued: ['В очереди', 'wait', 0, 'Сообщение ожидает доставки.'],
    delivered: ['Доставлено', 'idle', 25, 'Транспорт подтвердил доставку. Агент ещё не принял сообщение.'],
    accepted: ['Принято', 'idle', 50, 'Агент подтвердил приём. Выполнение ещё не завершено.'],
    running: ['В работе', 'run', 75, 'Агент выполняет поручение.'],
    replied: ['Получен ответ', 'ok', 100, 'Ответ агента добавлен в тред.'],
    completed: ['Выполнено', 'ok', 100, 'Агент подтвердил завершение поручения.'],
    failed: ['Ошибка', 'err', 100, 'Не удалось выполнить сообщение. Откройте агента и уточните причину.'],
    cancelled: ['Отменено', 'exit', 0, 'Поручение удалено из очереди и не будет доставлено.'],
    expired: ['Срок истёк', 'wait', 0, 'Срок сообщения истёк. Обсудите с агентом новый срок.'],
    unknown: ['Статус неизвестен', 'wait', 0, 'Подтверждение не получено. Сверьте статус; повтор запрещён, пока результат неизвестен.'],
  };
  const stateOf = (state) => states[state] || states.unknown;
  const terminal = (state) => ['replied', 'completed', 'failed', 'cancelled', 'expired'].includes(state);
  const waiting = (a) => a.state === 'waiting' || a.state === 'waiting_approval';
  const now = () => new Date().toLocaleTimeString('ru-RU', { hour: '2-digit', minute: '2-digit' });
  const recipient = (t) => typeof t.to === 'string' ? t.to : t.to && (t.to.agent || t.to.session);
  const agent = (t) => t && W.sessions[recipient(t)];
  S.threadTo = recipient;
  const sameScope = (t) => { const a = agent(t); return !!a && S.wsOf(a.wb) === S.wsOf(t.wb); };
  const contexts = (t) => (t.ctx || t.context || []).map((c) => typeof c === 'string' ? c : c.label || c.ref || 'Контекст');
  const textOf = (m) => m.text || m.body || '';
  const timers = new Map();
  W.hubEvents = W.hubEvents || [];

  function prepare(t) {
    const a = agent(t);
    if (!t.wb) t.wb = a ? a.wb : S.st.route.wb;
    if (t.generation == null && a) t.generation = typeof t.to === 'object' && t.to.generation != null ? t.to.generation : a.gen;
    if (!t.messages) t.messages = [];
    t.messages.forEach((m) => { if (!m.id) m.id = S.uid('msg'); });
    return t;
  }
  W.threads.forEach(prepare);
  // Тред виден и там, откуда отправлен, и там, где живёт получатель. Фильтр по агенту оставляет его треды в обе стороны.
  const sender = (t) => t.from && W.sessions[t.from];
  const threads = (p) => W.threads.map(prepare).filter((t) => {
    const a = agent(t); const f = sender(t);
    if ((t.homeWs || S.wsOf(t.wb)) !== S.wsOf(p.wb)) return false;
    if (!(t.wb === p.wb || (a && a.wb === p.wb) || (f && f.wb === p.wb))) return false;
    return !p.state.agentFilter || recipient(t) === p.state.agentFilter || t.from === p.state.agentFilter;
  });
  const addressee = (p, t) => { const a = agent(t); const to = a && a.wb !== p.wb ? `${recipient(t)}, ${W.workbenches[a.wb].title}` : recipient(t); return t.from ? `${t.from} → ${to}` : to; };
  const findThread = (p, id) => threads(p).find((t) => t.id === (id || p.state.thread));
  const outbound = (t) => [...t.messages].reverse().find((m) => m.from === 'Вы' || m.kind === 'reply') || t.messages[0];
  const obsolete = (t) => { const a = agent(t); return a && t.generation != null && a.gen !== t.generation; };
  const matches = (p, text) => S.match(text, p.filter || '') > 0;

  function record(t) {
    if (t.hubSeen === t.state) return;
    t.hubSeen = t.state;
    W.hubEvents.unshift({ id: S.uid('event'), thread: t.id, wb: t.wb, title: t.title, to: recipient(t), state: t.state, when: t.when || now() });
    W.hubEvents = W.hubEvents.slice(0, 120);
  }
  function change(t, state, message = outbound(t)) {
    t.state = state; t.when = now();
    if (message) message.state = state;
    record(t); S.render();
  }
  function stopDelivery(t) {
    (timers.get(t.id) || []).forEach(clearTimeout);
    timers.delete(t.id);
  }
  S.threadCancel = (t) => { stopDelivery(t); change(t, 'cancelled'); };

  // Доставка принадлежит сообщениям: дополнение встаёт в очередь треда и не отменяет предыдущие.
  const queues = new Map();
  S.simulateDelivery = (t, rc, added) => {
    prepare(t); record(t);
    const message = added || outbound(t);
    if (!message || (!added && (t.state === 'unknown' || terminal(t.state)))) return;
    if (!message.state) message.state = t.state;
    const queue = queues.get(t.id) || [];
    queues.set(t.id, queue);
    if (!queue.includes(message)) queue.push(message);
    if (added && queue[0] === message) change(t, 'queued', message);
    if (timers.get(t.id)) return;
    const steps = ['queued', 'delivered', 'accepted', 'running'];
    const schedule = (fn, ms) => {
      const timer = setTimeout(fn, ms);
      timers.set(t.id, [...(timers.get(t.id) || []), timer]);
    };
    const advance = () => {
      const current = queue[0];
      if (!current || !W.threads.includes(t) || terminal(t.state) || t.state === 'unknown') { stopDelivery(t); return; }
      const a = agent(t);
      if (a && !sameScope(t)) { t.deliveryNote = 'Получатель теперь в другом воркспейсе. Сообщение не доставлено.'; change(t, 'failed', current); stopDelivery(t); return; }
      if (obsolete(t)) { t.deliveryNote = 'Агент перезапущен. Сообщение осталось в прежнем поколении.'; stopDelivery(t); S.render(); return; }
      if (!a || a.host !== 'ok') { t.deliveryNote = 'Нет связи с получателем. Откройте агента и восстановите сессию.'; change(t, 'unknown', current); stopDelivery(t); return; }
      if (t.deadline && Date.parse(t.deadline) <= Date.now()) { change(t, 'expired', current); stopDelivery(t); return; }
      if ((current.state === 'queued' && a.state === 'running') || (['queued', 'delivered'].includes(current.state) && waiting(a))) { schedule(advance, 700); return; }
      delete t.deliveryNote;
      const index = steps.indexOf(current.state);
      if (index < 3) { change(t, steps[index + 1], current); schedule(advance, index === 2 ? 2400 : 700); return; }
      const result = t.kind === 'task' && current.kind !== 'reply' ? 'completed' : 'replied';
      const response = result === 'completed' ? 'Поручение выполнено. Результат и вывод проверки доступны в сессии агента.' : 'Сообщение принято. Контекст треда сохранён; подробности можно обсудить в сессии агента.';
      const back = { id: S.uid('msg'), reply_to: current.id, from: rc.title || a.title, text: response, state: 'replied', when: now() };
      // Ответ уходит тому поколению отправителя, что задало вопрос. Иначе он остаётся в треде до явной передачи.
      if (t.from) {
        const f = sender(t);
        // Обратный адрес — поколение автора вопроса или поколение, которому ответ передан явно.
        const gen = t.returnGen != null ? t.returnGen : t.fromGen;
        const same = !!f && f.host === 'ok' && f.state !== 'exited' && !f.forgotten && (gen == null || f.gen === gen);
        back.returnTo = same ? { id: t.from, gen } : null;
        if (!same) t.returnNote = `${t.from}${gen != null ? `, поколение ${gen},` : ''} ${!f || f.forgotten ? 'убран' : f.gen !== gen ? 'перезапущен' : 'недоступен'}. Ответ остался в треде и новому поколению не передан.`;
      }
      t.messages.push(back);
      change(t, result, current);
      queue.shift();
      if (queue.length) { change(t, queue[0].state, queue[0]); schedule(advance, 700); } else { queues.delete(t.id); stopDelivery(t); }
      S.notify({ ...S.threadToast(t.wb, a.wb), title: `Ответ от ${esc(a.title)}`, text: esc(t.title), action: { label: 'Открыть тред', run: () => S.open('threads', { wb: t.wb, thread: t.id }) } });
    };
    schedule(advance, 700);
  };

  function entries(p) {
    const ts = threads(p);
    ts.forEach(record);
    if (p.state.presentation === 'events') return W.hubEvents.filter((e) => ts.some((t) => t.id === e.thread) && matches(p, `${e.title} ${e.to} ${stateOf(e.state)[0]}`)).map((e) => ({ kind: 'event', id: e.id, value: e }));
    return [...ts.filter((t) => matches(p, `${t.title} ${recipient(t)} ${stateOf(t.state)[0]} ${contexts(t).join(' ')} ${t.messages.map(textOf).join(' ')}`)).map((t) => ({ kind: 'thread', id: t.id, value: t }))];
  }
  const selected = (p) => { const list = entries(p); return list[S.clamp(p.state.cur, 0, Math.max(0, list.length - 1))]; };
  function currentThread(p) {
    if (p.state.area === 'detail') return findThread(p);
    const item = selected(p);
    return item && (item.kind === 'thread' ? item.value : item.kind === 'event' ? findThread(p, item.value.thread) : null);
  }
  function currentAgent(p) {
    const t = currentThread(p);
    if (!t && p.state.agentFilter) { const a = W.sessions[p.state.agentFilter]; return a && S.wsOf(a.wb) === S.wsOf(p.wb) ? a : null; }
    return t && sameScope(t) ? agent(t) : null;
  }
  const canReply = (p) => { const t = currentThread(p); const a = agent(t); return !!(t && t.state !== 'unknown' && a && sameScope(t) && a.host === 'ok' && !obsolete(t)); };
  const canCancel = (p) => { const t = currentThread(p); return !!(t && t.kind === 'task' && t.state === 'queued' && (outbound(t) || {}).kind !== 'reply'); };
  const canHandBack = (p) => { const t = currentThread(p); const f = t && sender(t); return !!(t && t.returnNote && f && f.host === 'ok' && f.state !== 'exited' && !f.forgotten); };
  const canReassign = (p) => { const t = currentThread(p); return !!(t && canCancel(p) && obsolete(t) && sameScope(t) && agent(t).host === 'ok'); };

  function openItem(p, arg) {
    const list = entries(p);
    const i = arg == null ? S.clamp(p.state.cur, 0, Math.max(0, list.length - 1)) : list.findIndex((it) => `${it.kind}:${it.id}` === arg);
    const item = list[i]; if (!item) return;
    const previous = { ...p.state };
    S.mark(p, () => { Object.assign(p.state, previous); });
    p.state.cur = i; p.state.area = 'detail'; p.state.message = 0;
    p.state.thread = item.kind === 'thread' ? item.id : item.value.thread;
    S.render();
  }

  function reconcile(p) {
    const t = currentThread(p); if (!t || t.checking) return;
    t.checking = true; t.deliveryNote = 'Сверяем подтверждение по прежнему идентификатору сообщения.'; S.render();
    setTimeout(() => {
      t.checking = false;
      const a = agent(t);
      if (!a || a.host !== 'ok' || obsolete(t)) {
        t.deliveryNote = obsolete(t) ? 'Поколение агента изменилось. Приём старого сообщения не подтверждён. Повтор запрещён.' : 'Сверка не удалась: получатель недоступен. Восстановите связь с агентом и повторите сверку.';
        S.render(); return;
      }
      if (t.state === 'unknown') {
        // В прототипе подтверждение моделируется по тому же id, без новой отправки.
        change(t, waiting(a) ? 'delivered' : 'accepted');
        t.deliveryNote = 'Приём исходного сообщения подтверждён. Новое сообщение не отправлялось.';
        S.simulateDelivery(t, a);
      } else t.deliveryNote = `Статус подтверждён: ${stateOf(t.state)[0].toLowerCase()}.`;
      S.render();
    }, 650);
  }

  function reply(p) {
    if (!canReply(p)) return;
    const t = currentThread(p);
    S.composer('ask', {
      ctx: [{ label: t.title, icon: 'hub' }, ...contexts(t).map((label) => ({ label, icon: 'file' }))],
      reply: {
        id: t.id,
        to: addressee(p, t),
        error: () => t.replyError,
        send: (text, ctx) => {
          if (!canReply(p)) return false;
          const size = new TextEncoder().encode(text).length;
          if (size > 65536) { t.replyError = 'Сообщение больше 64 КиБ. Сократите текст и отправьте снова.'; S.render(); return false; }
          const messages = W.threads.filter((x) => recipient(x) === recipient(t) && !terminal(x.state)).flatMap((x) => x.messages || []);
          const bytes = messages.reduce((sum, m) => sum + new TextEncoder().encode(textOf(m)).length, 0);
          if (messages.length >= 256 || bytes + size > 1048576) { t.replyError = 'Ящик заполнен. Дождитесь обработки сообщений или отмените поручения из очереди.'; S.render(); return false; }
          delete t.replyError;
          const message = { id: S.uid('msg'), conversation: t.id, kind: 'reply', reply_to: t.messages.length ? t.messages[t.messages.length - 1].id : null, from: 'Вы', text, ctx, state: 'queued', when: now() };
          t.messages.push(message);
          S.simulateDelivery(t, agent(t), message);
          return true;
        },
      },
    });
  }

  const deliveryBar = (state) => { const [, tone, pct] = stateOf(state); return K.bar(pct, ['wait', 'err', 'ok'].includes(tone) ? tone : undefined); };
  const status = (state) => { const [label, tone] = stateOf(state); return `${K.status(tone, label)} ${deliveryBar(state)}`; };
  const backButton = () => K.btn('Назад', { do: 'back', small: true, kind: 'flat' });
  function listView(p) {
    const list = entries(p);
    p.state.cur = S.clamp(p.state.cur, 0, Math.max(0, list.length - 1));
    if (!list.length) return K.empty({ icon: 'hub', title: p.filter ? 'Ничего не найдено' : p.state.agentFilter ? `У ${esc(p.state.agentFilter)} пока нет тредов` : 'Пока нет тредов', text: p.filter ? 'Измените фильтр или очистите его.' : 'Задайте агенту вопрос с контекстом.', actions: p.filter ? K.btn('Сбросить фильтр', { do: 'clearFilter' }) : K.btn('Спросить агента', { do: 'ask' }) });
    return list.map((item, i) => {
      const value = item.value;
      const sub = `${item.kind === 'event' ? value.to : addressee(p, value)}, ${stateOf(value.state)[0].toLowerCase()}`;
      return K.row({ k: item.id, title: esc(value.title), sub: esc(sub), icon: 'hub', on: i === p.state.cur, quiet: p.state.area !== 'list' || S.focused() !== p, do: 'open', arg: `${item.kind}:${item.id}`, trail: `${deliveryBar(value.state)}<span>${esc(value.when || '')}</span>` });
    }).join('');
  }

  function detail(p) {
    const t = currentThread(p);
    const a = currentAgent(p);
    if (!t) return K.empty({ icon: 'hub', title: 'Выберите тред', actions: K.btn('Спросить агента', { do: 'ask' }) });
    const state = stateOf(t.state);
    const body = `${K.pad(`<div>${status(t.state)}</div><p>${esc(state[3])}</p>${K.kv([t.from && ['Отправитель', esc(t.fromGen != null ? `${t.from}, поколение ${t.fromGen}` : t.from)], t.returnGen != null && ['Ответы идут', `поколению ${t.returnGen}`], ['Получатель', esc(addressee(p, t).split(' → ').pop())], t.state === 'unknown' && ['Поколение', t.generation == null ? 'Неизвестно' : t.generation], t.state === 'unknown' && ['Сообщение', esc((outbound(t) || {}).id || t.id)], ['Тип', t.kind === 'task' ? 'Поручение' : 'Вопрос'], t.deadline && ['Срок', esc(t.deadline)]])}`)}
      ${t.state === 'unknown' ? K.banner('wait', 'Нужна сверка', 'Повторная отправка заблокирована до подтверждения исходного сообщения.') : ''}
      ${obsolete(t) ? K.banner('wait', 'Агент перезапущен', 'Сообщение адресовано прежнему поколению и не будет перенесено автоматически.', canReassign(p) ? K.btn('Переназначить', { do: 'reassign', small: true }) : '') : ''}
      ${agent(t) && !sameScope(t) ? K.banner('info', 'Только чтение', 'Получатель теперь в другом воркспейсе. История доступна, новые сообщения отправить нельзя.') : ''}
      ${t.deliveryNote ? K.banner(t.checking ? 'info' : 'wait', t.checking ? 'Сверяем статус' : 'Результат сверки', esc(t.deliveryNote)) : ''}
      ${t.replyError ? K.banner('err', 'Ответ не отправлен', esc(t.replyError)) : ''}
      ${t.returnNote ? K.banner('wait', 'Ответ не доставлен отправителю', esc(t.returnNote), canHandBack(p) ? K.btn(`Передать ${esc(t.from)}`, { do: 'handBack', small: true }) : '') : ''}
      ${K.pad(`${K.btn('Открыть агента', { do: 'openAgent', small: true, disabled: !a })} ${K.btn('Ответить', { do: 'reply', small: true, disabled: !canReply(p) })}${t.state === 'unknown' ? ` ${K.btn(t.checking ? 'Сверяем…' : 'Сверить статус', { do: 'reconcile', small: true, disabled: t.checking })}` : ''}${canCancel(p) ? ` ${K.btn('Отменить поручение', { do: 'cancel', small: true })}` : ''}`)}
      ${K.sec('Контекст', contexts(t).length)}${contexts(t).length ? K.pad(contexts(t).map((c) => `<div>${esc(c)}</div>`).join('')) : K.pad('Контекст не приложен.')}
      ${K.sec('Сообщения', t.messages.length)}${t.messages.map((m, i) => `<div class="inline-card" style="margin:6px 12px${p.state.area === 'detail' && i === p.state.message ? ';border-color:var(--nav-chip)' : ''}" data-k="${esc(m.id)}" data-do="selectMessage" data-arg="${i}"><div class="ic-h"><b style="font-weight:600;color:var(--text)">${esc(m.from || 'Агент')}</b><span>${esc(m.when || '')}</span></div><div class="ic-b">${esc(textOf(m)).replace(/\n/g, '<br>')}</div>${m.ctx && m.ctx.length ? `<div class="ic-b">${m.ctx.map((label) => `<div class="row-s">${esc(label)}</div>`).join('')}</div>` : ''}${m.state ? `<div class="ic-b">${status(m.state)}</div>` : ''}</div>`).join('')}`;
    return `${K.ctx(esc(t.title), p.state.area === 'detail' ? backButton() : '')}${K.scroll(body)}`;
  }
  function move(p, delta) {
    if (p.state.area === 'detail' && currentThread(p)) p.state.message = S.clamp(p.state.message + delta, 0, Math.max(0, currentThread(p).messages.length - 1));
    else p.state.cur = S.clamp(p.state.cur + delta, 0, Math.max(0, entries(p).length - 1));
    S.render();
  }
  function reopen(p, opts = {}) {
    if (opts.agent !== undefined && opts.agent !== p.state.agentFilter) { p.state.agentFilter = opts.agent || null; p.state.area = 'list'; p.state.cur = 0; }
    if (!opts.thread) return;
    const t = findThread(p, opts.thread); if (!t) return;
    const previous = { ...p.state }; S.mark(p, () => Object.assign(p.state, previous));
    Object.assign(p.state, { thread: t.id, area: 'detail', message: 0 }); p.filter = '';
  }

  S.plugin({
    id: 'threads', title: 'Переписка', icon: 'hub', cardinality: 'workbench', defaultZone: 'right', placements: ['left', 'right', 'center', 'bottom'], minSize: { w: 230, h: 90 }, inputDefault: false,
    init(p, opts) { Object.assign(p.state, { area: 'list', presentation: 'side', cur: 0, message: 0, thread: null, agentFilter: null }); reopen(p, opts); },
    reopen,
    tabTitle: (p) => (p.state.agentFilter ? `Переписка: ${p.state.agentFilter}` : 'Переписка'),
    tabStatus: (p) => threads(p).some((t) => t.state === 'unknown' || t.state === 'queued') ? 'wait' : null,
    meter: () => ({ kind: 'estimate', text: '≈ 0,1% CPU · 2,4 МБ' }),
    context: (p) => { const t = currentThread(p); return t ? [{ label: t.title, icon: 'hub' }, ...contexts(t).map((label) => ({ label, icon: 'file' }))] : []; },
    view(p, geo) {
      const presentation = geo.zone === 'bottom' ? 'events' : geo.zone === 'center' && geo.size !== 'Narrow' && geo.width >= 620 ? 'threads' : 'side';
      if (presentation !== p.state.presentation) { p.state.presentation = presentation; p.state.cur = 0; }
      const head = (label) => K.ctx(p.state.agentFilter ? `${K.chip(esc(p.state.agentFilter), { icon: 'agent', remove: 'allThreads' })}` : esc(label), K.btn('Спросить', { do: 'ask', small: true }));
      const narrow = () => p.state.area === 'detail' ? detail(p) : `${head(W.workbenches[p.wb].title)}${K.scroll(listView(p))}`;
      return K.adapt(geo, {
        left: narrow, right: narrow,
        center: () => presentation === 'side' ? narrow() : K.cols([{ w: geo.size === 'Wide' ? '300px' : '240px', html: `${head('Треды')}${K.scroll(listView(p))}` }, { html: detail(p) }]),
        bottom: () => p.state.area === 'detail' ? detail(p) : `${head(geo.size === 'ShortBar' ? 'Последние события' : 'События рабочей области')}${K.scroll(listView(p))}`,
      });
    },
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
      open: { title: 'Открыть детали', key: ['Enter', 'l'], hint: 'Открыть', run: (p, arg) => { if (arg != null || p.state.area === 'list') openItem(p, arg); } },
      back: { title: 'Назад', key: 'h', when: (p) => p.state.area === 'detail', hint: 'Назад', run: (p) => { p.state.area = 'list'; S.render(); } },
      allThreads: { title: 'Все треды', when: (p) => !!p.state.agentFilter, run: (p) => { p.state.agentFilter = null; p.state.area = 'list'; p.state.cur = 0; S.render(); } },
      openAgent: { title: 'Открыть агента', key: 'o', hint: 'Агент', when: (p) => !!currentAgent(p), run: (p) => { const a = currentAgent(p); if (a) S.openSession(a.id); } },
      reply: { title: 'Ответить в треде', key: 'r', when: canReply, run: reply },
      reconcile: { title: 'Сверить статус', key: 's', when: (p) => !!currentThread(p) && currentThread(p).state === 'unknown' && !currentThread(p).checking, run: reconcile },
      cancel: { title: 'Отменить поручение из очереди', key: 'x', when: canCancel, run: (p) => { if (!canCancel(p)) return; const t = currentThread(p); stopDelivery(t); change(t, 'cancelled'); } },
      handBack: { title: 'Передать ответ отправителю', when: canHandBack, run: (p) => {
        const t = currentThread(p); const f = sender(t); const last = t.messages[t.messages.length - 1];
        const target = f.gen; const was = t.returnGen != null ? t.returnGen : t.fromGen;
        S.sheet({ icon: 'agent', title: `Передать ответ ${esc(f.title)}?`, sub: `Ответ ждало поколение ${was}, сейчас работает поколение ${target}. Ответ уйдёт поколению ${target} отдельным сообщением.`, width: 460, actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Передать', kind: 'primary', key: 'Enter', run: () => {
          if (!canHandBack(p) || sender(t) !== f || f.gen !== target) { t.deliveryNote = `${f.title} снова перезапущен, передача поколению ${target} отменена. Подтвердите передачу заново.`; S.render(); return; }
          last.returnTo = { id: f.id, gen: target, explicit: true }; t.returnGen = target; delete t.returnNote; delete t.deliveryNote;
          S.notify({ ...S.owner(f.wb), title: `Ответ передан ${esc(f.title)}`, text: `Поколение ${target}.`, tone: 'ok' }); S.render();
        } }] });
      } },
      reassign: { title: 'Переназначить поручение', when: canReassign, run: (p) => {
        if (!canReassign(p)) return;
        const t = currentThread(p); const a = agent(t); const generation = a.gen;
        S.sheet({ icon: 'agent', title: 'Переназначить поручение', sub: `${esc(t.title)}. Получатель ${esc(a.title)}, поколение ${generation}.`, body: () => K.pad('Исходное поручение будет отменено в старой очереди. В новом поколении появится отдельное поручение с тем же текстом и контекстом.'), actions: [{ label: 'Отменить', key: 'Esc' }, { label: 'Переназначить', kind: 'primary', key: 'Enter', run: () => {
          if (t.state !== 'queued' || !obsolete(t) || a.host !== 'ok' || a.gen !== generation) { t.deliveryNote = 'Получатель или очередь изменились. Сверьте статус перед переназначением.'; S.render(); return; }
          const message = outbound(t); stopDelivery(t); change(t, 'cancelled');
          const next = { id: S.uid('th'), wb: t.wb, to: a.id, generation: a.gen, kind: 'task', title: t.title, state: 'queued', ctx: contexts(t), when: now(), messages: [{ id: S.uid('msg'), from: 'Вы', text: textOf(message), state: 'queued', when: now() }] };
          W.threads.unshift(next); reopen(p, { thread: next.id }); S.simulateDelivery(next, a); S.render();
        } }] });
      } },
      ask: { title: 'Спросить агента', key: 'a', run: (p) => { const a = currentAgent(p); S.composer('ask', a ? { to: a.id } : {}); } },
      showThread: { title: '', menu: false, run: (p, id) => { const t = findThread(p, id); if (!t) return; const previous = { ...p.state }; S.mark(p, () => Object.assign(p.state, previous)); Object.assign(p.state, { area: 'detail', thread: t.id, message: 0 }); S.render(); } },
      selectMessage: { title: '', menu: false, run: (p, i) => { const t = currentThread(p); if (!t) return; if (p.state.area !== 'detail') { S.mark(p, () => { p.state.area = 'list'; }); p.state.thread = t.id; p.state.area = 'detail'; } p.state.message = +i; S.render(); } },
      clearFilter: { title: 'Сбросить фильтр', when: (p) => !!p.filter, run: (p) => { p.filter = ''; p.state.cur = 0; S.render(); } },
    },
  });
})();
