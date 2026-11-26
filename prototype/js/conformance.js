// Детерминированный вход сверки (ADR-0037): работает только при ?conformance=1 и ставится до
// первого скрипта прототипа. Часы UTC 2026-01-01, монотонный счётчик от нуля, seed 1, очереди
// таймеров и кадров без самостоятельного хода, анимации в фазе 0. Сцена и рецепт — через
// window.conformance; готовность — window.conformance.ready.
(() => {
  if (new URLSearchParams(location.search).get('conformance') !== '1') return;

  const EPOCH = Date.UTC(2026, 0, 1);
  let mono = 0;
  const RealDate = Date;
  class FixedDate extends RealDate {
    constructor(...a) { if (a.length) super(...a); else super(EPOCH + mono); }
    static now() { return EPOCH + mono; }
  }
  window.Date = FixedDate;
  Object.defineProperty(performance, 'now', { value: () => mono, configurable: true });

  // mulberry32, seed 1.
  let seed = 1;
  Math.random = () => {
    seed = (seed + 0x6D2B79F5) | 0;
    let t = seed;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };

  // Таймеры ходят только по явному шагу step(ms).
  const timers = new Map(); let tid = 0;
  const addTimer = (fn, ms, args, every) => { const id = ++tid; timers.set(id, { fn, at: mono + Math.max(0, +ms || 0), args, every }); return id; };
  window.setTimeout = (fn, ms, ...args) => addTimer(fn, ms, args, null);
  window.setInterval = (fn, ms, ...args) => addTimer(fn, ms, args, Math.max(1, +ms || 1));
  window.clearTimeout = (id) => { timers.delete(id); };
  window.clearInterval = window.clearTimeout;

  // Кадры копятся и сбрасываются входом; настоящий кадр нужен только для ожидания раскладки.
  const realRaf = window.requestAnimationFrame.bind(window);
  const rafs = new Map(); let rid = 0;
  window.requestAnimationFrame = (fn) => { const id = ++rid; rafs.set(id, fn); return id; };
  window.cancelAnimationFrame = (id) => { rafs.delete(id); };
  const flush = () => {
    for (let round = 0; rafs.size && round < 100; round += 1) {
      const list = [...rafs.values()]; rafs.clear();
      list.forEach((fn) => fn(mono));
    }
  };
  const frame = () => new Promise((resolve) => realRaf(() => resolve()));

  const still = document.createElement('style');
  still.textContent = '*, *::before, *::after { animation-delay: 0s !important; animation-play-state: paused !important; transition: none !important; caret-color: transparent !important; }';
  document.head.appendChild(still);

  const step = (ms) => {
    const until = mono + ms;
    for (;;) {
      const due = [...timers.entries()].filter(([, t]) => t.at <= until).sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
      if (!due) break;
      const [id, t] = due;
      mono = Math.max(mono, t.at);
      if (t.every) t.at += t.every; else timers.delete(id);
      t.fn(...t.args);
    }
    mono = until;
    flush();
  };

  // Раскладка устойчива, когда два сброса подряд после настоящего кадра дают ту же разметку и кадры.
  const signature = () => {
    const app = document.getElementById('app');
    const rects = [...document.querySelectorAll('[data-ck]')].map((el) => { const r = el.getBoundingClientRect(); return `${el.dataset.ck}:${r.x},${r.y},${r.width},${r.height}`; });
    return `${app ? app.innerHTML : ''}|${rects.join(';')}`;
  };
  // Конечная анимация (появление поповера) доводится до конца, бесконечная стоит в фазе 0.
  const freeze = () => {
    document.getAnimations().forEach((a) => {
      const timing = a.effect && a.effect.getComputedTiming();
      if (timing && Number.isFinite(timing.endTime)) a.finish(); else { a.pause(); a.currentTime = 0; }
    });
  };
  const settle = async () => {
    await document.fonts.ready;
    let last = null;
    for (let i = 0; i < 50; i += 1) {
      flush();
      freeze();
      await frame();
      flush();
      freeze();
      const now = signature();
      if (now === last) return;
      last = now;
    }
    throw new Error('раскладка не устоялась за 50 кадров');
  };

  const ZONES = ['left', 'right', 'center', 'bottom'];
  const group = (zone) => ({ id: S.uid('g'), zone, tabs: [], active: null, w: 1 });

  // Сцена заменяет стартовый мир: каталог только из сцены, сценариев нет, раскладка из сцены.
  const load = async (scene) => {
    const St = S.st;
    const known = S.plugins;
    S.plugins = {};
    scene.catalog.forEach((c) => {
      const base = known[c.id] || { view: () => '' };
      S.plugins[c.id] = { ...base, id: c.id, title: c.title, icon: c.icon, placements: c.placements, defaultZone: c.default_zone, minSize: { w: c.min_size.width, h: c.min_size.height }, cardinality: { multi: 'multi', per_workbench: 'workbench', per_project: 'project', per_workspace: 'workspace', per_app: 'app' }[c.cardinality] };
      // Подпись вкладки, точка, счётчик, нагрузка и действия демо-мира в сцену не попадают.
      ['tabTitle', 'tabStatus', 'tabCount', 'meter', 'actions', 'inputHints', 'inputTarget', 'surfaceless'].forEach((k) => { delete S.plugins[c.id][k]; });
      // Получатель ввода есть только у плагина, которому его дала сцена. Режим клавиатуры сцена
      // не называет: он следует за фактическим получателем.
      S.plugins[c.id].inputDefault = !!(c.capabilities || {}).input;
      if (!known[c.id]) S.plugins[c.id].view = () => '';
    });
    S.scenarios.length = 0;
    St.scene = scene; St.statusMessage = null;
    // Возможности плагинов, отсутствующие действия и сводки правой части строки статуса — входы
    // сцены: по ним общая отрисовка строки статуса считает подсказки.
    St.sceneInputs = {
      capabilities: Object.fromEntries(scene.catalog.map((c) => [c.id, { input: false, list_keys: false, filter: false, hints: [], ...(c.capabilities || {}) }])),
      commandsAbsent: scene.commands_absent || [],
      right: { load: (scene.status || {}).load || null },
      keymapText: null,
    };
    // Воркспейс сцены единственный: подпись из сцены, сводок других воркспейсов нет.
    const ws = S.ws(); ws.title = scene.workspace;
    W.workspaces.length = 0; W.workspaces.push(ws);
    St.overlays = []; St.pending = [];
    S.resetLayouts();
    S.uid.reset();
    const L = S.layout();
    L.groups = {}; L.parts = { left: [], right: [], center: [], bottom: [] }; L.last = {}; L.zoom = null; L.focus = null;
    const nav = group('left'); nav.nav = true; L.groups[nav.id] = nav; L.nav = nav.id;
    const shown = {};
    scene.zones.forEach((z) => {
      shown[z.zone] = true;
      z.parts.forEach((part, i) => {
        const navPart = z.zone === 'left' && part.tabs.length === 1 && part.tabs[0].plugin === 'navigator';
        const g = navPart ? nav : group(z.zone);
        if (!navPart) { L.groups[g.id] = g; L.parts[z.zone].push(g.id); }
        part.tabs.forEach((t) => {
          const p = S.create(t.plugin, { zone: z.zone });
          // Панель начинает в Навигации: во «ВВОД» её переводит только поле с фокусом.
          p.mode = 'nav';
          g.tabs.push(p.id);
          if (t.active) g.active = p.id;
        });
        if (scene.focus.zone === z.zone && scene.focus.part === i + 1) L.focus = g.active;
      });
    });
    ZONES.forEach((zone) => { if (!L.parts[zone].length) { const g = group(zone); L.groups[g.id] = g; L.parts[zone].push(g.id); } });
    L.hidden = { left: !shown.left, right: !shown.right, bottom: !shown.bottom, plate: !L.parts.left.some((id) => L.groups[id].tabs.length) };
    S.setAppearance(scene.appearance);
    S.renderNow();
    await settle();
  };

  // Аккорд в записи keymap → поля события клавиши по физической клавише раскладки US.
  const SIGNS = { ';': ['Semicolon', 0], ':': ['Semicolon', 1], '/': ['Slash', 0], '?': ['Slash', 1], ',': ['Comma', 0], '<': ['Comma', 1], '.': ['Period', 0], '>': ['Period', 1], '[': ['BracketLeft', 0], '{': ['BracketLeft', 1], ']': ['BracketRight', 0], '}': ['BracketRight', 1], '\\': ['Backslash', 0], '|': ['Backslash', 1], '-': ['Minus', 0], '_': ['Minus', 1], '=': ['Equal', 0], '+': ['Equal', 1], "'": ['Quote', 0], '"': ['Quote', 1], '`': ['Backquote', 0], '~': ['Backquote', 1] };
  const NAMED = { Space: [' ', 'Space'], Enter: ['Enter', 'Enter'], Esc: ['Escape', 'Escape'], Tab: ['Tab', 'Tab'], Backspace: ['Backspace', 'Backspace'], Delete: ['Delete', 'Delete'], Up: ['ArrowUp', 'ArrowUp'], Down: ['ArrowDown', 'ArrowDown'], Left: ['ArrowLeft', 'ArrowLeft'], Right: ['ArrowRight', 'ArrowRight'] };
  const MODS = { Cmd: 'metaKey', Ctrl: 'ctrlKey', Alt: 'altKey', Shift: 'shiftKey' };
  const chord = (name) => {
    const e = { metaKey: false, ctrlKey: false, altKey: false, shiftKey: false };
    let rest = name;
    for (let m = rest.match(/^(Cmd|Ctrl|Alt|Shift)-(.+)$/); m; m = rest.match(/^(Cmd|Ctrl|Alt|Shift)-(.+)$/)) { e[MODS[m[1]]] = true; rest = m[2]; }
    if (NAMED[rest]) [e.key, e.code] = NAMED[rest];
    else if (/^[a-z]$/.test(rest)) { e.key = rest; e.code = `Key${rest.toUpperCase()}`; }
    else if (/^[A-Z]$/.test(rest)) { e.key = rest; e.code = `Key${rest}`; e.shiftKey = true; }
    else if (/^[0-9]$/.test(rest)) { e.key = rest; e.code = `Digit${rest}`; }
    else if (SIGNS[rest]) { e.key = rest; e.code = SIGNS[rest][0]; e.shiftKey = e.shiftKey || !!SIGNS[rest][1]; }
    else throw new Error(`аккорд ${name} не разобран`);
    return e;
  };
  // Нажатие и отпускание идут тому, у кого фокус, и всплывают к маршрутизатору прототипа. Текст в
  // поле синтетическая клавиша не печатает: его задаёт шаг filter.
  const press = (name) => {
    const init = { ...chord(name), bubbles: true, cancelable: true };
    const to = document.activeElement && document.activeElement !== document.body ? document.activeElement : document.body;
    to.dispatchEvent(new KeyboardEvent('keydown', init));
    to.dispatchEvent(new KeyboardEvent('keyup', init));
  };

  const chordsOf = (keys) => keys.split(' ').map(chord);

  // Часть шага: номер на экране, колонка навигатора — левая часть 1.
  const partOf = (s) => { const L = S.layout(); const el = document.querySelector(`.group[data-ck="zone.${s.zone}.part.${s.part}"]`); if (!el) throw new Error(`части zone.${s.zone}.part.${s.part} нет`); return L.groups[el.dataset.g]; };
  const STEPS = {
    hide_zone: (s) => { S.layout().hidden[s.zone] = true; },
    show_zone: (s) => { S.layout().hidden[s.zone] = false; },
    zoom: (s) => { const L = S.layout(); const g = S.groupOf(L, L.focus); L.zoom = (g && g.zone === s.zone ? L.focus : S.partsOf(L, s.zone)[s.part - 1].active); },
    split: (s) => { const g = partOf(s); S.focus(g.active); S.renderNow(); const room = S.splitRoom(g, [S.st.panels[g.active].plugin, s.plugin], { fresh: true }); if (!room.ok) throw new Error(`деление не помещается: ${S.roomText(room)}`); S.open(s.plugin, { zone: s.zone, at: { split: g.id, side: 'after' } }); },
    split_refused: (s) => { const g = partOf(s); S.focus(g.active); S.renderNow(); const stay = g.tabs.filter((x) => x !== g.active); const room = stay.length ? S.splitRoom(g, [S.st.panels[stay[0]].plugin, S.st.panels[g.active].plugin]) : S.splitRoom(g, [S.st.panels[g.active].plugin, s.plugin], { fresh: true }); if (room.ok) throw new Error('деление помещается, отказа нет'); S.refuse(S.roomText(room)); },
    open_menu: (s) => { S.focus(partOf(s).active); S.renderNow(); S.run('panel.menu'); },
    open_picker: (s) => { const g = partOf(s); S.focus(g.active); S.renderNow(); S.partPicker(g); },
    focus: (s) => { S.focus(partOf(s).active); },
    // Перенос вкладки в зону или в новую часть зоны. Место перенос не проверяет (ADR-0009).
    move: (s) => {
      const L = S.layout(); const from = partOf(s);
      const id = from.tabs.find((t) => S.st.panels[t].plugin === s.plugin); if (!id) throw new Error(`вкладки ${s.plugin} в части нет`);
      const full = S.partsOf(L, s.to).filter((g) => g.tabs.length);
      if (!s.new_part || !full.length) { if (from.zone === s.to) throw new Error('перенос в свою зону без новой части'); S.moveTo(id, { zone: s.to }); return; }
      const ref = from.zone === s.to ? from : full[full.length - 1];
      const room = S.splitRoom; S.splitRoom = () => ({ ok: true });
      try { S.moveTo(id, { split: ref.id, side: 'after' }); } finally { S.splitRoom = room; }
    },
    close_tab: (s) => { const id = partOf(s).tabs.find((t) => S.st.panels[t].plugin === s.plugin); if (!id) throw new Error(`вкладки ${s.plugin} в части нет`); S.close(id, { force: true }); },
    // Клавиши записи keymap по одной: между аккордами последовательности маршрутизатор ждёт префикс.
    key: (s) => { s.keys.split(' ').forEach((name) => { press(name); S.renderNow(); }); },
    // Правка keymap.toml: прототип TOML не разбирает и получает те же привязки под своими именами команд.
    keymap: (s) => {
      S.st.sceneInputs.keymapText = s.text;
      Object.entries(s.commands || {}).forEach(([id, keys]) => {
        keys.forEach(chordsOf);
        if (id === 'escape') { if (keys.length !== 1) throw new Error('аккорд выхода один'); S.st.keymap.escape = keys[0]; return; }
        if (!S.command(id)) throw new Error(`команды ${id} у прототипа нет`);
        S.st.keymap[id] = keys;
      });
    },
    // Фокус в поле активной панели части.
    focus_field: (s) => {
      const g = partOf(s); S.focus(g.active); S.renderNow();
      const el = document.querySelector(`.group[data-ck="zone.${s.zone}.part.${s.part}"]`);
      const field = el && [...el.querySelectorAll('input,textarea')].find((x) => S.isField(x) && x.dataset.pfilter == null);
      if (!field) throw new Error(`в части zone.${s.zone}.part.${s.part} нет поля`);
      field.focus();
      if (document.activeElement !== field) throw new Error(`поле части zone.${s.zone}.part.${s.part} не получило фокус`);
    },
    // Текст строки фильтра панели с фокусом.
    filter: (s) => {
      const p = S.focused(); if (!p) throw new Error('панели с фокусом нет');
      p.filtering = true; p.filter = s.text; S.renderNow();
      const inp = document.querySelector(`[data-pfilter="${p.id}"]`);
      if (!inp) throw new Error(`строки фильтра у панели ${p.plugin} нет`);
      inp.value = s.text;
    },
  };

  // Рецепт применяется над загруженной сценой в ячейке { theme, contrast }.
  // Тему ячейки продукт задаёт явным выбором оформления (ui.appearance.*), поэтому и здесь
  // выбор равен теме ячейки: кнопка оформления показывает его.
  const apply = async (recipe, cell) => {
    S.setAppearance(cell.theme);
    S.setPref('contrast', cell.contrast === 'high');
    S.setPref('motion', recipe.motion === 'reduce');
    S.renderNow();
    for (const s of recipe.steps || []) {
      const run = STEPS[s.op];
      if (!run) throw new Error(`шаг ${s.op} прототип пока не выполняет`);
      run(s);
      S.renderNow();
      await settle();
    }
    S.renderNow();
    await settle();
  };

  // Наблюдаемый состав и сырые факты элементов с ключами; шрифты, псевдоэлементы и маски
  // съёмщик берёт по CDP.
  // Заливка, которую видно под элементом: фоны предков от корня документа, полупрозрачные
  // накладываются на нижние. Корень поверхности несёт её вместо прозрачной заливки, чтобы фон под
  // значком был известен из паспорта.
  const parseRgba = (text) => { const m = text.match(/rgba?\(([^)]*)\)/); if (!m) return null; const [r, g, b, a = 1] = m[1].split(',').map(Number); return { r, g, b, a }; };
  const seenFill = (el) => {
    const chain = []; for (let n = el; n && n.nodeType === 1; n = n.parentElement) chain.unshift(parseRgba(getComputedStyle(n).backgroundColor));
    let c = { r: 255, g: 255, b: 255 };
    chain.forEach((f) => { if (f && f.a > 0) c = { r: f.r * f.a + c.r * (1 - f.a), g: f.g * f.a + c.g * (1 - f.a), b: f.b * f.a + c.b * (1 - f.a) }; });
    return `rgb(${Math.round(c.r)}, ${Math.round(c.g)}, ${Math.round(c.b)})`;
  };

  const facts = () => {
    const L = S.layout();
    const ck = (el) => el.dataset.ck;
    const zones = {};
    document.querySelectorAll('.group[data-zone]').forEach((el) => {
      const g = L.groups[el.dataset.g]; if (!g) return;
      const zone = el.dataset.zone;
      const tabs = [...el.querySelectorAll('.tab[data-tab]')].map((t) => S.st.panels[t.dataset.tab].plugin);
      const flat = el.classList.contains('flat');
      const shownTabs = flat ? g.tabs.map((id) => S.st.panels[id].plugin) : tabs;
      const on = flat ? 1 : [...el.querySelectorAll('.tab[data-tab]')].findIndex((t) => t.classList.contains('on')) + 1;
      const body = el.querySelector('.stub') ? 'stub' : shownTabs.length ? 'ready' : 'empty';
      const tools = [...el.querySelectorAll('.tabbar > [data-cmd]')].map((b) => ({ 'split.v': 'split', 'split.s': 'split', zoom: L.zoom ? 'restore' : 'zoom', 'panel.menu': 'menu' }[b.dataset.cmd] || b.dataset.cmd));
      (zones[zone] = zones[zone] || []).push({ tabs: shownTabs, active: on > 0 ? on : null, body, tools });
    });
    const focusEl = document.querySelector('.group.is-focus[data-zone]') || (L.focus && document.querySelector(`.group[data-active="${L.focus}"]`));
    const focus = focusEl ? { zone: focusEl.dataset.zone, part: (zones[focusEl.dataset.zone] || []).length && [...document.querySelectorAll(`.group[data-zone="${focusEl.dataset.zone}"]`)].indexOf(focusEl) + 1 } : null;
    const pop = document.querySelector('.pop[data-ck]');
    const menu = pop ? [...pop.querySelectorAll('.row')].map((row) => ({ title: row.querySelector('.row-t').textContent, enabled: !row.classList.contains('dim') })) : undefined;
    const controls = (root) => [...document.querySelectorAll(`${root} button[data-ck]`)].map(ck);
    const elements = [...document.querySelectorAll('[data-ck]')].map((el) => {
      const r = el.getBoundingClientRect();
      const parent = el.parentElement && el.parentElement.closest('[data-ck]');
      const cs = getComputedStyle(el);
      const leaf = el.children.length === 0 || el.tagName === 'KBD' || el.classList.contains('row-t');
      // Поле показывает значение, а без него — placeholder: textContent у него пуст.
      const shown = el.tagName === 'INPUT' ? el.value || el.placeholder : el.textContent;
      return {
        ck: ck(el), tag: el.tagName.toLowerCase(), parent: parent ? ck(parent) : null,
        frame: { x: r.x, y: r.y, w: r.width, h: r.height },
        visible: cs.visibility !== 'hidden' && cs.display !== 'none' && r.width > 0 && r.height > 0,
        text: leaf && el.tagName !== 'svg' ? shown : null,
        truncated: leaf && cs.textOverflow === 'ellipsis' ? el.scrollWidth > el.clientWidth : false,
        icon: el.dataset.icon ? { name: el.dataset.icon, size: +el.getAttribute('width') } : null,
        style: {
          fontFamily: cs.fontFamily, fontWeight: cs.fontWeight, fontSize: cs.fontSize, lineHeight: cs.lineHeight, letterSpacing: cs.letterSpacing,
          color: cs.color, background: parent ? cs.backgroundColor : seenFill(el),
          border: ['Top', 'Right', 'Bottom', 'Left'].map((side) => ({ side: side.toLowerCase(), color: cs[`border${side}Color`], width: cs[`border${side}Width`], style: cs[`border${side}Style`] })),
          radius: cs.borderTopLeftRadius, padding: [cs.paddingTop, cs.paddingRight, cs.paddingBottom, cs.paddingLeft], shadow: cs.boxShadow,
        },
      };
    });
    return { composition: { zones, focus, menu, titlebar: controls('.titlebar'), statusline: controls('.status') }, elements };
  };

  // Инвентарь обычного мира: id зарегистрированных сценариев и плагины с размещениями.
  const inventory = () => ({ scenarios: S.scenarios.map((s) => s.id), plugins: Object.values(S.plugins).map((p) => ({ id: p.id, placements: p.placements || ZONES })) });

  window.conformance = { load, apply, step, settle, flush, facts, inventory, ready: false };
  window.addEventListener('load', () => { settle().then(() => { window.conformance.ready = true; }); });
})();
