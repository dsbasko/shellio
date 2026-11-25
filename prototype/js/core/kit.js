// Компоненты интерфейса. Плагины собирают представления только из них.
window.K = {};
(() => {
  const { esc, cx, icon } = S;
  const data = (o = {}) => {
    let s = '';
    if (o.do) s += ` data-do="${o.do}"`;
    if (o.cmd) s += ` data-cmd="${o.cmd}"`;
    if (o.arg != null) s += ` data-arg="${esc(o.arg)}"`;
    if (o.k != null) s += ` data-k="${esc(o.k)}"`;
    if (o.tip) s += ` title="${esc(o.tip)}"`;
    return s;
  };

  K.icon = icon;

  K.status = (kind, label) => {
    const glyph = kind === 'run'
      ? '<svg class="st-spin" width="12" height="12" viewBox="0 0 12 12" fill="none"><circle cx="6" cy="6" r="4.5" stroke="currentColor" stroke-opacity=".25" stroke-width="1.6"/><path d="M6 1.5A4.5 4.5 0 0 1 10.5 6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>'
      : '';
    return `<span class="st st-${kind || 'idle'}">${glyph}${label ? `<span class="st-t">${label}</span>` : ''}</span>`;
  };

  const KEY = { Esc: 'esc', Enter: '⏎', Up: '↑', Down: '↓', Left: '←', Right: '→' };
  const keyText = (k) => KEY[k] || (k.length === 1 ? k.toUpperCase() : k);
  const parseKey = (tok) => {
    const m = tok.match(/^((?:(?:Ctrl|Alt|Shift|Cmd)-)+)(.+)$/);
    if (!m) return null;
    const mods = m[1].split('-').filter(Boolean);
    const signs = [['Alt', '⌥'], ['Shift', '⇧'], ['Cmd', '⌘']].filter(([n]) => mods.includes(n)).map(([, s]) => s);
    return { ctrl: mods.includes('Ctrl'), signs, key: signs.length ? keyText(m[2]) : (KEY[m[2]] || m[2]) };
  };
  // Модификатор читается как в macOS: ⌘K одной клавишей, Ctrl и клавиша двумя.
  // ck — ключ сверки: у одной клавиши он сам, у нескольких — с номером клавиши.
  K.kbd = (keys, ck) => String(keys).split(' ').flatMap((tok) => {
    const k = parseKey(tok);
    if (!k) return [KEY[tok] || tok];
    return [...(k.ctrl ? ['Ctrl'] : []), `${k.signs.join('')}${k.key}`];
  }).map((k, i, all) => `<kbd${ck ? ` data-ck="${all.length > 1 ? `${ck}.${i + 1}` : ck}"` : ''}>${esc(k)}</kbd>`).join('');
  // В палитре каждый модификатор идёт отдельной клавишей.
  K.keyChips = (keys) => String(keys).split(' ').flatMap((tok) => {
    const k = parseKey(tok);
    return k ? [...(k.ctrl ? ['Ctrl'] : []), ...k.signs, k.key] : [tok];
  }).join(' ');

  // В тексте подсказки клавиши идут строкой: ⌘K одним знаком, аккорд Ctrl как в keymap.
  K.keyLine = (keys) => String(keys).split(' ').map((tok) => {
    const k = parseKey(tok);
    return k ? `${k.ctrl ? 'Ctrl-' : ''}${k.signs.join('')}${k.key}` : (KEY[tok] || tok);
  }).join(' ');

  K.btn = (label, o = {}) => `<button type="button" class="${cx('btn', o.kind, o.small && 'small', o.icon && !label && 'only')}"${data(o)}${o.disabled ? ' disabled' : ''}>${o.icon ? icon(o.icon, 13) : ''}${label ? `<span>${label}</span>` : ''}${o.key ? `<span class="btn-k">${K.kbd(o.key)}</span>` : ''}</button>`;

  K.iconBtn = (name, o = {}) => K.btn('', { ...o, icon: name, kind: cx('flat', o.kind) });

  K.chip = (label, o = {}) => `<span class="${cx('chip', o.tone)}"${data({ k: o.k })}>${o.icon ? icon(o.icon, 12) : ''}<span>${label}</span>${o.remove ? `<button type="button" class="chip-x" data-do="${o.remove}" data-arg="${esc(o.arg == null ? '' : o.arg)}">${icon('close', 10)}</button>` : ''}</span>`;

  K.ctx = (left, right = '') => `<div class="ctx"><div class="ctx-l">${left}</div><div class="ctx-r">${right}</div></div>`;

  K.crumbs = (items) => `<span class="crumb">${items.map((t) => `<span>${t}</span>`).join('<i>/</i>')}</span>`;

  K.sec = (title, trail = '') => `<div class="sec"><span class="sec-t">${title}</span><span class="sec-r">${trail}</span></div>`;

  // Заголовок группы со счётчиком и действием справа.
  K.grp = (title, n, action = '') => `<div class="sec"><span class="sec-t">${title}</span>${n != null ? `<span class="n">${n}</span>` : ''}${action}</div>`;
  K.lnk = (label, o = {}) => `<button type="button" class="lnk"${data(o)}>${label}</button>`;

  // Навигатор
  K.navTree = (o) => {
    const chev = o.chev == null ? '' : o.chevDo ? `<span class="tree-chev" data-do="${o.chevDo}" data-arg="${esc(o.arg)}" title="${o.chev ? 'Свернуть' : 'Раскрыть'}">${icon(o.chev ? 'chevD' : 'chevR', 12)}</span>` : icon(o.chev ? 'chevD' : 'chevR', 12);
    const more = o.more ? `<span class="tree-more" data-do="${o.more}" data-arg="${esc(o.arg)}" title="Действия">${icon('more', 13)}</span>` : '';
    const quick = o.quick ? `<span class="tree-more" data-do="${o.quick.do}" data-arg="${esc(o.arg)}" title="${esc(o.quick.tip)}">${icon(o.quick.icon, 13)}</span>` : '';
    return `<button type="button" class="${cx('tree', o.level === 2 && 'l2', o.sel && 'sel', o.kb && 'kb', o.dim && 'dim')}"${data(o)}${o.sel ? ' aria-current="true"' : ''}${o.kb || o.reveal ? ' data-cur="1"' : ''}>${chev}${icon(o.icon, 14)}<span class="tree-t">${o.title}</span>${o.trail || ''}${quick}${more}</button>`;
  };
  K.navSect = (title, sub = '', actions = '', always = false) => `<div class="sect" data-k="sect-${esc(title)}">${title}${sub ? `<span class="sub">${sub}</span>` : ''}${actions ? `<span class="${cx('sect-a', always && 'on')}">${actions}</span>` : ''}</div>`;
  K.navSess = (o) => `<button type="button" class="${cx('sess', o.sel && 'sel', o.kb && 'kb')}"${data(o)}${o.kb ? ' data-cur="1"' : ''}>${icon(o.icon, 14)}<span class="nm">${o.title}</span><span class="${cx('sess-st', o.stCls)}"><span class="dot ${o.dot || 'none'}"></span>${o.status}</span></button>`;

  // Строка файла в изменениях
  K.frow = (o) => `<div class="${cx('frow', o.on && 'on', o.kb && 'kb')}" role="button"${data(o)}${o.kb ? ' data-cur="1"' : ''}><span class="s s-${o.status.toLowerCase()}">${o.status}</span><span class="fn2">${o.name}</span>${o.note || ''}<span class="nums">${o.a ? `<span class="na">+${o.a}</span>` : ''}${o.d ? `<span class="nd">−${o.d}</span>` : ''}${o.flag ? `<span>${o.flag}</span>` : ''}</span>${o.act ? `<button type="button" class="act" aria-label="${esc(o.act.label)}" title="${esc(o.act.label)}" data-do="${o.act.do}" data-arg="${esc(o.act.arg)}">${icon(o.act.icon, 12)}</button>` : ''}</div>`;

  // Пайплайны
  const JOB = { ok: ['okc', 'ic-ok'], failed: ['errc', 'ic-err'], running: ['runc', 'ic-run'], pending: ['idlec', 'ic-idle'], manual: ['idlec', 'ic-wait'], canceled: ['errc', 'ic-idle'], skipped: ['idlec', 'ic-idle'] };
  const jobIcon = (state, size) => { const [n, c] = JOB[state] || JOB.pending; return icon(n, size).replace('class="i"', `class="i ${c}"`); };
  K.plRow = (o) => `<button type="button" class="${cx('pl-row', o.on && 'on', o.kb && 'kb')}"${data(o)}${o.on ? ' aria-pressed="true"' : ''}>${jobIcon(o.state, 16)}<span class="pl-main"><span class="pl-id">${o.id}</span><span class="${cx('pl-st', o.state === 'failed' && 'err')}">${o.label}</span></span><span class="pl-br">${o.branch}</span></button>`;
  K.pstat = (state, label) => { const [n, c] = JOB[state] || JOB.pending; return `<span class="pstat ${c}">${icon(n, 14)}${label}</span>`; };
  K.job = (o) => `<div class="${cx('job', o.state === 'failed' && 'fail', o.on && 'on', o.kb && 'kb')}" role="button"${data(o)}>${jobIcon(o.state, 16)}${o.name}${o.retry ? `<button type="button" class="retry" aria-label="Перезапустить задание" title="${o.retryOff ? 'Нет аккаунта в этом воркспейсе' : 'Перезапустить задание'}" data-do="${o.retry}" data-arg="${esc(o.name)}"${o.retryOff ? ' disabled' : ''}>${icon('retry', 12)}</button>` : ''}</div>`;
  K.graph = (stages) => `<div class="graph">${stages.map((s, i) => `${i ? '<div class="link" aria-hidden="true"></div>' : ''}<div class="stage"><div class="stage-t">${s.title}</div>${s.jobs.map(K.job).join('')}</div>`).join('')}</div>`;

  K.row = (o) => `<div class="${cx('row', o.on && 'on', o.quiet && 'quiet', o.dim && 'dim', o.indent && `in-${o.indent}`, o.tall && 'tall')}"${data(o)}${o.on ? ' data-cur="1"' : ''}>${o.lead || ''}${o.st ? `<span class="row-g">${K.status(o.st)}</span>` : o.icon ? `<span class="row-g">${icon(o.icon, 15)}</span>` : ''}<span class="row-m"><span class="${cx('row-t', o.mono && 'mono')}">${o.title}</span>${o.sub ? `<span class="row-s">${o.sub}</span>` : ''}</span>${o.trail != null ? `<span class="row-r">${o.trail}</span>` : ''}</div>`;

  K.check = (state, o = {}) => `<span class="${cx('check', state === true && 'on', state === 'mixed' && 'mixed')}"${data(o)}>${state === true ? icon('check', 10) : state === 'mixed' ? '<i></i>' : ''}</span>`;

  K.radio = (on, o = {}) => `<span class="${cx('radio', on && 'on')}"${data(o)}></span>`;

  K.toggle = (on, o = {}) => `<span class="${cx('toggle', on && 'on')}"${data(o)}><i></i></span>`;

  K.seg = (items) => `<span class="seg">${items.map((it) => `<button type="button" class="${cx('seg-i', it.on && 'on')}"${data(it)}>${it.icon ? icon(it.icon, 13) : ''}${it.label}${it.count != null ? `<span class="seg-n">${it.count}</span>` : ''}</button>`).join('')}</span>`;

  K.banner = (tone, title, text = '', actions = '') => `<div class="${cx('banner', tone)}">${icon(tone === 'err' ? 'warn' : tone === 'ok' ? 'check' : tone === 'wait' ? 'warn' : 'info', 15)}<div class="banner-m"><b>${title}</b>${text ? `<span>${text}</span>` : ''}</div>${actions ? `<div class="banner-a">${actions}</div>` : ''}</div>`;

  K.empty = (o) => `<div class="empty">${o.icon ? `<div class="empty-i">${icon(o.icon, 22)}</div>` : ''}<div class="empty-t">${o.title}</div>${o.text ? `<div class="empty-s">${o.text}</div>` : ''}${o.actions ? `<div class="empty-a">${o.actions}</div>` : ''}</div>`;

  K.kv = (pairs) => `<dl class="kv">${pairs.filter(Boolean).map(([a, b]) => `<dt>${a}</dt><dd>${b}</dd>`).join('')}</dl>`;

  K.table = (t) => `<table class="tbl"><colgroup>${t.cols.map((c) => `<col${c.w ? ` style="width:${c.w}"` : ''}>`).join('')}</colgroup><thead><tr>${t.cols.map((c) => `<th class="${c.num ? 'num' : ''}">${c.t}</th>`).join('')}</tr></thead><tbody>${t.rows.map((r) => `<tr class="${cx(r.on && 'on', r.dim && 'dim')}"${data(r)}${r.on ? ' data-cur="1"' : ''}>${r.cells.map((c, i) => `<td class="${t.cols[i].num ? 'num' : ''}">${c}</td>`).join('')}</tr>`).join('')}</tbody></table>`;

  K.bar = (pct, tone) => `<span class="${cx('bar', tone)}"><i style="width:${Math.min(100, pct)}%"></i></span>`;

  K.field = (o) => o.rows
    ? `<textarea class="${cx('field', o.mono && 'mono')}" rows="${o.rows}" data-bind="${o.bind}" placeholder="${esc(o.placeholder || '')}"${o.ro ? ' readonly' : ''}>${esc(o.value || '')}</textarea>`
    : `<input class="${cx('field', o.mono && 'mono')}" data-bind="${o.bind}" placeholder="${esc(o.placeholder || '')}" value="${esc(o.value || '')}"${o.ro ? ' readonly' : ''}>`;

  K.cols = (items, cls = '') => `<div class="cols ${cls}">${items.filter(Boolean).map((c, i) => `${i ? '<div class="div-v"></div>' : ''}<div class="col" style="${c.w ? `flex:0 0 ${c.w}` : 'flex:1 1 0'}">${c.html}</div>`).join('')}</div>`;
  K.stack = (items, cls = '') => `<div class="stack ${cls}">${items.filter(Boolean).join('')}</div>`;
  K.scroll = (html, cls = '') => `<div class="scroll ${cls}">${html}</div>`;
  K.foot = (left, right = '') => `<div class="foot"><div class="foot-l">${left}</div><div class="foot-r">${right}</div></div>`;
  K.pad = (html) => `<div class="pad">${html}</div>`;

  const kw = {
    rust: /\b(pub|fn|let|mut|use|struct|impl|async|await|return|match|if|else|for|in|mod|enum|const|self|crate)\b/g,
    sql: /\b(CREATE|TABLE|INDEX|ON|PRIMARY|KEY|NOT|NULL|REFERENCES|DEFAULT|UNIQUE|CONSTRAINT|TEXT|UUID|TIMESTAMPTZ)\b/g,
    ts: /\b(export|async|function|const|let|return|if|await|new|import|from)\b/g,
  };
  K.hl = (text, lang) => {
    const src = esc(text);
    if (!lang || !kw[lang]) return src;
    const m = lang === 'sql' ? /(--.*)$/ : /(\/\/.*)$/;
    const hold = [];
    const stash = (cls) => (s) => { hold.push(`<span class="${cls}">${s}</span>`); return String.fromCharCode(0xE000 + hold.length - 1); };
    return src
      .replace(m, stash('c-cm'))
      .replace(/(&quot;[^&]*?&quot;)/g, stash('c-str'))
      .replace(kw[lang], stash('c-kw'))
      .replace(/\b([a-z_][a-zA-Z0-9_]*)(?=\()/g, stash('c-fn'))
      .replace(lang === 'sql' ? /$^/ : /\b([A-Z][A-Z0-9_]+)\b/g, stash('c-num'))
      .replace(lang === 'sql' ? /$^/ : /\b([A-Z][A-Za-z0-9_]*)\b/g, stash('c-ty'))
      .replace(/\b(\d+)\b/g, stash('c-num'))
      .replace(/[\uE000-\uF8FF]/g, (ch) => hold[ch.charCodeAt(0) - 0xE000]);
  };

  // Поверхность редактора. opts: lang, cursor, curCol, curKind, sel [a,b], sign(i), onLine, tail (пустота после конца файла), cls
  K.nv = (lines, o = {}) => `<div class="${cx('nv-code scroll', o.cls)}">${lines.map((raw, idx) => {
    const l = typeof raw === 'object' ? raw.text : raw;
    const i = typeof raw === 'object' ? raw.at : idx;
    const on = o.cursor === i;
    const inSel = o.sel && i >= o.sel[0] && i <= o.sel[1];
    const body = on && o.curCol != null
      ? `${K.hl(l.slice(0, o.curCol), o.lang)}<span class="${o.curKind === 'bar' ? 'ch-bar' : 'ch-cur'}">${esc(l[o.curCol] || ' ')}</span>${K.hl(l.slice(o.curCol + 1), o.lang)}`
      : K.hl(l, o.lang) || ' ';
    const sign = o.sign ? o.sign(i) : '';
    return `<div class="${cx('nv-l', on && o.mark !== false && 'curl', inSel && 'sel')}" data-k="l${i}"${o.onLine ? ` data-do="${o.onLine}" data-arg="${i}"` : ''}${on ? ' data-cur="1"' : ''}><span class="${cx('nv-sign', sign && 'ch')}">${sign}</span><span class="nv-n">${i + 1}</span><span>${body}</span></div>`;
  }).join('')}${o.tail ? `<div style="height:${o.tail}px"></div>` : ''}</div>`;

  // Код. opts: lang, cursor (индекс строки), start (номер первой), gutter(i) -> html, sel [a,b], curCol, after(i) -> html, onLine (действие клика)
  K.code = (lines, o = {}) => {
    const start = o.start || 1;
    return `<div class="code">${lines.map((l, i) => {
      const on = o.cursor === i;
      const inSel = o.sel && i >= o.sel[0] && i <= o.sel[1];
      const body = on && o.curCol != null
        ? `${K.hl(l.slice(0, o.curCol), o.lang)}<span class="cur">${esc(l[o.curCol] || ' ')}</span>${K.hl(l.slice(o.curCol + 1), o.lang)}`
        : K.hl(l, o.lang) || ' ';
      return `<div class="${cx('ln', on && 'on', inSel && 'sel')}" data-k="l${i}"${o.onLine ? ` data-do="${o.onLine}" data-arg="${i}"` : ''}${on ? ' data-cur="1"' : ''}>${o.gutter ? `<span class="gut">${o.gutter(i)}</span>` : ''}<span class="no">${start + i}</span><span class="tx">${body}</span></div>${o.after ? o.after(i) || '' : ''}`;
    }).join('')}</div>`;
  };

  // Diff. file.hunks[].lines[]: t, o, n, s. opts: mode ('unified'|'split'), cursor (глобальный индекс строки), sel [a,b], staged(gi) -> bool, after(gi, line) -> html, onLine
  K.flat = (file) => {
    const out = [];
    (file.hunks || []).forEach((h, hi) => h.lines.forEach((l) => out.push({ ...l, hi })));
    return out;
  };

  // Ключи строк режима «Рядом»: пара удаление и добавление занимает одну строку.
  K.splitKeys = (file) => {
    const out = []; let gi = 0;
    (file.hunks || []).forEach((h) => {
      for (let i = 0; i < h.lines.length;) {
        if (h.lines[i].t === ' ') { out.push(gi); i++; gi++; continue; }
        let j = i; let d = 0; let a = 0;
        while (j < h.lines.length && h.lines[j].t === '-') { d++; j++; }
        while (j < h.lines.length && h.lines[j].t === '+') { a++; j++; }
        for (let x = 0; x < Math.max(d, a); x++) out.push(gi + x);
        gi += d + a; i = j;
      }
    });
    return out;
  };

  K.diff = (file, o = {}) => {
    const lang = o.lang || (/\.rs$/.test(file.path) ? 'rust' : /\.sql$/.test(file.path) ? 'sql' : /\.tsx?$/.test(file.path) ? 'ts' : null);
    let gi = 0;
    const cell = (l, i, side) => {
      if (!l) return '<span class="ln-e"></span>';
      return `<span class="no">${(side === 'a' ? l.o : l.n) || ''}</span><span class="sg">${l.t === ' ' ? '' : l.t}</span><span class="tx">${K.hl(l.s, lang) || ' '}</span>`;
    };
    const lineAttrs = (i) => (o.onLine ? ` data-do="${o.onLine}" data-arg="${i}"` : '');
    const flag = (i) => {
      const on = o.cursor === i; const sel = o.sel && i >= o.sel[0] && i <= o.sel[1];
      return { cls: cx(on && 'on', sel && 'sel'), cur: on ? ' data-cur="1"' : '' };
    };
    const stage = (i, l) => (o.staged ? `<span class="stg">${l.t === ' ' ? '' : K.check(o.staged(i))}</span>` : '');
    const sideAttrs = (i, side) => (o.onLine && o.sideArg ? ` data-do="${o.onLine}" data-arg="${i}:${side}"` : '');
    const picked = (f, side) => (o.side === side && f.cls.split(' ').includes('on') ? ' pick' : '');
    const html = (file.hunks || []).map((h, hi) => {
      let body = '';
      if (o.mode === 'split') {
        for (let i = 0; i < h.lines.length;) {
          const l = h.lines[i];
          if (l.t === ' ') {
            const f = flag(gi);
            body += `<div class="ln2 ${f.cls}"${f.cur}${lineAttrs(gi)} data-k="${hi}-${gi}"><div class="half${picked(f, 'old')}"${sideAttrs(gi, 'old')}>${cell(l, gi, 'a')}</div><div class="half${picked(f, 'new')}"${sideAttrs(gi, 'new')}>${cell(l, gi, 'b')}</div></div>${o.after ? o.after(gi, l) || '' : ''}`;
            i++; gi++;
          } else {
            const dels = []; const adds = [];
            let j = i;
            while (j < h.lines.length && h.lines[j].t === '-') dels.push(h.lines[j++]);
            while (j < h.lines.length && h.lines[j].t === '+') adds.push(h.lines[j++]);
            const n = Math.max(dels.length, adds.length);
            for (let x = 0; x < n; x++) {
              const curI = gi + (x < dels.length ? x : dels.length + x - dels.length);
              const f = flag(curI);
              body += `<div class="ln2 ${f.cls}"${f.cur}${lineAttrs(curI)} data-k="${hi}-${curI}"><div class="half ${dels[x] ? 'del' : 'void'}${picked(f, 'old')}"${sideAttrs(curI, 'old')}>${cell(dels[x], curI, 'a')}</div><div class="half ${adds[x] ? 'add' : 'void'}${picked(f, 'new')}"${sideAttrs(curI, 'new')}>${cell(adds[x], curI, 'b')}</div></div>${o.after ? o.after(curI, adds[x] || dels[x]) || '' : ''}`;
            }
            gi += dels.length + adds.length; i = j;
          }
        }
      } else {
        h.lines.forEach((l) => {
          const f = flag(gi);
          body += `<div class="ln ${l.t === '+' ? 'add' : l.t === '-' ? 'del' : ''} ${f.cls}"${f.cur}${lineAttrs(gi)} data-k="${hi}-${gi}">${stage(gi, l)}<span class="no">${l.o || ''}</span><span class="no">${l.n || ''}</span><span class="sg">${l.t === ' ' ? '' : l.t}</span><span class="tx">${K.hl(l.s, lang) || ' '}</span></div>${o.after ? o.after(gi, l) || '' : ''}`;
          gi++;
        });
      }
      return `<div class="hunk" data-k="h${hi}">${esc(h.header)}</div>${body}`;
    }).join('');
    return `<div class="${cx('code diff', o.mode === 'split' && 'code-wrap')}">${html}</div>`;
  };

  let charW = 0;
  let termPad = 0;
  S.termCols = (w) => {
    if (!charW) {
      const box = document.createElement('div');
      box.className = 'term';
      box.style.cssText = 'position:fixed;left:-9999px;top:0;width:auto';
      box.innerHTML = '<span>' + '─'.repeat(40) + '</span>';
      document.body.appendChild(box);
      const span = box.firstChild.getBoundingClientRect().width;
      const cs = getComputedStyle(box);
      termPad = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
      box.remove();
      if (span) charW = span / 40;
    }
    return Math.max(20, Math.floor((w - (termPad || 28) - 18) / (charW || 7.3)));
  };

  K.termLine = (l, i) => {
    if (l === '') return `<div class="gap" data-k="t${i}"></div>`;
    if (l && l.raw) return l.raw;
    return `<div class="${cx('t-l', l.cls)}" data-k="t${i}">${l.html != null ? l.html : l}</div>`;
  };
  K.term = (lines, o = {}) => `<div class="${cx('term', o.anchored && 'bottom-anchored')}" data-follow="1">${lines.map(K.termLine).join('')}${o.input != null ? `<div class="t-l" data-k="t-in">${o.prompt || ''}${esc(o.input)}<span class="cur"></span></div>` : ''}${o.box ? `<div class="tui-box" data-k="t-box">${o.box}</div>` : ''}</div>`;

  // Проверка правила адаптации: зоны разбираются полностью, новая зона не пройдёт незамеченной.
  K.adapt = (geo, m) => {
    switch (geo.zone) {
      case 'left': return m.left(geo);
      case 'right': return m.right(geo);
      case 'center': return m.center(geo);
      case 'bottom': return m.bottom(geo);
      default: throw new Error(`unknown zone ${geo.zone}`);
    }
  };
})();
