// Маршрутизатор клавиш. Верхний слой получает клавишу первым, скрытые слои её не видят.
(() => {
  const St = S.st;
  const CODE = { Semicolon: [';', ':'], Slash: ['/', '?'], Comma: [',', '<'], Period: ['.', '>'], BracketLeft: ['[', '{'], BracketRight: [']', '}'], Backslash: ['\\', '|'], Minus: ['-', '_'], Equal: ['=', '+'], Quote: ["'", '"'], Backquote: ['`', '~'], Digit1: ['1', '!'], Digit2: ['2', '@'], Digit3: ['3', '#'], Digit4: ['4', '$'], Digit5: ['5', '%'], Digit6: ['6', '^'], Digit7: ['7', '&'], Digit8: ['8', '*'], Digit9: ['9', '('], Digit0: ['0', ')'] };
  const NAMED = { ' ': 'Space', Enter: 'Enter', Escape: 'Esc', Tab: 'Tab', Backspace: 'Backspace', Delete: 'Delete', ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right' };

  S.token = (e, physical = true) => {
    if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Dead', 'Process'].includes(e.key)) return null;
    let base = NAMED[e.key];
    const isNamed = !!base;
    if (!base) {
      const m = e.code && e.code.match(/^Key([A-Z])$/);
      const phys = physical || e.ctrlKey || e.metaKey;
      if (m && phys) base = e.shiftKey ? m[1] : m[1].toLowerCase();
      else if (e.code && CODE[e.code] && phys) base = CODE[e.code][e.shiftKey ? 1 : 0];
      else base = e.key;
    }
    let mods = '';
    if (e.metaKey) mods += 'Cmd-';
    if (e.ctrlKey) mods += 'Ctrl-';
    if (e.altKey) mods += 'Alt-';
    if (isNamed && e.shiftKey && base !== 'Space') mods += 'Shift-';
    return mods + base;
  };

  S.commands = [];
  S.cmd = (c) => { S.commands.push(c); };
  S.command = (id) => S.commands.find((c) => c.id === id);
  S.run = (id, arg) => { const c = S.command(id); if (c) c.run(arg); };
  S.keysOf = (c) => St.keymap[c.id] || c.keys || [];
  // Подпись действующей клавиши: привычная, пока она назначена, иначе первая из назначенных.
  S.keyFor = (id, usual) => { const c = S.commands.find((x) => x.id === id); const all = c ? S.keysOf(c) : []; return all.includes(usual) ? usual : all[0] || ''; };
  S.modeKey = () => St.keymap.escape || 'Ctrl-\\';
  S.navKeys = () => ['h', 'j', 'k', 'l'].map((d) => S.keyFor(`focus.${d}`, d)).filter(Boolean).join(' ');
  S.keyGroups = { Space: 'Команды', 'Space g': 'Git', 'Space r': 'Ревью', 'Space w': 'Рабочая область', 'Space z': 'Зоны', 'Ctrl-w': 'Панели и зоны', g: 'Переход', ']': 'Вперёд', '[': 'Назад' };

  const parse = (k) => k.split(' ');
  const eq = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
  const starts = (a, p) => a.length > p.length && p.every((x, i) => x === a[i]);

  S.bindings = () => {
    const out = [];
    const p = S.focused();
    if (p) {
      const def = S.plugins[p.plugin];
      Object.entries(def.actions || {}).forEach(([name, a]) => {
        if (!a.key || (a.when && !a.when(p))) return;
        [].concat(a.key).forEach((k) => out.push({ seq: parse(k), title: a.title, run: () => a.run(p), local: true, noField: !!a.noField, name }));
      });
    }
    const taken = new Set(out.map((b) => b.seq.join(' ')));
    S.commands.forEach((c) => {
      if (c.when && !c.when()) return;
      S.keysOf(c).forEach((k) => { if (!taken.has(k)) out.push({ seq: parse(k), title: c.title, run: () => c.run(), id: c.id, hidden: c.hidden }); });
    });
    return out;
  };

  S.conflict = (seq, exceptId) => S.commands.find((c) => c.id !== exceptId && S.keysOf(c).includes(seq)) || null;

  let whichTimer = null;
  const setPending = (seq) => {
    St.pending = seq;
    clearTimeout(whichTimer);
    if (seq.length) { whichTimer = setTimeout(() => { St.whichShown = true; S.render(); }, seq[0] === 'Space' || seq[0] === 'Ctrl-w' ? 90 : 380); }
    else St.whichShown = false;
    S.render();
  };
  S.clearPending = () => setPending([]);

  function nav(e, tok) {
    const seq = [...St.pending, tok];
    const binds = S.bindings();
    const exact = binds.find((b) => eq(b.seq, seq));
    const longer = binds.some((b) => starts(b.seq, seq));
    if (exact && !longer) { setPending([]); exact.run(); return true; }
    if (longer) { setPending(seq); return true; }
    if (St.pending.length) { setPending([]); return true; }
    return tok.length === 1;
  }

  let lastEscape = 0;
  function onKey(e) {
    if (e.isComposing) return;
    if (S.kb && S.kb.capture) { const t = S.token(e); if (t) { e.preventDefault(); e.stopPropagation(); S.kb.capture(t, e); } return; }
    const physical = S.mode() === 'nav' && !S.isField(document.activeElement);
    const tok = S.token(e, physical);
    if (!tok) return;
    St.kbNav = true;
    const stop = () => { e.preventDefault(); e.stopPropagation(); };

    // Номера воркспейсов принадлежат приложению: работают из любого режима и поля.
    const wsCmd = tok.startsWith('Cmd-') && S.commands.find((c) => c.id.startsWith('ws.go.') && S.keysOf(c).includes(tok));
    if (wsCmd) { stop(); wsCmd.run(); return; }

    const top = St.overlays[St.overlays.length - 1];
    if (top && top.onKey) {
      const r = top.onKey(e, tok);
      if (r !== false) { if (r !== 'native') stop(); return; }
    }
    if (tok.startsWith('Cmd-')) {
      // Из поля не срабатывают действия, которые адресуют поверхность панели, а не это поле: например, смена агента.
      const inField = S.isField(document.activeElement);
      const b = S.bindings().find((x) => eq(x.seq, [tok]) && !(x.noField && inField));
      if (b) { stop(); b.run(); }
      return;
    }

    const field = S.isField(document.activeElement);
    if (field) {
      if (tok === 'Esc') { if (document.activeElement.dataset.ownEsc != null) return; stop(); document.activeElement.blur(); S.render(); return; }
      return;
    }

    const p = S.focused();
    const escChord = St.keymap.escape || 'Ctrl-\\';
    if (S.mode() === 'input') {
      if (tok === escChord) {
        stop();
        lastEscape = performance.now();
        St.lastInputPanel = p;
        St.prefs.seenEscape = true;
        S.setMode('nav');
        return;
      }
      if (p) {
        const def = S.plugins[p.plugin];
        if (def.onKey) def.onKey(p, e, tok);
      }
      stop();
      return;
    }

    if (tok === escChord && performance.now() - lastEscape < 450 && St.lastInputPanel === p && p) {
      stop();
      const def = S.plugins[p.plugin];
      if (def.onKey) def.onKey(p, e, tok);
      S.setMode('input');
      return;
    }
    if (tok === escChord) { stop(); S.run('mode.toggle'); return; }
    if (tok === 'Esc' && St.pending.length) { stop(); setPending([]); return; }
    if (nav(e, tok)) stop();
  }

  document.addEventListener('keydown', onKey, true);
})();
