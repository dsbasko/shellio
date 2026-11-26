// Виды демо-плагинов сцены сверки. В каталог обычного мира они не входят: плагин называет сцена,
// а её загрузчик берёт вид отсюда.
(() => {
  const { esc } = S;
  S.sceneViews = {
    // «Демо: поле»: поле ввода и подпись с сохранённым значением. Получатель ввода — само поле:
    // панель, взявшая фокус, отдаёт его полю, Enter сохраняет черновик, Esc его отбрасывает.
    'demo-native': {
      fieldInput: true,
      init: (p) => { p.state.value = ''; p.state.draft = ''; },
      onFocus: (p) => { p.wantField = true; },
      view: (p) => `<div class="demo-field">${K.field({ bind: 'draft', value: p.state.draft, placeholder: 'Значение' })}<p class="row-s">${p.state.value ? `Сохранено: ${esc(p.state.value)}` : 'Значение не задано'}</p></div>`,
    },
  };
  // Enter в поле сохраняет черновик, Esc возвращает сохранённое значение; фокус с поля по Esc
  // снимает маршрутизатор клавиш.
  document.addEventListener('keydown', (e) => {
    const el = e.target;
    if (!el.matches || !el.matches('.demo-field input')) return;
    const panelEl = el.closest('.panel[data-panel]'); const p = panelEl && S.st.panels[panelEl.dataset.panel]; if (!p) return;
    if (e.key === 'Enter') { p.state.value = el.value; p.state.draft = el.value; S.render(); }
    if (e.key === 'Escape') { p.state.draft = p.state.value; el.value = p.state.value; }
  }, true);
})();
