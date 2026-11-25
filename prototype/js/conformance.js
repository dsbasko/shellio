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
  const settle = async () => {
    await document.fonts.ready;
    let last = null;
    for (let i = 0; i < 50; i += 1) {
      flush();
      await frame();
      flush();
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
      if (!known[c.id]) { delete S.plugins[c.id].tabTitle; S.plugins[c.id].view = () => ''; }
    });
    S.scenarios.length = 0;
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
          g.tabs.push(p.id);
          if (t.active) g.active = p.id;
        });
        if (scene.focus.zone === z.zone && scene.focus.part === i + 1) L.focus = g.active;
      });
    });
    ZONES.forEach((zone) => { if (!L.parts[zone].length) { const g = group(zone); L.groups[g.id] = g; L.parts[zone].push(g.id); } });
    L.hidden = { left: !shown.left, right: !shown.right, bottom: !shown.bottom, plate: !L.parts.left.some((id) => L.groups[id].tabs.length) };
    if (scene.appearance !== 'system') S.setTheme(scene.appearance);
    S.renderNow();
    await settle();
  };

  const STEPS = {
    hide_zone: (s) => { S.layout().hidden[s.zone] = true; },
    show_zone: (s) => { S.layout().hidden[s.zone] = false; },
    zoom: (s) => { const L = S.layout(); const g = S.groupOf(L, L.focus); L.zoom = (g && g.zone === s.zone ? L.focus : S.partsOf(L, s.zone)[s.part - 1].active); },
  };

  // Рецепт применяется над загруженной сценой в ячейке { theme, contrast }.
  const apply = async (recipe, cell) => {
    S.setTheme(cell.theme);
    S.setPref('contrast', cell.contrast === 'high');
    S.setPref('motion', recipe.motion === 'reduce');
    (recipe.steps || []).forEach((s) => {
      const run = STEPS[s.op];
      if (!run) throw new Error(`шаг ${s.op} прототип пока не выполняет`);
      run(s);
    });
    S.renderNow();
    await settle();
  };

  // Наблюдаемый состав и сырые факты элементов с ключами; шрифты, псевдоэлементы и маски
  // съёмщик берёт по CDP.
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
    const controls = (root) => [...document.querySelectorAll(`${root} button[data-ck]`)].map(ck);
    const elements = [...document.querySelectorAll('[data-ck]')].map((el) => {
      const r = el.getBoundingClientRect();
      const parent = el.parentElement && el.parentElement.closest('[data-ck]');
      const cs = getComputedStyle(el);
      const leaf = el.children.length === 0 || el.tagName === 'KBD';
      return {
        ck: ck(el), tag: el.tagName.toLowerCase(), parent: parent ? ck(parent) : null,
        frame: { x: r.x, y: r.y, w: r.width, h: r.height },
        visible: cs.visibility !== 'hidden' && cs.display !== 'none' && r.width > 0 && r.height > 0,
        text: leaf && el.tagName !== 'svg' ? el.textContent : null,
        truncated: leaf && cs.textOverflow === 'ellipsis' ? el.scrollWidth > el.clientWidth : false,
        icon: el.dataset.icon ? { name: el.dataset.icon, size: +el.getAttribute('width') } : null,
        style: {
          fontFamily: cs.fontFamily, fontWeight: cs.fontWeight, fontSize: cs.fontSize, lineHeight: cs.lineHeight, letterSpacing: cs.letterSpacing,
          color: cs.color, background: cs.backgroundColor, borderColor: cs.borderTopColor, borderWidth: cs.borderTopWidth, borderStyle: cs.borderTopStyle,
          radius: cs.borderTopLeftRadius, padding: [cs.paddingTop, cs.paddingRight, cs.paddingBottom, cs.paddingLeft], shadow: cs.boxShadow,
        },
      };
    });
    return { composition: { zones, focus, titlebar: controls('.titlebar'), statusline: controls('.status') }, elements };
  };

  window.conformance = { load, apply, step, settle, flush, facts, ready: false };
  window.addEventListener('load', () => { settle().then(() => { window.conformance.ready = true; }); });
})();
