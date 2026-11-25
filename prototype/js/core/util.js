// Базовые утилиты: экранирование, морфинг DOM, нечёткий поиск, иконки.
window.S = window.S || {};

S.esc = (s) => String(s == null ? '' : s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
S.cx = (...a) => a.filter(Boolean).join(' ');
S.uid = (() => { let n = 0; const uid = (p = 'id') => `${p}-${++n}`; uid.reset = () => { n = 0; }; return uid; })();
S.clamp = (v, a, b) => Math.max(a, Math.min(b, v));
S.fmt = {
  num: (v) => String(v).replace('.', ','),
  mem: (mb) => (mb >= 1024 ? `${String((mb / 1024).toFixed(1)).replace('.', ',')} ГБ` : `${String(Math.round(mb * 10) / 10).replace('.', ',')} МБ`),
  plural: (n, one, few, many) => { const a = n % 100, b = n % 10; const w = a > 10 && a < 20 ? many : b === 1 ? one : b >= 2 && b <= 4 ? few : many; return `${n} ${w}`; },
};

S.match = (text, query) => {
  if (!query) return 1;
  const t = text.toLowerCase(); const q = query.toLowerCase();
  const direct = t.indexOf(q);
  if (direct >= 0) return 1000 - direct;
  let i = 0, score = 0, last = -2;
  for (const c of q) {
    const at = t.indexOf(c, i);
    if (at < 0) return 0;
    score += at === last + 1 ? 6 : 1;
    last = at; i = at + 1;
  }
  return score;
};

function sameNode(a, b) {
  if (a.nodeType !== b.nodeType) return false;
  if (a.nodeType === 1) return a.tagName === b.tagName && (a.getAttribute('data-k') || '') === (b.getAttribute('data-k') || '');
  return true;
}

function syncAttrs(from, to) {
  for (const { name, value } of Array.from(to.attributes)) if (from.getAttribute(name) !== value) from.setAttribute(name, value);
  for (const { name } of Array.from(from.attributes)) if (!to.hasAttribute(name) && name !== 'tabindex') from.removeAttribute(name);
}

function morphNode(from, to) {
  if (from.nodeType === 3 || from.nodeType === 8) { if (from.nodeValue !== to.nodeValue) from.nodeValue = to.nodeValue; return; }
  const keep = to.hasAttribute && to.hasAttribute('data-keep');
  syncAttrs(from, to);
  const tag = from.tagName;
  if (tag === 'TEXTAREA' || tag === 'INPUT') {
    if (document.activeElement !== from) { if (to.type === 'checkbox') from.checked = to.checked; else from.value = to.value != null ? (tag === 'TEXTAREA' ? to.textContent : to.value) : ''; }
    return;
  }
  if (keep) return;
  morphChildren(from, to);
}

function morphChildren(from, to) {
  const keyed = new Map();
  for (const c of Array.from(from.children)) { const k = c.getAttribute('data-k'); if (k) keyed.set(k, c); }
  let cur = from.firstChild; let next = to.firstChild;
  while (next) {
    const n = next; next = next.nextSibling;
    let match = null;
    const k = n.nodeType === 1 ? n.getAttribute('data-k') : null;
    if (k && keyed.has(k)) { match = keyed.get(k); keyed.delete(k); }
    else if (cur && sameNode(cur, n) && !(cur.nodeType === 1 && cur.getAttribute('data-k'))) match = cur;
    if (match) {
      if (match !== cur) from.insertBefore(match, cur); else cur = cur.nextSibling;
      morphNode(match, n);
    } else from.insertBefore(n.cloneNode(true), cur);
  }
  // Строка фильтра живёт вне разметки панели и держит ввод.
  while (cur) { const r = cur; cur = cur.nextSibling; if (!(r.classList && r.classList.contains('pfilter'))) from.removeChild(r); }
}

const tpl = document.createElement('template');
S.morph = (el, html) => {
  const box = el.cloneNode(false);
  tpl.innerHTML = html;
  box.appendChild(tpl.content);
  morphChildren(el, box);
};

const P = {
  search: '<circle cx="7" cy="7" r="4.3"/><path d="M10.2 10.2L13.5 13.5"/>',
  plus: '<path d="M4 8h8M8 4v8"/>',
  close: '<path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/>',
  chevR: '<path d="M6 4l4 4-4 4"/>',
  chevD: '<path d="M4 6l4 4 4-4"/>',
  chevL: '<path d="M10 3.5 5.5 8l4.5 4.5"/>',
  more: '<circle cx="3.5" cy="8" r="1.2" fill="currentColor" stroke="none"/><circle cx="8" cy="8" r="1.2" fill="currentColor" stroke="none"/><circle cx="12.5" cy="8" r="1.2" fill="currentColor" stroke="none"/>',
  sideL: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6 3v10"/>',
  sideR: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M10 3v10"/>',
  sideB: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M2 9.5h12"/>',
  expand: '<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9"/>',
  shrink: '<path d="M9 2.8V7h4.2M7 13.2V9H2.8M13.5 2.5 9 7M2.5 13.5 7 9"/>',
  splitCols: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M8 3v10"/>',
  splitRows: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M2 8h12"/>',
  term: '<rect x="1.8" y="2.8" width="12.4" height="10.4" rx="2"/><path d="M4.8 6.3l2 1.7-2 1.7M8.6 10h2.6"/>',
  agent: '<path d="M8 2.2v11.6M2.2 8h11.6M3.9 3.9l8.2 8.2M12.1 3.9l-8.2 8.2"/>',
  file: '<path d="M4 2h5l3 3v9H4z"/><path d="M9 2v3h3"/>',
  folder: '<path d="M2 4.5h4.2l1.5 1.5H14v6.5H2z"/>',
  branch: '<circle cx="5" cy="3.8" r="1.5"/><circle cx="5" cy="12.2" r="1.5"/><circle cx="11" cy="5.8" r="1.5"/><path d="M5 5.3v5.4M11 7.3c0 2.2-2.4 2.6-6 3.4"/>',
  commit: '<circle cx="8" cy="8" r="2.6"/><path d="M1.5 8h3.9M10.6 8h3.9"/>',
  changes: '<path d="M8 2.5v5M5.5 5h5M5.5 11.5h5"/><rect x="2.5" y="1.5" width="11" height="13" rx="2.2"/>',
  history: '<circle cx="5" cy="3" r="1.4"/><circle cx="5" cy="8" r="1.4"/><circle cx="5" cy="13" r="1.4"/><circle cx="11.5" cy="5.5" r="1.4"/><path d="M5 4.4v2.2M5 9.4v2.2M11.5 6.9c0 2.5-3.8 3.2-5.6 5"/>',
  review: '<path d="M2.5 3.5c0-.6.4-1 1-1h9c.6 0 1 .4 1 1v7c0 .6-.4 1-1 1H7l-3 2.5v-2.5h-.5c-.6 0-1-.4-1-1z"/><path d="M5.5 6h5M5.5 8.5h3"/>',
  pipeline: '<circle cx="3.5" cy="8" r="1.8"/><circle cx="12.5" cy="8" r="1.8"/><path d="M5.3 8h5.4"/>',
  cpu: '<rect x="4" y="4" width="8" height="8" rx="1.5"/><path d="M6.5 1.8V4M9.5 1.8V4M6.5 12v2.2M9.5 12v2.2M1.8 6.5H4M1.8 9.5H4M12 6.5h2.2M12 9.5h2.2"/>',
  gear: '<circle cx="8" cy="8" r="2.2"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6 4.8 4.8M11.2 11.2l1.2 1.2M3.6 12.4l1.2-1.2M11.2 4.8l1.2-1.2"/>',
  keys: '<rect x="1.5" y="4" width="13" height="8" rx="1.8"/><path d="M4.5 6.8h.01M7 6.8h.01M9.5 6.8h.01M12 6.8h.01M5 9.5h6"/>',
  hub: '<path d="M2 3.5c0-.6.4-1 1-1h6.5c.6 0 1 .4 1 1V7c0 .6-.4 1-1 1H6L3.8 9.8V8H3c-.6 0-1-.4-1-1z"/><path d="M12.5 6H13c.6 0 1 .4 1 1v3.5c0 .6-.4 1-1 1h-.8v1.7L10 11.5H8"/>',
  check: '<path d="m3 8.5 3.4 3.4L13 4.8"/>',
  warn: '<path d="M8 2.2 14.3 13H1.7z"/><path d="M8 6.5v3M8 11.3h.01"/>',
  info: '<circle cx="8" cy="8" r="6"/><path d="M8 7.2v3.6M8 5h.01"/>',
  clock: '<circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.3 1.6"/>',
  lock: '<rect x="3.5" y="7" width="9" height="6.5" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/>',
  pin: '<path d="m9.5 2 4.5 4.5-2 .7-2.7 2.7-.2 2.6L3.3 7.7l2.6-.2 2.7-2.7z"/><path d="M5.5 10.5 2.2 13.8"/>',
  stop: '<rect x="4" y="4" width="8" height="8" rx="1.4"/>',
  play: '<path d="M5 3.2 12.5 8 5 12.8z"/>',
  refresh: '<path d="M13.5 8A5.5 5.5 0 1 1 11.8 4"/><path d="M12 1.5V4.4H9.1"/>',
  link: '<path d="M6.8 9.2a2.6 2.6 0 0 0 3.7 0l2.2-2.2a2.6 2.6 0 0 0-3.7-3.7L8 4.3"/><path d="M9.2 6.8a2.6 2.6 0 0 0-3.7 0L3.3 9a2.6 2.6 0 0 0 3.7 3.7l1-1"/>',
  out: '<path d="M6.5 3H3.5C3 3 2.5 3.5 2.5 4v8.5c0 .5.5 1 1 1H12c.5 0 1-.5 1-1v-3"/><path d="M9.5 2.5h4v4M13.5 2.5 7.5 8.5"/>',
  gitlab: '<path d="m8 14-6-5 1.8-6.5L5.8 7h4.4l2-4.5L14 9z"/>',
  github: '<path d="M8 1.8a6.2 6.2 0 0 0-2 12c.3 0 .4-.1.4-.3v-1.2c-1.7.4-2-.7-2-.7-.3-.7-.7-.9-.7-.9-.6-.4 0-.4 0-.4.6 0 1 .6 1 .6.5.9 1.4.7 1.8.5 0-.4.2-.7.4-.9-1.4-.1-2.8-.7-2.8-3.1 0-.7.2-1.2.6-1.7-.1-.1-.3-.8 0-1.6 0 0 .5-.2 1.7.6a6 6 0 0 1 3.2 0c1.2-.8 1.7-.6 1.7-.6.3.8.1 1.5 0 1.6.4.5.6 1 .6 1.7 0 2.4-1.4 3-2.8 3.1.2.2.4.6.4 1.2v1.8c0 .2.1.4.4.3a6.2 6.2 0 0 0-2-12Z"/>',
  grid: '<rect x="2.5" y="2.5" width="11" height="11" rx="2"/><path d="M2.5 7h11M7 7v6.5"/>',
  update: '<circle cx="8" cy="8" r="6"/><path d="M8 4.8v6M5.5 8.6 8 11l2.5-2.4"/>',
  help: '<circle cx="8" cy="8" r="6"/><path d="M6.2 6.2A1.9 1.9 0 0 1 8 4.8c1.1 0 1.9.7 1.9 1.7 0 1.4-1.9 1.5-1.9 2.9M8 11.4h.01"/>',
  eye: '<path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="1.9"/>',
  draft: '<path d="M3 13.2l.6-2.8 7.1-7.1a1.4 1.4 0 0 1 2 2l-7.1 7.1z"/><path d="M9.6 4.4l2 2"/>',
  merge: '<circle cx="4" cy="3.5" r="1.6"/><circle cx="4" cy="12.5" r="1.6"/><circle cx="12" cy="12.5" r="1.6"/><path d="M4 5v6M4 6c.5 3 4 5 6.4 6.4"/>',
  label: '<path d="M2.5 3.5h5.2l5.8 5.8-4.2 4.2-5.8-5.8z"/><path d="M5.5 6.2h.01"/>',
  user: '<circle cx="8" cy="5.2" r="2.7"/><path d="M2.8 13.8c0-2.8 2.3-4.6 5.2-4.6s5.2 1.8 5.2 4.6"/>',
  send: '<path d="M2 8 14 2.5 11 14l-3-4.5z"/><path d="M8 9.5 14 2.5"/>',
  filter: '<path d="M2 3.5h12L9.5 9v4l-3 1.2V9z"/>',
  sun: '<circle cx="8" cy="8" r="2.8"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M3.6 12.4l1.1-1.1M11.3 4.7l1.1-1.1"/>',
  moon: '<path d="M13.2 9.6A5.4 5.4 0 0 1 6.4 2.8a5.4 5.4 0 1 0 6.8 6.8z"/>',
  appearanceSystem: '<circle cx="8" cy="8" r="5.6"/><path d="M8 2.4v11.2M10.4 3.9v8.2M12.5 5.9v4.2"/>',
  sliders: '<path d="M2.5 5h7M12 5h1.5M2.5 11H4M7 11h6.5"/><circle cx="10.5" cy="5" r="1.5"/><circle cx="5.5" cy="11" r="1.5"/>',
  book: '<path d="M2.5 3c2.2-.8 4-.5 5.5.8 1.5-1.3 3.3-1.6 5.5-.8v10c-2.2-.8-4-.5-5.5.8-1.5-1.3-3.3-1.6-5.5-.8z"/><path d="M8 3.8v10"/>',
  undo: '<path d="M5 3 2 6l3 3"/><path d="M2 6h7.5a3.5 3.5 0 0 1 0 7H6"/>',
  drop: '<path d="M8 2v8M4.5 7 8 10.5 11.5 7"/><path d="M2.5 13.5h11"/>',
  sideLP: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6.5 3v10M9 3v10"/>',
  minus: '<path d="M4 8h8"/>',
  cmd: '<path d="M5 4.5L8.5 8 5 11.5M9.5 11.5H13"/>',
  retry: '<path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2M12.5 3v2.6H9.9"/>',
  okc: '<circle cx="8" cy="8" r="6.2"/><path d="M5.3 8.2l1.8 1.8 3.6-3.8"/>',
  errc: '<circle cx="8" cy="8" r="6.2"/><path d="M5.9 5.9l4.2 4.2M10.1 5.9l-4.2 4.2"/>',
  idlec: '<circle cx="8" cy="8" r="6.2"/>',
  runc: '<circle cx="8" cy="8" r="6.2" stroke="var(--line-2)"/><path d="M8 1.8a6.2 6.2 0 0 1 6.2 6.2"/><circle cx="8" cy="8" r="1.6" fill="currentColor" stroke="none"/>',
  scenes: '<rect x="2.5" y="2.5" width="11" height="8.5" rx="1.5"/><path d="M5 13.5h6"/><path d="m6.5 5 3 1.7-3 1.8z"/>',
};

// data-icon называет нарисованный контур набора: неизвестное имя рисуется контуром file.
S.icon = (name, size = 16, ck) => `<svg class="i" width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" data-icon="${P[name] ? name : 'file'}"${ck ? ` data-ck="${ck}"` : ''}>${P[name] || P.file}</svg>`;
