// Запуск прототипа: раскладки рабочих областей и общий такт.
(() => {
  const St = S.st;

  S.seedLayout = (wb, tpl) => {
    const L = S.layout(wb);
    if (Object.values(St.panels).some((p) => p.wb === wb)) return;
    const prev = { ...St.route };
    S.setRoute(wb);
    S.open('navigator', { zone: 'left', focus: false });
    if (S.isHomeKey(wb)) {
      // Каталог открывает S.land: он один на воркспейс и переезжает сюда.
    } else if (wb === 'feature-sso') {
      ['claude-sso', 'codex-tests'].forEach((sid) => S.open('agent', { sid, key: sid, zone: 'center', focus: false }));
      // Третий агент работает без открытого вида и поэтому стоит в «В фоне».
      if (W.sessions['claude-docs']) W.sessions['claude-docs'].background = true;
      if (S.plugins.editor && W.sessions.nvim && W.sessions.nvim.wb === wb) S.open('editor', { sid: 'nvim', file: 'src/auth/sso.rs', line: 44, col: 8, zone: 'center', focus: false }).scrollTops = [700];
      S.open('changes', { zone: 'right', focus: false });
      if (S.plugins.history) S.open('history', { zone: 'right', focus: false });
      S.open('terminal', { sid: 'zsh', zone: 'bottom', focus: false });
      if (S.plugins.pipelines) S.open('pipelines', { zone: 'bottom', focus: false });
      const [g, right, bottom] = ['center', 'right', 'bottom'].map((z) => S.partsOf(L, z)[0]);
      g.active = g.tabs[0]; right.active = right.tabs[0]; bottom.active = bottom.tabs[0];
      L.focus = g.active;
      // Окно открывается в навигации: клавиши идут в программу после Ctrl-\ или щелчка по ней.
      S.panelsOf(wb).forEach((p) => { p.mode = 'nav'; });
    } else if (wb === 'review' && S.plugins.review) {
      const p = S.open('review', { key: '!142', target: '!142', zone: 'center', focus: false });
      L.hidden.right = true; L.hidden.bottom = true; L.focus = p.id;
    } else if (tpl === 'review' && S.plugins.review) {
      const p = S.open('review', { key: 'list', zone: 'center', focus: false }); L.focus = p.id;
    } else if (tpl === 'task') {
      // Агента поручения добавляет S.launchTaskAgent.
    } else if (tpl === 'agent') {
      const a = S.launchAgent(W.profiles.find((f) => f.default) || W.profiles[0], '', 'center');
      S.open('changes', { zone: 'right', focus: false });
      L.focus = a.id;
    } else {
      const t = S.open('terminal', { zone: 'center', focus: false });
      L.focus = t.id;
    }
    St.route = prev;
  };

  const params = new URLSearchParams(location.search);
  setInterval(() => {
    if (params.has('still')) return;
    const changed = S.tickAgents();
    const s = W.sessions['make-test'];
    if (s && s.host === 'ok') { s.cpu = Math.round((20.3 + Math.random() * 3.6) * 10) / 10; }
    if (changed || s) S.render();
  }, 1000);

  S.seedLayout('feature-sso');
  S.renderNow();
  requestAnimationFrame(() => S.renderNow());
  const t = params.get('theme');
  if (t) S.setTheme(t);
})();
