// «История»: граф коммитов, коммит, файлы и diff, история файла и строк, сравнение ревизий (ADR-0033).
(() => {
  const { esc, cx, icon } = S;
  const WIP = '0000000';
  const RH = 26, LW = 14, X0 = 9;
  const timers = new Map();
  W.historyJobs = W.historyJobs || {};
  const job = (p) => W.historyJobs[p.id];

  const wbOf = (p) => W.workbenches[p.wb] || S.wb();
  const graph = (p) => { const w = wbOf(p); return w ? W.graphOf(w.repo) : { order: [], branches: {}, remotes: {}, tags: {} }; };
  const headOf = (p) => { const w = wbOf(p), g = graph(p); return (w && g.branches[w.branch]) || g.order[0] || null; };
  const wipList = (p) => W.changesOf(p.wb).files || [];
  const commitOf = (sha) => W.commits[sha] || null;
  const parentsOf = (sha) => { const c = commitOf(sha); return c ? (c.parents || (c.parent ? [c.parent] : [])) : []; };
  const renamesOf = (sha) => W.commitRenames[sha] || {};
  const base = (path) => path.split('/').pop();
  const dir = (path) => path.split('/').slice(0, -1).join('/');
  const short = (sha) => (sha === WIP ? 'рабочая копия' : sha || '');
  const wideCenter = (geo) => geo.zone === 'center' && geo.size === 'Wide';
  const bottomCols = (geo) => geo.zone === 'bottom' && geo.width >= 640;
  const deep = (geo) => !wideCenter(geo) && !bottomCols(geo);

  function ancestors(sha) {
    const out = new Set(), stack = [sha];
    while (stack.length) { const s = stack.pop(); if (!s || out.has(s)) continue; out.add(s); stack.push(...parentsOf(s)); }
    return out;
  }

  function refsOf(p, sha) {
    const g = graph(p), w = wbOf(p), out = [];
    Object.entries(g.branches || {}).forEach(([n, s]) => { if (s === sha) out.push({ t: 'branch', n, head: !!w && n === w.branch }); });
    Object.entries(g.remotes || {}).forEach(([n, s]) => { if (s === sha) out.push({ t: 'remote', n }); });
    Object.entries(g.tags || {}).forEach(([n, s]) => { if (s === sha) out.push({ t: 'tag', n }); });
    return out;
  }

  // Построчный diff двух снимков с тремя строками контекста. Снимки маленькие, поэтому хватает LCS.
  const diffCache = new Map();
  function lineDiff(a, b) {
    a = a || []; b = b || [];
    if (a === b) return [];
    const key = `${a.join('\n')}\u0000${b.join('\n')}`;
    if (diffCache.has(key)) return diffCache.get(key);
    const n = a.length, m = b.length, dp = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    const ops = []; let i = 0, j = 0;
    while (i < n && j < m) {
      if (a[i] === b[j]) { ops.push({ t: ' ', o: i + 1, n: j + 1, s: a[i] }); i++; j++; } else if (dp[i + 1][j] >= dp[i][j + 1]) { ops.push({ t: '-', o: i + 1, n: null, s: a[i] }); i++; } else { ops.push({ t: '+', o: null, n: j + 1, s: b[j] }); j++; }
    }
    while (i < n) { ops.push({ t: '-', o: i + 1, n: null, s: a[i] }); i++; }
    while (j < m) { ops.push({ t: '+', o: null, n: j + 1, s: b[j] }); j++; }
    const changed = ops.map((x, k) => (x.t === ' ' ? -1 : k)).filter((k) => k >= 0), spans = [];
    changed.forEach((k) => { const last = spans[spans.length - 1]; if (last && k - 3 <= last[1] + 1) last[1] = Math.min(ops.length - 1, k + 3); else spans.push([Math.max(0, k - 3), Math.min(ops.length - 1, k + 3)]); });
    const out = spans.map(([x, y]) => {
      const lines = ops.slice(x, y + 1), os = lines.filter((l) => l.o != null), ns = lines.filter((l) => l.n != null);
      const o0 = os.length ? os[0].o : (ops.slice(0, x).reverse().find((l) => l.o != null) || { o: 0 }).o;
      const n0 = ns.length ? ns[0].n : (ops.slice(0, x).reverse().find((l) => l.n != null) || { n: 0 }).n;
      return { header: `@@ -${o0},${os.length} +${n0},${ns.length} @@`, lines };
    });
    diffCache.set(key, out); return out;
  }
  const same = (x, y) => x === y || (!!x && !!y && x.join('\n') === y.join('\n'));
  // Снимок ревизии; у рабочей копии это HEAD с сохранёнными буферами изменённых файлов.
  const treeOf = (p, sha) => {
    if (sha !== WIP) return W.treeOf(sha);
    const t = { ...W.treeOf(headOf(p)) };
    wipList(p).forEach((f) => { const b = W.buffers[f.path]; if (b && b.lines) t[f.path] = b.savedLines || b.lines; });
    return t;
  };
  // Diff файла в ревизии: у коммита от первого родителя, у корневого от пустого дерева, у рабочей копии от HEAD.
  function hunksOf(p, sha, path) {
    if (sha === WIP) {
      if (!wipList(p).some((f) => f.path === path)) return [];
      return W.buffers[path] ? lineDiff(W.treeOf(headOf(p))[path], treeOf(p, WIP)[path]) : W.diffs[path] || [];
    }
    const from = renamesOf(sha)[path] || path;
    return lineDiff(W.treeOf(parentsOf(sha)[0])[from], W.treeOf(sha)[path]);
  }
  // Перенос диапазона строк через ханки: так же, как git log -L.
  function table(h) {
    const map = new Map(); let dels = [], adds = [];
    const flush = () => { adds.forEach((n, k) => map.set(n, k < dels.length ? dels[k] : null)); dels = []; adds = []; };
    h.lines.forEach((l) => {
      if (l.t === ' ') { flush(); map.set(l.n, l.o); } else if (l.t === '-') { if (adds.length) flush(); dels.push(l.o); } else adds.push(l.n);
    });
    flush();
    const ns = h.lines.filter((l) => l.n != null).map((l) => l.n), os = h.lines.filter((l) => l.o != null).map((l) => l.o);
    return { map, n0: ns.length ? Math.min(...ns) : null, n1: ns.length ? Math.max(...ns) : null, o0: os.length ? Math.min(...os) : null, nc: ns.length, oc: os.length };
  }
  function toOld(hunks, x) {
    let shift = 0;
    for (const t of hunks.map(table)) {
      if (t.n0 != null && x >= t.n0 && x <= t.n1) return t.map.has(x) ? t.map.get(x) : null;
      if (t.n0 != null ? t.n1 < x : t.o0 <= x - shift) shift += t.nc - t.oc;
    }
    return x - shift;
  }
  function touches(hunks, a, b) {
    return hunks.some((h) => {
      let prev = (h.lines.find((l) => l.n != null) || { n: 1 }).n - 1;
      return h.lines.some((l) => {
        if (l.t === '+') { prev = l.n; return l.n >= a && l.n <= b; }
        if (l.t === ' ') { prev = l.n; return false; }
        return prev >= a && prev < b;
      });
    });
  }
  function mapRange(hunks, a, b) {
    let oa = null, ob = null;
    for (let x = a; x <= b && oa == null; x++) oa = toOld(hunks, x);
    for (let x = b; x >= a && ob == null; x--) ob = toOld(hunks, x);
    return oa == null || ob == null ? null : [Math.min(oa, ob), Math.max(oa, ob)];
  }

  // История строк по правилу -L: слияние, которое не меняло диапазон, ведёт в совпадающего родителя.
  function lineHistory(p) {
    const t = p.state; let path = t.path, [a, b] = t.lines; const out = [];
    if (!t.at) {
      const hunks = hunksOf(p, WIP, path);
      if (hunks.length) {
        if (touches(hunks, a, b)) out.push({ sha: WIP, path, range: [a, b] });
        const r = mapRange(hunks, a, b); if (!r) return out;
        [a, b] = r;
      }
    }
    for (let s = t.at || headOf(p), guard = 0; s && guard < 500; guard++) {
      const tree = W.treeOf(s);
      if (!tree[path]) break;
      const from = renamesOf(s)[path] || path, ps = parentsOf(s);
      const diffs = ps.map((q) => lineDiff(W.treeOf(q)[from], tree[path]));
      const k = ps.findIndex((q, i) => W.treeOf(q)[from] && !touches(diffs[i], a, b));
      if (k >= 0) { [a, b] = mapRange(diffs[k], a, b) || [a, b]; s = ps[k]; path = from; continue; }
      out.push({ sha: s, path, range: [a, b] });
      if (!ps.length) break;
      const r = mapRange(diffs[0], a, b); if (!r) break;
      [a, b] = r; s = ps[0]; path = from;
    }
    return out;
  }

  // История файла с упрощением по правилу --follow: слияние, совпадающее с родителем по файлу, не показывается.
  function fileHistory(p) {
    const t = p.state, keep = new Set(), seen = new Set(), queue = [[t.at || headOf(p), t.path]];
    while (queue.length) {
      const [s, f] = queue.shift(), key = `${s}|${f}`;
      if (!s || seen.has(key)) continue; seen.add(key);
      const v = W.treeOf(s)[f]; if (!v) continue;
      const from = renamesOf(s)[f] || f, ps = parentsOf(s);
      const twin = ps.find((q) => same(W.treeOf(q)[from], v));
      if (twin) { queue.push([twin, from]); continue; }
      keep.add(s); ps.forEach((q) => queue.push([q, from]));
    }
    return graph(p).order.filter((s) => keep.has(s));
  }

  const searchText = (p, s) => {
    if (s === WIP) return `Рабочая копия ${wipList(p).map((f) => f.path).join(' ')}`;
    const c = commitOf(s);
    return `${c.sha} ${c.title} ${c.author} ${(c.files || []).join(' ')} ${refsOf(p, s).map((r) => r.n).join(' ')}`;
  };

  function visible(p) {
    const t = p.state, g = graph(p), head = headOf(p);
    let list;
    if (t.path && t.lines) list = lineHistory(p).map((e) => e.sha);
    else if (t.path) {
      list = fileHistory(p);
      if (!t.at && wipList(p).some((f) => f.path === t.path)) list.unshift(WIP);
    } else {
      list = g.order.slice();
      if (t.scope === 'head' && head) { const a = ancestors(head); list = list.filter((s) => a.has(s)); }
      if (wipList(p).length && head && list.includes(head)) list.unshift(WIP);
    }
    if (p.filter && t.area === 'graph') list = list.filter((s) => S.match(searchText(p, s), p.filter));
    return list;
  }

  const cursor = (p) => { const list = visible(p), head = headOf(p); return list.includes(p.state.sel) ? p.state.sel : list.includes(head) ? head : list[0] || null; };

  // Раскладка линий. Скрытые коммиты пропускаются с сохранением порядка родителей,
  // выделенная цепочка идёт по исходным первым родителям от HEAD.
  function layout(p, list) {
    const vis = new Set(list), head = headOf(p), rmemo = new Map(), memo = new Map();
    const resolve = (q) => {
      if (rmemo.has(q)) return rmemo.get(q);
      rmemo.set(q, []);
      let out;
      if (vis.has(q)) out = [q];
      else { out = []; parentsOf(q).forEach((x) => resolve(x).forEach((r) => { if (!out.includes(r)) out.push(r); })); }
      rmemo.set(q, out); return out;
    };
    const vparents = (sha) => {
      if (memo.has(sha)) return memo.get(sha);
      const out = [];
      (sha === WIP ? (head ? [head] : []) : parentsOf(sha)).forEach((q) => resolve(q).forEach((r) => { if (!out.includes(r)) out.push(r); }));
      memo.set(sha, out); return out;
    };
    const next = new Map();
    let prev = vis.has(WIP) ? WIP : null;
    for (let s = head; s; s = parentsOf(s)[0]) if (vis.has(s)) { if (prev) next.set(prev, s); prev = s; }
    const chain = new Set([...next.keys(), ...next.values(), ...(prev ? [prev] : [])]);

    let lanes = [], hot = [], width = 1;
    const rows = list.map((sha) => {
      const before = lanes, beforeHot = hot;
      let col = before.indexOf(sha);
      const inc = []; before.forEach((s, i) => { if (s === sha) inc.push(i); });
      if (col < 0) col = before.length;
      const nextLanes = before.map((s) => (s === sha ? null : s));
      const nextHot = beforeHot.map((h, i) => (before[i] === sha ? false : h));
      const ps = vparents(sha), outHot = [];
      ps.forEach((q, k) => {
        const h = next.get(sha) === q;
        outHot.push(h);
        const at0 = nextLanes.indexOf(q);
        if (at0 >= 0) { if (h) nextHot[at0] = true; return; }
        let at = k === 0 && nextLanes[col] == null ? col : nextLanes.indexOf(null);
        if (at < 0) at = nextLanes.length;
        nextLanes[at] = q; nextHot[at] = h;
      });
      const keep = nextLanes.map((_, i) => i).filter((i) => nextLanes[i] != null);
      const to = {}; keep.forEach((i, n) => { to[i] = n; });
      const after = keep.map((i) => nextLanes[i]), afterHot = keep.map((i) => nextHot[i]);
      const edges = [];
      before.forEach((s, i) => { if (s !== sha) edges.push([i, 0, to[i], 2, afterHot[to[i]]]); });
      inc.forEach((i) => edges.push([i, 0, col, 1, beforeHot[i]]));
      ps.forEach((q, k) => edges.push([col, 1, to[nextLanes.indexOf(q)], 2, outHot[k], sha === WIP]));
      width = Math.max(width, before.length, col + 1, after.length);
      lanes = after; hot = afterHot;
      return { sha, col, edges, hot: chain.has(sha), head: sha === head, merge: parentsOf(sha).length > 1 };
    });
    return { rows, width };
  }

  function lanesSvg(r, width) {
    const w = X0 * 2 + (width - 1) * LW, y = [0, RH / 2, RH], x = (i) => X0 + i * LW;
    const path = ([a, ya, b, yb, h, dash]) => {
      const x1 = x(a), x2 = x(b), y1 = y[ya], y2 = y[yb], my = (y1 + y2) / 2;
      const d = x1 === x2 ? `M${x1} ${y1}V${y2}` : `M${x1} ${y1}C${x1} ${my} ${x2} ${my} ${x2} ${y2}`;
      return `<path class="${cx(h && 'h', dash && 'w')}" d="${d}"/>`;
    };
    const node = `<circle class="${cx(r.sha === WIP ? 'w' : r.merge ? 'm' : r.hot && 'h', r.head && 'hd')}" cx="${x(r.col)}" cy="${RH / 2}" r="${r.head ? 4.5 : 3.5}"/>`;
    return `<svg class="hg" width="${w}" height="${RH}" viewBox="0 0 ${w} ${RH}" aria-hidden="true">${r.edges.map(path).join('')}${node}</svg>`;
  }

  function refChips(p, sha, compact) {
    let refs = refsOf(p, sha);
    // Upstream на том же коммите, что и ветка, отдельной меткой не показывается.
    refs = refs.filter((r) => r.t !== 'remote' || !refs.some((l) => l.t === 'branch' && r.n === `origin/${l.n}`));
    if (compact) refs = refs.filter((r) => r.t !== 'remote');
    const mark = p.state.compareBase === sha ? `<span class="href cmp" title="База сравнения: c на другом коммите задаёт цель">${icon('pin', 11)}база</span>` : '';
    return mark + refs.map((r) => `<span class="${cx('href', r.t, r.head && 'head')}" title="${esc(r.t === 'tag' ? `Тег ${r.n}` : r.t === 'remote' ? `Ветка на сервере ${r.n}` : r.head ? `${r.n}, ветка рабочей области` : `Ветка ${r.n}`)}">${icon(r.t === 'tag' ? 'label' : 'branch', 11)}<span>${esc(r.n)}</span></span>`).join('');
  }

  function row(p, r, width, geo, on) {
    const sha = r.sha, wip = sha === WIP, c = wip ? null : commitOf(sha);
    const compact = geo.zone === 'left' || geo.zone === 'right' || geo.width < 560;
    const title = wip ? 'Рабочая копия' : esc(c.title);
    const cells = compact
      ? `<span class="hist-d">${wip ? 'сейчас' : esc(c.age)}</span>`
      : `<span class="hist-a">${wip ? `${wipList(p).length} в работе` : esc(c.author)}</span><span class="hist-d">${wip ? 'сейчас' : esc(c.age)}</span><span class="hist-sha">${wip ? '' : esc(sha)}</span>`;
    return `<div class="${cx('hist-row', on && 'on', wip && 'wip')}" role="button" data-do="pick" data-arg="${sha}" data-k="h-${sha}"${on ? ' data-cur="1"' : ''}${wip ? ' title="Незакоммиченные изменения. Enter открывает «Изменения»"' : ''}>${lanesSvg(r, width)}<span class="hist-t">${refChips(p, sha, compact)}<span class="hist-m">${title}</span></span>${cells}</div>`;
  }

  function graphView(p, geo) {
    const t = p.state, list = visible(p);
    if (!list.length) {
      if (p.filter) return K.empty({ icon: 'history', title: 'Совпадений нет', text: 'Поиск идёт по сообщению, автору, SHA и пути файла.', actions: K.btn('Очистить поиск', { do: 'clearFilter' }) });
      if (t.path) return K.empty({ icon: 'history', title: t.lines ? 'Эти строки не менялись' : 'Файл ещё не входил в коммиты', text: esc(t.path), actions: K.btn('Вся история', { do: 'clearPath' }) });
      return K.empty({ icon: 'history', title: 'В репозитории нет коммитов', text: 'Первый коммит появится здесь.' });
    }
    // При поиске соседние совпадения не обязательно родители, поэтому строки идут списком без линий.
    const head = headOf(p);
    const { rows, width } = p.filter ? { rows: list.map((sha) => ({ sha, col: 0, edges: [], hot: false, head: sha === head, merge: parentsOf(sha).length > 1 })), width: 1 } : layout(p, list);
    const cur = cursor(p);
    const note = t.lines && list.length === 1 && list[0] === WIP ? '<div class="hist-note">Строки изменены в рабочей копии и ещё не входили в коммиты.</div>' : '';
    return K.scroll(`<div class="hist">${rows.map((r) => row(p, r, width, geo, r.sha === cur)).join('')}${note}</div>`);
  }

  // Что показывает правая часть: выбранный коммит или сравнение базы и цели.
  const target = (p) => (p.state.compare ? { kind: 'compare', ...p.state.compare } : { kind: 'commit', sha: cursor(p) });
  // Ревизии сторон diff: null означает рабочую копию.
  const sides = (p, tg) => (tg.kind === 'compare'
    ? { old: tg.a === WIP ? null : tg.a, new: tg.b === WIP ? null : tg.b }
    : { old: tg.sha === WIP ? headOf(p) : parentsOf(tg.sha)[0] || null, new: tg.sha === WIP ? null : tg.sha });

  // Сравнение снимков базы и цели: файлы и diff в координатах этих двух ревизий.
  function compareEntries(p, tg) {
    const ta = treeOf(p, tg.a), tb = treeOf(p, tg.b);
    return [...new Set([...Object.keys(ta), ...Object.keys(tb)])].filter((f) => !same(ta[f], tb[f])).sort()
      .map((f) => ({ path: f, hunks: lineDiff(ta[f], tb[f]), st: !ta[f] ? 'A' : !tb[f] ? 'D' : 'M' }));
  }

  function entries(p) {
    const tg = target(p);
    if (tg.kind === 'compare') return compareEntries(p, tg);
    if (!tg.sha) return [];
    if (tg.sha === WIP) return wipList(p).map((f) => ({ path: f.path, from: f.from || null, hunks: hunksOf(p, WIP, f.path), st: f.status }));
    const parent = W.treeOf(parentsOf(tg.sha)[0]), tree = W.treeOf(tg.sha);
    return (commitOf(tg.sha)?.files || []).map((f) => { const from = renamesOf(tg.sha)[f] || null; return { path: f, from, hunks: hunksOf(p, tg.sha, f), st: from ? 'R' : !parent[f] ? 'A' : !tree[f] ? 'D' : 'M' }; });
  }
  const shownFiles = (p) => entries(p).filter((e) => p.state.area !== 'commit' || S.match(e.path, p.filter));
  const entryOf = (p, path) => entries(p).find((e) => e.path === path) || null;

  function fileRows(p, active) {
    const list = shownFiles(p);
    if (!list.length) return p.filter && p.state.area === 'commit' ? K.empty({ icon: 'file', title: 'Совпадений нет', actions: K.btn('Очистить фильтр', { do: 'clearFilter' }) }) : K.empty({ icon: 'file', title: 'Файлов нет', text: 'Ревизии совпадают.' });
    const focused = S.focused() === p;
    return list.map((e, i) => {
      const flat = K.flat(e), here = active && i === p.state.fileIndex;
      const note = e.from ? `<span class="dir">из ${esc(base(e.from))}</span>` : dir(e.path) ? `<span class="dir">${esc(dir(e.path))}</span>` : '';
      return K.frow({ k: `f-${e.path}`, on: here, kb: focused && here && !!S.st.kbNav, status: e.st, name: esc(base(e.path)), note, a: flat.filter((l) => l.t === '+').length, d: flat.filter((l) => l.t === '-').length, do: 'openDiff', arg: e.path });
    }).join('');
  }

  // Ханки выбранного коммита, которые задевают прослеженный диапазон строк.
  function lineSection(p, sha) {
    const t = p.state;
    if (!t.path || !t.lines) return '';
    const e = lineHistory(p).find((x) => x.sha === sha);
    if (!e) return '';
    // Показываются только строки диапазона и по две строки вокруг, даже если ханк длиннее.
    const [a, z] = e.range;
    const hunks = hunksOf(p, sha, e.path).filter((h) => touches([h], a, z)).map((h) => {
      let prev = 0;
      const lines = h.lines.filter((l) => { const at = l.n != null ? (prev = l.n) : prev; return at >= a - 2 && at <= z + 2; });
      return { header: h.header, lines };
    }).filter((h) => h.lines.length);
    if (!hunks.length) return '';
    return `${K.sec(`Строки ${e.range[0]}–${e.range[1]} в ${esc(base(e.path))}`)}${K.diff({ path: e.path, hunks }, { mode: 'unified' })}`;
  }

  function details(p, geo, active, filesOnly = false) {
    const tg = target(p);
    if (tg.kind === 'commit' && !tg.sha) return K.empty({ icon: 'commit', title: 'Коммит не выбран' });
    const n = shownFiles(p).length;
    let head = '';
    if (tg.kind === 'compare') {
      head = filesOnly ? K.sec(`База ${esc(short(tg.a))}, цель ${esc(short(tg.b))}`, n) : `${K.pad(K.kv([['База', esc(short(tg.a))], ['Цель', esc(short(tg.b))]]))}${K.sec('Различающиеся файлы', n)}`;
    } else if (tg.sha === WIP) {
      head = filesOnly ? K.sec('Рабочая копия', n) : `${K.pad(`${K.kv([['Рабочая копия', 'изменения ещё не в коммите'], ['Над', esc(headOf(p) || '')]])}<div style="margin-top:10px">${K.btn('Изменения', { cmd: 'changes', key: 'Space g c', small: true })}</div>`)}${lineSection(p, WIP)}${K.sec('Файлы', n)}`;
    } else {
      const c = commitOf(tg.sha), refs = refsOf(p, tg.sha).map((r) => esc(r.n)).join(', '), ps = parentsOf(tg.sha);
      const parents = ps.length > 1 ? `${esc(ps[0])}, diff от него; ${ps.slice(1).map(esc).join(', ')}` : ps.map(esc).join('') || 'нет, diff от пустого дерева';
      head = filesOnly ? K.sec(`${esc(c.sha)} ${esc(c.title)}`, n) : `${K.pad(K.kv([['Коммит', esc(c.sha)], ['Автор', esc(c.author)], ['Дата', esc(c.when)], [ps.length > 1 ? 'Родители' : 'Родитель', parents], refs ? ['Ветки и теги', refs] : null]))}${K.pad(`<b>${esc(c.title)}</b><br>${esc(c.body || '')}`)}${lineSection(p, tg.sha)}${K.sec('Файлы', n)}`;
    }
    return K.scroll(`${head}<div class="hist-files">${fileRows(p, active)}</div>`);
  }

  function diffData(p) { const e = p.state.diffFile ? entryOf(p, p.state.diffFile) : null; return e || { path: p.state.diffFile || '', hunks: [] }; }

  function diffView(p, geo, width) {
    const d = diffData(p), w = width || geo.width, compact = w < 420;
    const tg = target(p);
    const where = tg.kind === 'compare' ? `база ${short(tg.a)}, цель ${short(tg.b)}` : short(tg.sha);
    const bar = K.ctx(`${K.btn('Назад', { do: 'back', kind: 'flat', small: true, key: 'h' })}${K.crumbs((compact ? [base(d.path)] : d.path.split('/')).map(esc))}`, `${compact ? '' : `<span class="hist-where">${esc(where)}</span>`}${K.btn('nvim', { do: 'gf', small: true, kind: 'flat', key: 'g f' })}`);
    if (!d.hunks.length) return K.stack([bar, K.empty({ icon: 'changes', title: 'Нет текстового diff', text: 'Откройте файл в nvim или вернитесь к коммиту.', actions: `${K.btn('Открыть в nvim', { do: 'gf' })}${K.btn('К коммиту', { do: 'back' })}` })]);
    return K.stack([bar, K.scroll(K.diff(d, { mode: w >= 1000 ? 'split' : 'unified', cursor: S.focused() === p ? p.state.diffLine : null, onLine: 'diffClick' }))]);
  }

  function top(p, geo) {
    const t = p.state, compact = geo.zone === 'left' || geo.zone === 'right' || geo.width < 560;
    const chips = [];
    if (t.path) chips.push(K.chip(esc(`${t.lines ? `${base(t.path)}:${t.lines[0]}${t.lines[1] !== t.lines[0] ? `–${t.lines[1]}` : ''}` : base(t.path)}${t.at ? ` в ${t.at}` : ''}`), { icon: 'file', remove: 'clearPath' }));
    if (t.compareBase) chips.push(K.chip(`База ${esc(short(t.compareBase))}`, { icon: 'pin', tone: 'wait', remove: 'clearCompare' }));
    if (t.area === 'commit' && deep(geo)) {
      const tg = target(p);
      return K.ctx(`${K.btn('Назад', { do: 'back', kind: 'flat', small: true, key: 'h' })}<span class="hist-where">${esc(tg.kind === 'compare' ? `база ${short(tg.a)}, цель ${short(tg.b)}` : short(tg.sha))}</span>`, '');
    }
    const scope = compact || t.path ? '' : K.seg([{ label: 'Все ветки', on: t.scope !== 'head', do: 'scope', arg: 'all', tip: 'b' }, { label: 'Текущая', on: t.scope === 'head', do: 'scope', arg: 'head', tip: 'b' }]);
    if (compact && t.scope === 'head' && !t.path) chips.push(K.chip('Текущая ветка', { icon: 'branch', remove: 'scopeAll' }));
    if (!chips.length && !scope) return '';
    return K.ctx(chips.join(''), scope);
  }

  function banner(p) {
    const j = job(p);
    if (!j || j.status === 'done') return '';
    if (j.status === 'cancelled') return K.banner('', 'Загрузка остановлена', `Показаны первые ${j.loaded} коммитов.`, K.btn('Повторить', { do: 'retry', small: true }));
    return K.banner('wait', `${K.status('run')} ${j.status === 'blocked' ? 'Git не отдаёт следующую порцию' : 'Загружаем историю'}`, j.status === 'blocked' ? `Показаны первые ${j.loaded} коммитов. Отмените загрузку и повторите её.` : 'Коммиты приходят порциями по 500, следующая придёт при прокрутке.', K.btn('Отменить', { do: 'cancel', small: true, key: 'Esc' }));
  }

  function body(p, geo) {
    const t = p.state, b = banner(p);
    if (wideCenter(geo)) {
      const left = t.area === 'diff' ? diffView(p, geo, geo.width - 340) : K.stack([top(p, geo), b, graphView(p, geo)]);
      return K.cols([{ html: left }, { w: '340px', html: details(p, geo, t.area === 'commit') }]);
    }
    if (bottomCols(geo)) {
      const right = t.area === 'diff' ? diffView(p, geo, geo.width * 0.45) : details(p, geo, t.area === 'commit', true);
      return K.cols([{ html: K.stack([top(p, geo), b, graphView(p, geo)]) }, { w: '45%', html: right }]);
    }
    if (t.area === 'diff') return diffView(p, geo);
    if (t.area === 'commit') return K.stack([top(p, geo), details(p, geo, true)]);
    return K.stack([top(p, geo), b, graphView(p, geo)]);
  }

  function mark(p) {
    const snapshot = JSON.parse(JSON.stringify(p.state)); const filter = p.filter;
    const scroll = (p.scrollTops || []).slice();
    S.mark(p, () => {
      Object.assign(p.state, snapshot); p.filter = filter; p.scrollTops = scroll; p._cur = null;
      S.render();
      requestAnimationFrame(() => {
        const el = document.querySelector(`.panel[data-panel="${p.id}"]`);
        if (el) el.querySelectorAll('.scroll').forEach((node, i) => { node.scrollTop = scroll[i] || 0; });
      });
    });
  }

  function cancelTimer(p) { clearTimeout(timers.get(p.id)); timers.delete(p.id); }
  function load(p, blocked) {
    cancelTimer(p);
    if (!blocked) { delete W.historyJobs[p.id]; return; }
    const task = W.historyJobs[p.id] = { status: 'loading', loaded: 500 };
    timers.set(p.id, setTimeout(() => { if (W.historyJobs[p.id] === task && task.status === 'loading') { task.status = 'blocked'; S.render(); } }, 900));
  }

  // Новый контекст: путь и строки относятся к ревизии rev, иначе rev только ставит курсор на коммит в общей истории.
  function apply(p, opts, first) {
    const t = p.state;
    if (!first && !(opts.reset || opts.path || opts.rev || opts.blocked)) return;
    if (!first) mark(p);
    if (first || opts.reset || opts.path || opts.rev) {
      t.path = opts.path || null;
      t.lines = opts.path && opts.lines ? opts.lines.map(Number) : null;
      t.at = opts.path && opts.rev ? opts.rev : null;
    }
    t.area = 'graph'; t.compare = null; t.compareBase = null; t.fileIndex = 0; t.diffFile = null; t.diffLine = 0; p.filter = ''; p._cur = null;
    if (opts.rev && !t.path && !visible(p).includes(opts.rev)) t.scope = 'all';
    const list = visible(p), head = headOf(p);
    if (opts.rev && !t.path && !list.includes(opts.rev)) S.notify({ title: 'Коммит не найден в истории', text: opts.rev, tone: 'wait' });
    t.sel = opts.rev && list.includes(opts.rev) ? opts.rev : (list.includes(head) ? head : list[0]) || null;
    load(p, !!opts.blocked);
  }

  function openCommit(p) {
    const sha = cursor(p); if (!sha) return;
    if (sha === WIP) { S.run('changes'); return; }
    p.state.sel = sha;
    mark(p);
    p.state.area = 'commit'; p.state.fileIndex = 0; p.filter = ''; p._cur = null; S.render();
  }

  function openDiff(p, file) {
    const list = shownFiles(p), path = file || (list[p.state.fileIndex] || {}).path;
    if (!path) return;
    if (p.state.area === 'graph') { p.state.sel = cursor(p); mark(p); p.state.area = 'commit'; }
    p.state.fileIndex = Math.max(0, list.findIndex((e) => e.path === path)); mark(p);
    p.state.diffFile = path; p.state.diffLine = 0; p.state.area = 'diff'; p.filter = ''; p._cur = null; S.render();
  }

  function move(p, delta) {
    const t = p.state;
    if (t.area === 'diff') t.diffLine = S.clamp(t.diffLine + delta, 0, Math.max(0, K.flat(diffData(p)).length - 1));
    else if (t.area === 'commit') t.fileIndex = S.clamp(t.fileIndex + delta, 0, Math.max(0, shownFiles(p).length - 1));
    else { const list = visible(p), i = list.indexOf(cursor(p)); if (list.length) { t.sel = list[S.clamp(i + delta, 0, list.length - 1)]; t.fileIndex = 0; } }
    S.render();
  }

  // Место под курсором: файл, строка, ревизия и сторона diff. Строка удаления относится к старой стороне.
  function location(p) {
    const t = p.state, tg = target(p), rev = sides(p, tg);
    if (t.area === 'diff') {
      const e = diffData(p), r = K.flat(e)[t.diffLine], old = !!r && r.t === '-';
      return { file: old && e.from ? e.from : e.path, line: r ? (old ? r.o : r.n) || 1 : 1, rev: old ? rev.old : rev.new, side: old ? 'old' : 'new' };
    }
    if (t.area === 'commit') {
      const e = shownFiles(p)[t.fileIndex]; if (!e) return {};
      const del = e.st === 'D';
      return { file: del && e.from ? e.from : e.path, line: 1, rev: del ? rev.old : rev.new, side: del ? 'old' : 'new' };
    }
    if (t.path) return { file: t.path, line: t.lines ? t.lines[0] : 1, rev: t.at, side: 'new' };
    return {};
  }

  S.plugin({
    id: 'history', title: 'История', icon: 'history', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'right', cardinality: 'workbench', minSize: { w: 230, h: 90 },
    init(p, opts = {}) { Object.assign(p.state, { path: null, lines: null, at: null, scope: 'all', area: 'graph', sel: null, fileIndex: 0, diffFile: null, diffLine: 0, compare: null, compareBase: null }); apply(p, opts, true); },
    reopen(p, opts = {}) { apply(p, opts, false); },
    tabTitle: (p) => (p.state.path ? `История ${base(p.state.path)}${p.state.lines ? `:${p.state.lines[0]}` : ''}` : 'История'),
    tabStatus: (p) => (job(p) && ['loading', 'blocked'].includes(job(p).status) ? 'run' : null),
    meter: () => ({ kind: 'estimate', text: '≈ 0,1% CPU · 2,1 МБ' }),
    location,
    context: (p) => {
      const tg = target(p), at = location(p), out = [];
      if (at.file) out.push({ label: `${at.file}:${at.line}${at.rev ? ` в ${at.rev}` : ''}`, icon: 'file' });
      if (tg.kind === 'compare') out.push({ label: `Сравнение: база ${short(tg.a)}, цель ${short(tg.b)}`, icon: 'commit' });
      else if (tg.sha && tg.sha !== WIP) out.push({ label: `Коммит ${tg.sha}`, icon: 'commit' });
      return out;
    },
    view(p, geo) {
      const render = () => body(p, geo);
      return K.adapt(geo, { left: render, right: render, center: render, bottom: render });
    },
    actions: {
      down: { title: 'Следующая строка', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
      up: { title: 'Предыдущая строка', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
      enter: { title: 'Открыть коммит или diff', key: ['Enter', 'l'], hint: 'Открыть', when: (p) => p.state.area !== 'diff', run: (p) => {
        if (p.state.area !== 'graph') { openDiff(p); return; }
        if (p.geo && !deep(p.geo) && cursor(p) !== WIP) { p.state.sel = cursor(p); mark(p); p.state.area = 'commit'; p.state.fileIndex = 0; p.filter = ''; S.render(); } else openCommit(p);
      } },
      back: { title: 'Назад', key: 'h', hint: 'Назад', when: (p) => p.state.area !== 'graph', run: () => S.back() },
      scope: { title: 'Все ветки или только текущая', key: 'b', when: (p) => p.state.area === 'graph' && !p.state.path, run: (p, arg) => { p.state.scope = arg === 'all' || arg === 'head' ? arg : p.state.scope === 'head' ? 'all' : 'head'; p._cur = null; S.render(); } },
      scopeAll: { title: '', menu: false, run: (p) => { p.state.scope = 'all'; S.render(); } },
      compare: { title: 'Сравнить: база, затем цель', key: 'c', hint: 'Сравнить', when: (p) => p.state.area === 'graph' && !!cursor(p), run: (p) => {
        const t = p.state, sha = cursor(p);
        if (!t.compareBase) { t.compareBase = sha; S.render(); return; }
        if (t.compareBase === sha) { t.compareBase = null; S.render(); return; }
        mark(p);
        t.compare = { a: t.compareBase, b: sha }; t.compareBase = null; t.area = 'commit'; t.fileIndex = 0; p.filter = ''; S.render();
      } },
      clearCompare: { title: 'Снять базу сравнения', key: 'Esc', when: (p) => !!p.state.compareBase && !(job(p) && ['loading', 'blocked'].includes(job(p).status)), run: (p) => { p.state.compareBase = null; S.render(); } },
      gf: { title: 'Открыть в nvim', key: 'g f', hint: 'nvim', when: (p) => !!location(p).file, run: (p) => {
        const at = location(p);
        S.openFile(at.file, at.line);
        // Ревизия открывается только для чтения, даже HEAD: рабочая копия может отличаться.
        if (at.rev) {
          const snap = W.treeOf(at.rev)[at.file] || [''];
          const buf = W.buffers[at.file] || (W.buffers[at.file] = { lang: /\.rs$/.test(at.file) ? 'rust' : /\.sql$/.test(at.file) ? 'sql' : null, lines: snap.slice(), origin: [], savedLines: snap.slice(), revisions: {} });
          buf.revisions = buf.revisions || {}; buf.revisions[at.rev] = snap;
          const editor = S.findPanel('editor'); if (editor) S.plugins.editor.reopen(editor, { file: at.file, line: at.line, rev: at.rev });
        }
        S.render();
      } },
      pick: { title: '', menu: false, run: (p, sha) => {
        const t = p.state;
        if (t.area !== 'graph' || t.compare) { mark(p); t.area = 'graph'; t.compare = null; }
        t.sel = sha; t.fileIndex = 0;
        if (p.geo && deep(p.geo)) openCommit(p); else S.render();
      } },
      openDiff: { title: '', menu: false, run: openDiff },
      diffClick: { title: '', menu: false, run: (p, line) => { p.state.diffLine = S.clamp(Number(line), 0, Math.max(0, K.flat(diffData(p)).length - 1)); S.render(); } },
      pageUp: { title: 'На страницу вверх', key: 'Ctrl-u', menu: false, run: (p) => move(p, -15) },
      pageDown: { title: 'На страницу вниз', key: 'Ctrl-d', menu: false, run: (p) => move(p, 15) },
      clearPath: { title: 'Вся история', when: (p) => !!p.state.path, run: (p) => { mark(p); Object.assign(p.state, { path: null, lines: null, at: null, area: 'graph' }); p._cur = null; S.render(); } },
      cancel: { title: 'Отменить загрузку истории', key: 'Esc', hint: 'Отменить', when: (p) => job(p) && ['loading', 'blocked'].includes(job(p).status), run: (p) => { cancelTimer(p); job(p).status = 'cancelled'; S.render(); } },
      retry: { title: 'Повторить загрузку истории', when: (p) => job(p) && job(p).status === 'cancelled', run: (p) => { load(p, true); S.render(); } },
      clearFilter: { title: 'Очистить поиск', when: (p) => !!p.filter, run: (p) => { p.filter = ''; S.render(); } },
    },
    destroy(p) { cancelTimer(p); delete W.historyJobs[p.id]; },
  });

  const inArea = () => { if (!S.wb() || S.wb().repo !== 'orbit-api') S.go('feature-sso'); };
  S.scenario({ id: 'history-file', group: 'История', title: 'История файла oidc.rs через переименование', run: () => { inArea(); S.open('history', { path: 'src/auth/oidc.rs' }); } });
  S.scenario({ id: 'history-lines', group: 'История', title: 'История строк 42–45 в sso.rs', run: () => { inArea(); S.open('history', { path: 'src/auth/sso.rs', lines: [42, 45] }); } });
  S.scenario({ id: 'history-compare', group: 'История', title: 'Сравнение: база main, цель feature/sso-login', run: () => {
    inArea(); const p = S.open('history', { reset: true }); if (!p) return;
    Object.assign(p.state, { compare: { a: '8e6d3f4', b: 'a3f9c21' }, area: 'commit', fileIndex: 0, sel: 'a3f9c21' }); S.render();
  } });
  S.scenario({ id: 'history-blocked', group: 'История', title: 'Git не отдаёт следующую порцию истории', run: () => { inArea(); S.open('history', { reset: true, blocked: true }); } });
})();
