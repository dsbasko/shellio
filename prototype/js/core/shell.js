// Отрисовка окна: левая зона, тулбар, центр, низ, правая зона, статус-бар.
(() => {
  const St = S.st;
  const { cx, icon } = S;

  const tabHtml = (L, id, active, zone, ck) => {
    const p = St.panels[id]; const def = S.plugins[p.plugin];
    const title = def.tabTitle ? def.tabTitle(p) : def.title;
    const st = def.tabStatus ? def.tabStatus(p) : null;
    const count = def.tabCount ? def.tabCount(p) : null;
    const side = zone === 'left' || zone === 'right';
    return `<button type="button" class="${cx('tab', active === id && 'on')}" data-k="tab-${id}" data-tab="${id}" data-ck="${ck}">${side ? '' : icon(def.icon, 13, `${ck}.icon`)}<span class="tab-t" data-ck="${ck}.label">${title}</span>${count ? `<span class="cnt">${count}</span>` : ''}<span class="dot ${st || 'none'}"></span><span class="x" data-close="${id}" data-ck="${ck}.close">${icon('close', 10, `${ck}.close.icon`)}</span></button>`;
  };

  const meterHtml = (p) => {
    const def = S.plugins[p.plugin];
    const m = def.meter ? def.meter(p) : null;
    if (!m) return '<span class="spacer"></span>';
    const title = def.tabTitle ? def.tabTitle(p) : def.title;
    const tip = m.tip || (m.kind === 'estimate' ? 'Оценка: панель работает внутри процесса shellio' : m.kind === 'shared' ? 'Общая часть приложения' : `Измерено: процесс ${title}`);
    return `<span class="res ${m.kind === 'estimate' ? 'est' : 'meas'}" title="${tip}"><span>${m.text}</span></span>`;
  };

  // Подсказка кнопки называет действие и клавишу из действующего keymap.
  const tip = (label, id, usual) => { const k = S.keyFor(id, usual); return k ? `${label}  ${K.keyLine(k)}` : `${label}. Клавиша не назначена`; };
  const toolBtn = (cmd, name, label, keyId, usual, ck) => `<button type="button" class="ib sm" data-cmd="${cmd}" data-k="tool-${cmd}" aria-label="${label}" title="${tip(label, keyId, usual)}" data-ck="${ck}">${icon(name, 14, `${ck}.icon`)}</button>`;
  // Деление идёт по оси зоны. Навигатор не делится, развёрнутая панель не делит раскладку под собой.
  const toolsHtml = (L, g, zone, ck) => {
    if (L.zoom) return toolBtn('zoom', 'shrink', 'Вернуть раскладку', 'zoom', 'Ctrl-w z', `${ck}.restore`);
    const row = S.AXIS[zone] === 'row';
    const split = g.nav ? '' : row ? toolBtn('split.v', 'splitCols', 'Разделить: части рядом', 'split.v', 'Ctrl-w v', `${ck}.split`) : toolBtn('split.s', 'splitRows', 'Разделить: одна над другой', 'split.s', 'Ctrl-w s', `${ck}.split`);
    return `${split}${toolBtn('zoom', 'expand', 'Развернуть панель', 'zoom', 'Ctrl-w z', `${ck}.zoom`)}`;
  };

  // Ключ сверки части: номер — порядок частей зоны на экране, колонка навигатора — левая часть 1.
  const partKey = (L, g, zone) => {
    if (L.zoom) return 'zone.center.part.1';
    const column = zone === 'left' && !L.hidden.left && L.groups[L.nav].tabs.length ? 1 : 0;
    return `zone.${zone}.part.${g.nav ? 1 : S.partsOf(L, zone).indexOf(g) + 1 + column}`;
  };

  const groupHtml = (L, g, zone, view = { tabs: g.tabs, active: g.active }, key = g.id) => {
    const pk = partKey(L, g, zone);
    const activeP = view.active ? St.panels[view.active] : null;
    const def = activeP ? S.plugins[activeP.plugin] : null;
    const focus = activeP && L.focus === view.active;
    const attrs = `data-g="${g.id}" data-k="${key}" data-zone="${zone}" data-ck="${pk}"${activeP ? ` data-panel="${activeP.id}" data-active="${activeP.id}"` : ''}`;
    if (!activeP) return `<section class="group panel" ${attrs}><div class="zone-empty" data-ck="${pk}.body">${K.empty({ ck: `${pk}.body.empty`, icon: 'grid', title: 'Здесь пока ничего нет', text: 'Откройте терминал или запустите агента. Они появятся в этой зоне.', actions: `${K.btn('Терминал', { cmd: 'terminal.new', key: 'Space t', ck: `${pk}.body.empty.action.1` })}${K.btn('Агент', { cmd: 'agent.new', key: 'Space a', ck: `${pk}.body.empty.action.2` })}${K.btn('nvim', { cmd: 'editor.open', key: 'Space e', ck: `${pk}.body.empty.action.3` })}` })}</div></section>`;
    if (def.chromeless && zone === 'left' && view.tabs.length === 1) return `<section class="group panel flat" ${attrs}><div class="panel-body" data-keep data-body="${activeP.id}" data-ck="${pk}.body"></div></section>`;
    return `<section class="${cx('group panel', focus && 'is-focus')}" ${attrs}>
      <div class="tabbar" data-ck="${pk}.tabs"><div class="tabs">${view.tabs.map((id, i) => tabHtml(L, id, view.active, zone, `${pk}.tabs.tab.${i + 1}`)).join('')}</div>${meterHtml(activeP)}${toolsHtml(L, g, zone, `${pk}.tabs`)}<button type="button" class="ib sm" data-cmd="panel.menu" aria-label="Действия панели" title="Действия панели" data-ck="${pk}.tabs.menu">${icon('more', 14, `${pk}.tabs.menu.icon`)}</button></div>
      <div class="panel-body" data-keep data-body="${activeP.id}" data-ck="${pk}.body"></div>
    </section>`;
  };

  // Части зоны идут по её оси: рядом в центре и внизу, одна над другой слева и справа.
  const zoneHtml = (L, zone) => {
    const list = S.partsOf(L, zone);
    if (list.length === 1) return groupHtml(L, list[0], zone);
    const axis = S.AXIS[zone];
    // Доли нормируются: после удаления части соседи занимают освободившееся место.
    const total = list.reduce((n, g) => n + g.w, 0) || 1;
    return `<div class="parts by-${axis}" data-k="parts-${zone}">${list.map((g, i) => `${i ? `<div class="sash ${axis === 'row' ? 'w' : 'h'}" data-sash="part" data-zone="${zone}" data-i="${i}" data-k="ps-${g.id}" data-ck="zone.${zone}.divider.${i}"></div>` : ''}<div class="part" data-part="${g.id}" data-k="p-${g.id}" style="flex:${Math.round((g.w / total) * 1000) / 10} 1 0">${groupHtml(L, g, zone)}</div>`).join('')}</div>`;
  };

  const light = (cls, cmd, label, path, ck) => `<button type="button" class="tl ${cls}" data-cmd="${cmd}" aria-label="${label}" title="${label}" data-ck="chrome.window.${ck}"><svg viewBox="0 0 12 12" width="8" height="8" aria-hidden="true">${path}</svg></button>`;
  const lights = `<div class="lights">${light('tl-c', 'win.close', 'Закрыть', '<path d="M3.5 3.5l5 5M8.5 3.5l-5 5"/>', 'close')}${light('tl-m', 'win.min', 'Свернуть', '<path d="M3 6h6"/>', 'minimize')}${light('tl-z', 'win.zoom', 'Развернуть', '<path d="M3.5 8.5V4.2L7.8 8.5zM8.5 3.5v4.3L4.2 3.5z" style="fill:#0A4A14;stroke:none"/>', 'zoom')}</div>`;

  const leftParts = (L) => {
    const g = L.groups[L.nav];
    return { g, navId: g.tabs[0] || null, others: S.zoneTabs(L, 'left') };
  };

  const keyTip = (id, usual) => K.keyChips(S.keyFor(id, usual)).replace(/ /g, '');
  const zoneBtn = (on, arg, label, keys, name, ck) => `<button type="button" class="${cx('ib', on && 'on')}" data-cmd="zone.toggle" data-arg="${arg}" aria-pressed="${on}" aria-label="${label}" title="${label}  ${keys}" data-ck="${ck}">${icon(name, 15, `${ck}.icon`)}</button>`;

  const titlebar = (L) => {
    const wb = S.wb(); const pr = S.project(); const ws = S.ws();
    const crossCount = S.crossTotal();
    document.title = `${ws.title} — ${wb ? `${pr.title} / ${wb.title} — ` : ''}shellio`;
    const { others } = leftParts(L);
    const rightEmpty = !S.zoneTabs(L, 'right').length;
    const sideLeft = S.sideZone(L) === 'plate';
    const sideOn = sideLeft ? !L.hidden.plate : !L.hidden.right && !rightEmpty;
    // Кнопка показывает текущий выбор оформления из трёх, а не действие (docs/ux.md, поток 16).
    const choice = S.appearance();
    const themeLabel = { light: 'Оформление: светлое', dark: 'Оформление: тёмное', system: 'Оформление: системное' }[choice];
    return `<header class="titlebar" data-k="titlebar" data-ck="chrome">
      <div class="tb-left">
        ${lights}
        <button type="button" class="ws-btn" data-cmd="ws.switch" aria-label="Воркспейс: ${ws.title}" aria-haspopup="menu" title="Переключить воркспейс" data-ck="chrome.workspace"><span class="ws-name" data-ck="chrome.workspace.label">${ws.title}</span>${crossCount ? `<span class="ws-n" data-ck="chrome.workspace.count">${crossCount}</span>` : ''}${icon('chevD', 11, 'chrome.workspace.chevron')}</button>
      </div>
      <button type="button" class="search" data-cmd="palette" aria-label="Поиск и команды" data-ck="chrome.search">${icon('search', 14, 'chrome.search.icon')}<span data-ck="chrome.search.label">Сессии, файлы, команды</span>${S.keyFor('palette', 'Cmd-k') ? K.kbd(S.keyFor('palette', 'Cmd-k'), 'chrome.search.kbd') : ''}</button>
      <div class="tb-right">
        <div class="seg" role="group" aria-label="Зоны" data-k="zones" data-ck="chrome.zones">
          ${zoneBtn(!L.hidden.left, 'left', 'Навигация', keyTip('zone.left', 'Cmd-0'), 'sideL', 'chrome.zones.left')}
          ${zoneBtn(!L.hidden.bottom, 'bottom', 'Нижняя зона', keyTip('zone.bottom', 'Cmd-j'), 'sideB', 'chrome.zones.bottom')}
          ${zoneBtn(sideOn, sideLeft ? 'plate' : 'right', sideLeft ? 'Левая зона' : 'Правая зона', keyTip('zone.right', 'Cmd-Alt-0'), sideLeft ? 'sideLP' : 'sideR', 'chrome.zones.right')}
        </div>
        <button type="button" class="ib" data-cmd="theme.toggle" aria-label="${themeLabel}" title="${themeLabel}" data-ck="chrome.appearance">${icon({ light: 'sun', dark: 'moon', system: 'appearanceSystem' }[choice], 15, 'chrome.appearance.icon')}</button>
      </div>
    </header>`;
  };

  const hintsHtml = () => {
    const mode = S.mode();
    const p = S.focused();
    const def = p && S.plugins[p.plugin];
    const h = (keys, text, cls = '') => `<span class="h">${K.kbd(keys)}<em${cls ? ` class="${cls}"` : ''}>${text}</em></span>`;
    const opt = (keys, text) => (keys ? h(keys, text) : '');
    if (St.pending.length) return `<span class="h">${K.kbd(St.pending.join(' '))}</span>`;
    if (mode === 'input') {
      if (S.isField(document.activeElement)) return h('Esc', 'навигация');
      const target = def ? (def.inputTarget ? def.inputTarget(p) : (def.tabTitle ? def.tabTitle(p) : def.title)) : '';
      const extra = def && def.inputHints ? def.inputHints(p).map(([k, v]) => h(k, v)).join('') : '';
      return `<span class="h"><em class="to">Клавиши идут в ${target}</em></span>${h(S.modeKey(), 'навигация')}${extra}`;
    }
    const local = def && !def.inputDefault ? Object.values(def.actions || {}).filter((a) => a.hint && (!a.when || a.when(p))).slice(0, 3) : [];
    if (local.length) return local.map((a) => h([].concat(a.key)[0], String(a.hint === true ? a.title : typeof a.hint === 'function' ? a.hint(p) : a.hint).toLowerCase())).join('');
    const typing = def && def.inputDefault && !(def.surfaceless && def.surfaceless(p));
    const tabs = ['1', '2', '3'].map((n) => S.keyFor(`tab.${n}`, n)).filter(Boolean).join(' ');
    return `${opt(S.navKeys(), 'панели')}${opt(tabs, 'вкладки')}${opt(S.keyFor('palette', 'Cmd-k'), 'поиск')}${typing ? h(S.modeKey(), 'ввод') : ''}`;
  };

  const agentsText = (agents) => {
    const waiting = agents.filter((a) => a.state === 'waiting');
    const running = agents.filter((a) => a.state === 'running').length;
    if (waiting.length === 1) return { wait: true, text: `1 ${S.stateName(waiting[0])}` };
    if (waiting.length > 1) return { wait: true, text: `${waiting.length} ждут` };
    if (running) return { wait: false, text: S.fmt.plural(running, 'работает', 'работают', 'работают') };
    return { wait: false, text: 'простаивают' };
  };

  const toastHtml = () => {
    const list = S.toasts();
    if (!list.length) return '';
    const t = S.shownToast();
    const tone = t.tone === 'err' || t.tone === 'ok' || t.tone === 'wait' ? t.tone : '';
    const more = list.length > 1 ? `<button type="button" class="toast-n" data-cmd="toast.list" title="Все уведомления" aria-label="Все уведомления: ${list.length}">+${list.length - 1}</button>` : '';
    return `<span class="${cx('toast', (t.action || more) && 'act')}" role="status" data-k="toast">${tone ? `<span class="dot ${tone}"></span>` : ''}<span class="toast-t">${t.title}</span>${t.text ? ` <span class="toast-s" title="${S.esc(t.text.replace(/<[^>]+>/g, ''))}">${t.text}</span>` : ''}${t.action ? `<button type="button" class="lnk" data-cmd="toast.act" data-arg="${t.id}">${t.action.label}</button>` : ''}${more}</span>`;
  };

  // Строка статуса сцены сверки: только то, что назвала сцена, без сводок демо-мира.
  const sceneStatus = (sc) => {
    const msg = St.statusMessage || sc.message;
    const hints = (sc.hints || []).map((h, i) => `<span class="h" data-ck="status.hint.${i + 1}">${K.kbd(h.key, `status.hint.${i + 1}.key`)}<em data-ck="status.hint.${i + 1}.label">${h.label}</em></span>`).join('');
    return `<footer class="status" data-k="status" data-ck="status">
      ${sc.mode ? `<button type="button" class="mode" data-cmd="mode.toggle" aria-label="Режим клавиатуры: ${sc.mode === 'input' ? 'ввод' : 'навигация'}" data-ck="status.mode">${sc.mode === 'input' ? 'ВВОД' : 'НАВИГАЦИЯ'}</button>` : ''}
      ${msg ? `<span class="sb-msg" role="status" data-ck="status.message">${msg}</span>` : `<div class="hints">${hints}</div>`}
      ${sc.load ? `<div class="sb-right"><button type="button" class="sb num" data-cmd="status.resources" data-ck="status.load"><span class="lab" data-ck="status.load.label">shellio</span><span data-ck="status.load.value">${sc.load}</span></button></div>` : ''}
    </footer>`;
  };

  const statusbar = () => {
    if (St.scene) return sceneStatus(St.scene.status || {});
    const mode = S.mode();
    const repo = S.repo();
    const L = S.layout();
    const agents = S.agents();
    const wb = S.wb();
    const pl = repo ? W.pipelines.filter((x) => x.repo === repo.id).sort((a, b) => b.id - a.id) : [];
    const failed = wb ? W.pipelines.find((x) => x.state === 'failed' && S.project().repos.includes(x.repo)) : null;
    const cur = pl[0];
    const t = S.totals();
    const mrDrafts = Object.values(W.mrs).filter((m) => S.repoWs(m.repo) === St.route.ws).reduce((a, m) => a + m.drafts.length, 0);
    const update = W.update.state;
    const ag = agentsText(agents);
    const sep = '<span class="vsep"></span>';
    const glyph = (r) => (r.state === 'running' ? '<span class="r">◐</span>' : r.state === 'failed' ? '<span class="e">✕</span>' : r.state === 'ok' ? '<span class="c-ok">✓</span>' : '');
    const shown = S.shownToast();
    const sepOpt = '<span class="vsep sb-opt"></span>';
    return `<footer class="${cx('status', shown && 'has-toast', shown && (shown.action || S.toasts().length > 1) && 'has-act')}" data-k="status">
      <button type="button" class="mode" data-cmd="mode.toggle" title="Переключить режим  ${S.modeKey()}" aria-label="Режим клавиатуры: ${mode === 'input' ? 'ввод' : 'навигация'}">${mode === 'input' ? 'ВВОД' : 'НАВИГАЦИЯ'}</button>
      ${St.statusMessage ? `<span class="sb-msg" role="status">${St.statusMessage}</span>` : `<div class="hints">${hintsHtml()}</div>`}
      ${toastHtml()}
      <div class="sb-right">
        ${L.zoom ? `<button type="button" class="sb" data-cmd="zoom">${icon('expand', 13)}Вернуть раскладку</button>${sep}` : ''}
        ${update !== 'none' ? `<button type="button" class="sb" data-cmd="update">${icon('update', 13)}<span class="${update === 'blocked' ? 'w' : ''}">${update === 'blocked' ? 'Обновление ждёт сессий' : `Обновление ${W.update.version}`}</span></button>${sep}` : ''}
        ${wb ? `<button type="button" class="sb" data-cmd="status.branch" title="Ветка рабочей области: ${repo.id}">${icon('branch', 13)}${W.changes.detached ? `detached ${W.changes.head}` : wb.branch}</button>` : ''}
        ${cur || failed ? `${sep}<button type="button" class="sb" data-cmd="pipelines" title="Открыть пайплайны"><span class="lab">CI</span>${cur ? `<span class="hit" data-cmd="pipelines.run" data-arg="${cur.id}" title="Открыть пайплайн #${cur.id}">${glyph(cur)}#${cur.id}</span>` : ''}${failed && (!cur || failed.id !== cur.id) ? `<span class="e" data-cmd="pipelines.failed" title="Открыть лог упавшего задания">✕ #${failed.id} ${failed.failed}</span>` : ''}</button>` : ''}
        ${wb ? sep : ''}<button type="button" class="sb" data-cmd="${ag.wait ? 'agents.waiting' : wb ? 'agents.find' : 'status.none'}" title="${ag.wait ? 'Перейти к агенту, который ждёт' : 'Найти агента'}"><span class="lab">Агенты</span>${ag.wait ? `<span class="dot wait"></span><span class="w">${ag.text}</span>` : `<span>${ag.text}</span>`}</button>
        ${mrDrafts ? `${sepOpt}<button type="button" class="sb sb-opt" data-cmd="review.drafts" title="Черновики ревью"><span class="lab">Ревью</span>${S.fmt.plural(mrDrafts, 'черновик', 'черновика', 'черновиков')}</button>` : ''}
        ${sepOpt}<button type="button" class="sb num sb-opt" data-cmd="status.resources" title="Потребление shellio целиком"><span class="lab">shellio</span>${S.fmt.num(t.cpu)}% CPU · ${S.fmt.mem(t.mem)}</button>
      </div>
    </footer>`;
  };

  function shellHtml() {
    const L = S.layout();
    if (L.zoom && St.panels[L.zoom]) {
      const g = S.groupOf(L, L.zoom);
      return `${titlebar(L)}<div class="work" data-k="work"><main class="zones nonav" data-k="zones"><div class="col-c" data-k="col"><div class="zone-c" data-k="zc" data-ck="zone.center">${groupHtml(L, g, 'center', { tabs: [L.zoom], active: L.zoom })}</div></div></main></div>${statusbar()}`;
    }
    const { g: navG, navId, others } = leftParts(L);
    const showNav = !L.hidden.left && !!navId;
    const plate = others.length && !L.hidden.plate;
    const showR = !L.hidden.right && S.zoneTabs(L, 'right').length;
    const showB = !L.hidden.bottom && S.zoneTabs(L, 'bottom').length;
    return `${titlebar(L)}
      <div class="work" data-k="work">
        ${showNav ? `<aside class="side" data-k="side" style="width:${L.size.left}px" aria-label="Навигация">${groupHtml(L, navG, 'left', { tabs: [navId], active: navId }, `${navG.id}-nav`)}<div class="sash w side-sash" data-sash="left" data-k="sl"></div></aside>` : ''}
        <main class="${cx('zones', !showNav && 'nonav')}" data-k="zones">
          ${plate ? `<div class="zone-r" data-k="zl" data-zone-el="left" data-ck="zone.left" style="width:${L.size.plate}px">${zoneHtml(L, 'left')}</div><div class="sash w" data-sash="plate" data-k="sp"></div>` : ''}
          <div class="col-c" data-k="col">
            <div class="zone-c" data-k="zc" data-zone-el="center" data-ck="zone.center">${zoneHtml(L, 'center')}</div>
            ${showB ? `<div class="sash h" data-sash="bottom" data-k="sb"></div><div class="zone-b" data-k="zb" data-zone-el="bottom" data-ck="zone.bottom" style="height:${L.size.bottom}px">${zoneHtml(L, 'bottom')}</div>` : ''}
          </div>
          ${showR ? `<div class="sash w" data-sash="right" data-k="sr"></div><div class="zone-r" data-k="zr" data-zone-el="right" data-ck="zone.right" style="width:${L.size.right}px">${zoneHtml(L, 'right')}</div>` : ''}
        </main>
      </div>
      ${statusbar()}`;
  }

  // Пороги ADR-0009 (проба P0.6), сравнения строгие: ширина ровно 480 — Regular.
  const sizeClass = (zone, w, h) => (h < 240 ? 'ShortBar' : w < 480 ? 'Narrow' : w >= 1100 ? 'Wide' : 'Regular');

  // Заглушка «Развернуть» (ADR-0009): подпись над одной кнопкой. В низком теле подпись уходит
  // раньше кнопки (18 + 8 + 26 и отступы по 12), у развёрнутой панели кнопки нет.
  function stub(p, def, geo, ck) {
    const expandable = !S.layout().zoom;
    const label = !expandable || geo.height >= 76;
    return `<div class="stub" data-ck="${ck}.stub">${label ? `<div data-ck="${ck}.stub.label">${def.title}: здесь тесно</div>` : ''}${expandable ? K.btn('Развернуть', { cmd: 'zoom', arg: p.id, ck: `${ck}.stub.expand` }) : ''}</div>`;
  }

  function renderPanels() {
    const L = S.layout();
    document.querySelectorAll('[data-body]').forEach((el) => {
      const p = St.panels[el.dataset.body]; if (!p) return;
      const def = S.plugins[p.plugin];
      const group = el.closest('.group');
      const zone = L.zoom ? 'center' : group.dataset.zone;
      const geo = { zone, width: el.clientWidth, height: el.clientHeight, size: sizeClass(zone, el.clientWidth, el.clientHeight) };
      p.geo = geo;
      const min = def.minSize || { w: 160, h: 90 };
      const fresh = el._shown !== p.id;
      el._shown = p.id;
      const html = geo.width < min.w || geo.height < min.h ? stub(p, def, geo, el.dataset.ck) : def.view(p, geo);
      S.morph(el, html);
      el.querySelectorAll('.term[data-follow]').forEach((t) => { if (t.dataset.stick !== '0') t.scrollTop = t.scrollHeight; });
      if (fresh && p.scrollTops) el.querySelectorAll('.scroll').forEach((s, i) => { if (p.scrollTops[i]) s.scrollTop = p.scrollTops[i]; });
      const cur = el.querySelector('[data-cur]');
      if (cur) {
        const key = (cur.getAttribute('data-k') || '') + cur.textContent.slice(0, 24);
        if (p._cur !== key) { p._cur = key; cur.scrollIntoView({ block: 'nearest' }); }
      }
      if (p.filtering) renderFilter(p, el);
      else { const stale = el.parentElement.querySelector('.pfilter'); if (stale) stale.remove(); }
    });
  }

  function renderFilter(p, el) {
    let box = el.parentElement.querySelector('.pfilter');
    if (!box) {
      box = document.createElement('div'); box.className = 'pfilter'; box.dataset.k = `pfilter-${p.id}`;
      box.innerHTML = `${icon('filter', 13)}<input placeholder="Фильтр" data-pfilter="${p.id}" data-own-esc>`;
      el.parentElement.appendChild(box);
      const inp = box.querySelector('input'); inp.value = p.filter || '';
      inp.addEventListener('input', () => { p.filter = inp.value; S.render(); });
      inp.addEventListener('keydown', (e) => { if (e.key === 'Escape' || e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); if (e.key === 'Escape') p.filter = ''; p.filtering = false; box.remove(); S.render(); } }, true);
      inp.focus();
    }
  }

  let queued = false;
  S.render = () => {
    if (queued) return; queued = true;
    requestAnimationFrame(() => { queued = false; S.renderNow(); });
  };
  const shownTab = {};
  S.renderNow = () => {
    document.documentElement.dataset.mode = S.mode();
    S.morph(document.getElementById('app'), shellHtml());
    renderPanels();
    document.querySelectorAll('.tabs').forEach((tabs) => {
      const on = tabs.querySelector('.tab.on');
      const g = tabs.closest('[data-g]').dataset.g;
      if (on && shownTab[g] !== on.dataset.tab) { shownTab[g] = on.dataset.tab; on.scrollIntoView({ inline: 'nearest', block: 'nearest' }); }
    });
    // Тесная строка вкладок первой отдаёт нагрузку: она остаётся в заголовке меню панели, обрезанное число не показывается.
    document.querySelectorAll('.tabbar .res').forEach((res) => {
      res.classList.remove('cut');
      const text = res.lastElementChild; const tabs = res.parentElement.querySelector('.tabs');
      res.classList.toggle('cut', (!!text && text.getBoundingClientRect().right > res.getBoundingClientRect().right + 0.5) || (!!tabs && tabs.scrollWidth > tabs.clientWidth + 1));
    });
    if (S.renderOverlays) S.renderOverlays();
  };

  // Мышь: одна точка входа для кнопок, вкладок и строк.
  document.addEventListener('mousedown', () => { St.kbNav = false; }, true);
  document.addEventListener('mousedown', (e) => {
    if (e.target.closest('#overlays') || e.target.closest('[data-tab]')) return;
    const groupEl = e.target.closest('.group[data-active]');
    if (!groupEl) return;
    const L = S.layout(); const id = groupEl.dataset.active;
    if (id && L.focus !== id) S.focus(id);
    const p = id && St.panels[id];
    if (p && S.plugins[p.plugin].onPointer) S.plugins[p.plugin].onPointer(p, e);
    if (p && S.plugins[p.plugin].inputDefault && e.target.closest('.panel-body') && !e.target.closest('button,[data-do],[data-cmd],input,textarea')) S.setMode('input');
  });

  document.addEventListener('click', (e) => {
    const picked = window.getSelection();
    if (picked && !picked.isCollapsed && String(picked) && !e.target.closest('button')) return;
    const closeEl = e.target.closest('[data-close]');
    if (closeEl) { e.stopPropagation(); S.close(closeEl.dataset.close); return; }
    const tab = e.target.closest('[data-tab]');
    if (tab && !St.dragged) { S.focus(tab.dataset.tab); return; }
    const el = e.target.closest('[data-do],[data-cmd]');
    if (!el) return;
    const arg = el.dataset.arg;
    if (el.dataset.cmd) { S.run(el.dataset.cmd, arg); return; }
    const ovEl = e.target.closest('[data-ov]');
    if (ovEl) {
      const o = St.overlays.find((x) => x.id === ovEl.dataset.ov);
      if (o && o.on && o.on[el.dataset.do]) { o.on[el.dataset.do](arg, o); S.render(); }
      return;
    }
    const panelEl = e.target.closest('.panel[data-panel]');
    if (!panelEl) return;
    S.action(St.panels[panelEl.dataset.panel], el.dataset.do, arg);
  });

  S.action = (p, name, arg) => {
    const a = (S.plugins[p.plugin].actions || {})[name];
    if (a) { a.run(p, arg); S.render(); }
  };

  document.addEventListener('input', (e) => {
    const b = e.target.dataset && e.target.dataset.bind; if (!b) return;
    const ovEl = e.target.closest('[data-ov]');
    if (ovEl) { const o = St.overlays.find((x) => x.id === ovEl.dataset.ov); if (o) { o.state[b] = e.target.value; if (o.onBind) o.onBind(b, e.target.value); } return; }
    const panelEl = e.target.closest('.panel[data-panel]'); if (!panelEl) return;
    const p = St.panels[panelEl.dataset.panel]; p.state[b] = e.target.value;
    const def = S.plugins[p.plugin]; if (def.onBind) def.onBind(p, b, e.target.value);
  });
  document.addEventListener('focusin', () => S.render());
  document.addEventListener('focusout', () => S.render());
  document.addEventListener('scroll', (e) => {
    if (!e.target.closest || !e.target.classList || !e.target.classList.contains('scroll')) return;
    const panelEl = e.target.closest('.panel[data-panel]'); if (!panelEl) return;
    const p = St.panels[panelEl.dataset.panel]; if (!p) return;
    p.scrollTops = Array.from(panelEl.querySelectorAll('.scroll')).map((s) => s.scrollTop);
  }, true);

  // Перетаскивание сплиттеров и вкладок.
  document.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    if (e.target.closest('.titlebar') && !e.target.closest('button,[data-cmd]') && S.native) S.native('drag');
    const sash = e.target.closest('[data-sash]');
    if (sash) { e.preventDefault(); dragSash(e, sash); return; }
    const tab = e.target.closest('[data-tab]');
    if (tab && !e.target.closest('[data-close]')) dragTab(e, tab);
  });

  function dragSash(e, sash) {
    const L = S.layout(); const kind = sash.dataset.sash; sash.classList.add('drag');
    const sx = e.clientX; const sy = e.clientY; const start = { ...L.size };
    let part = null;
    if (kind === 'part') {
      const zone = sash.dataset.zone; const i = +sash.dataset.i;
      S.partPx(L, zone);
      const list = S.partsOf(L, zone);
      part = { zone, i, a0: list[i - 1].w, b0: list[i].w, row: S.AXIS[zone] === 'row' };
    }
    const move = (ev) => {
      if (kind === 'left') L.size.left = S.clamp(start.left + ev.clientX - sx, 180, 520);
      else if (kind === 'plate') L.size.plate = S.clamp(start.plate + ev.clientX - sx, 200, 760);
      else if (kind === 'right') L.size.right = S.clamp(start.right - (ev.clientX - sx), 200, 760);
      else if (kind === 'bottom') L.size.bottom = S.clamp(start.bottom - (ev.clientY - sy), 90, 620);
      else if (part) S.resizePart(L, part.zone, part.i, part.a0, part.b0, part.row ? ev.clientX - sx : ev.clientY - sy);
      S.render();
    };
    const up = () => { sash.classList.remove('drag'); window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); };
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', up);
  }

  // Цель перетаскивания: часть целиком, край части вдоль оси зоны (новая часть) или скрытая зона.
  // Полоса края — 24 точки внутри тела и не шире его половины (ADR-0009).
  const EDGE = 24;
  function partTarget(id, x, y) {
    const L = S.layout(); const from = S.groupOf(L, id);
    for (const el of document.querySelectorAll('[data-zone-el] .group[data-g]')) {
      const r = el.getBoundingClientRect();
      if (x < r.left || x > r.right || y < r.top || y > r.bottom) continue;
      const g = L.groups[el.dataset.g]; if (!g) return null;
      const row = S.AXIS[g.zone] === 'row';
      const body = el.querySelector('.panel-body') || el;
      const b = body.getBoundingClientRect();
      const len = row ? b.width : b.height; const at = row ? x - b.left : y - b.top;
      const edge = Math.min(EDGE, len / 2);
      const inBody = y >= b.top;
      const side = inBody && at < edge ? 'before' : inBody && at > len - edge ? 'after' : null;
      const fits = () => { const stay = g.tabs.filter((x) => x !== id); return !stay.length || S.splitRoom(g, [S.st.panels[id].plugin, S.st.panels[stay.includes(g.active) ? g.active : stay[0]].plugin]).ok; };
      if (side && !(from === g && g.tabs.length === 1) && fits()) {
        const half = row ? { left: side === 'before' ? r.left : r.left + r.width / 2, top: r.top, width: r.width / 2, height: r.height } : { left: r.left, top: side === 'before' ? r.top : r.top + r.height / 2, width: r.width, height: r.height / 2 };
        return { t: { split: g.id, side }, r: { ...half, right: half.left + half.width, bottom: half.top + half.height }, title: 'Новая часть' };
      }
      if (from === g) return { none: true };
      return { t: { group: g.id }, r, title: L.parts[g.zone].length > 1 ? 'В эту часть' : S.ZONE_TITLE[g.zone] };
    }
    return null;
  }

  function dragTab(e, tab) {
    const id = tab.dataset.tab; const sx = e.clientX; const sy = e.clientY; let active = false; let ghost; let target = null;
    const zones = () => {
      const out = [];
      const add = (zone, sel, title) => { const el = document.querySelector(sel); if (el) out.push({ zone, r: el.getBoundingClientRect(), title }); };
      add('left', '.side', 'Левая зона'); add('left', '[data-k="zl"]', 'Левая зона'); add('center', '.zone-c', 'Центр'); add('bottom', '.zone-b', 'Нижняя зона'); add('right', '[data-k="zr"]', 'Правая зона');
      const app = document.getElementById('app').getBoundingClientRect();
      if (!out.find((x) => x.zone === 'left')) out.push({ zone: 'left', r: { left: app.left, top: app.top + 44, width: 120, height: app.height - 90, right: app.left + 120, bottom: app.bottom - 46 }, title: 'Левая зона' });
      if (!out.find((x) => x.zone === 'right')) out.push({ zone: 'right', r: { left: app.right - 120, top: app.top + 44, width: 120, height: app.height - 90, right: app.right, bottom: app.bottom - 46 }, title: 'Правая зона' });
      if (!out.find((x) => x.zone === 'bottom')) out.push({ zone: 'bottom', r: { left: app.left + 200, top: app.bottom - 150, width: app.width - 400, height: 110, right: app.right - 200, bottom: app.bottom - 40 }, title: 'Нижняя зона' });
      return out;
    };
    let cached = null; let dropEl;
    const move = (ev) => {
      if (!active && Math.hypot(ev.clientX - sx, ev.clientY - sy) < 6) return;
      if (!active) { active = true; St.dragged = true; ghost = document.createElement('div'); ghost.className = 'drag-ghost'; ghost.textContent = tab.textContent; document.body.appendChild(ghost); cached = zones(); dropEl = document.createElement('div'); dropEl.className = 'drop'; dropEl.style.display = 'none'; document.body.appendChild(dropEl); }
      ghost.style.left = `${ev.clientX + 10}px`; ghost.style.top = `${ev.clientY + 10}px`;
      const pt = partTarget(id, ev.clientX, ev.clientY);
      target = pt && pt.none ? null : pt || cached.find((z) => ev.clientX >= z.r.left && ev.clientX <= z.r.right && ev.clientY >= z.r.top && ev.clientY <= z.r.bottom) || null;
      if (target) { Object.assign(dropEl.style, { display: 'grid', left: `${target.r.left}px`, top: `${target.r.top}px`, width: `${target.r.width}px`, height: `${target.r.height}px` }); dropEl.textContent = target.title; } else dropEl.style.display = 'none';
    };
    const up = () => {
      window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up);
      if (active) { ghost.remove(); dropEl.remove(); if (target) S.moveTo(id, target.t || { zone: target.zone }); setTimeout(() => { St.dragged = false; }, 0); }
    };
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', up);
  }

  new ResizeObserver(() => S.render()).observe(document.getElementById('app'));
})();
