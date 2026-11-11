(() => {
  const { esc } = S;
  const tones = { running: 'run', pending: 'idle', manual: 'wait', failed: 'err', ok: 'ok', canceled: 'exit', skipped: 'exit' };
  const labels = { running: 'выполняется', pending: 'в очереди', manual: 'ручной шаг', failed: 'упал', ok: 'успешно', canceled: 'отменён', skipped: 'пропущен' };
  const active = (s) => ['running', 'pending'].includes(s);
  const terminal = (s) => ['failed', 'canceled', 'ok'].includes(s);
  const status = (s) => K.status(tones[s] || 'idle', labels[s] || esc(s));
  const timers = new Map();
  W.pipelineState = W.pipelineState || { offline: false, logs: {}, pins: {} };
  W.pipelineSources = W.pipelineSources || {
    'orbit-api': { project: 'orbit/orbit-api' }, 'orbit-web': { project: 'orbit/orbit-web', workflow: 'ci' },
  };
  W.pipelineChecks = W.pipelineChecks || {
    512: [{ name: 'Code scanning', provider: 'GitHub Advanced Security', state: 'ok' }],
  };
  const state = () => W.pipelineState;
  const source = (r) => ({ host: r.host || W.repos[r.repo].host, project: r.project || (W.pipelineSources[r.repo] || {}).project || r.repo });
  const keyOf = (r) => JSON.stringify([source(r).host, source(r).project, r.id]);
  const identity = (r) => JSON.stringify([keyOf(r), r.attempt, r.ref, r.sha]);
  const allJobs = (r) => r ? (W.jobs[r.id] || []).flatMap((stage) => stage.jobs.map((job) => ({ stage: stage.stage, job }))) : [];
  const reposOf = (p) => W.projects[W.workbenches[p.wb].project].repos;
  const runsOf = (p) => W.pipelines.filter((r) => reposOf(p).includes(r.repo));
  const matches = (p, text) => S.match(text, p.filter || '') > 0;
  const runs = (p) => runsOf(p).filter((r) => matches(p, `${r.repo} ${r.id} ${r.ref} ${r.sha} ${r.title} ${labels[r.state]}`));
  const runAt = (p) => p.state.area === 'list'
    ? runs(p).find((r) => keyOf(r) === p.state.run) || runs(p)[0]
    : runsOf(p).find((r) => keyOf(r) === p.state.run);
  const jobs = (p) => allJobs(runAt(p)).filter(({ stage, job }) => matches(p, `${stage} ${job.name} ${labels[job.state]}`));
  const jobAt = (p) => {
    const r = runAt(p); const list = p.state.area === 'run' ? jobs(p) : allJobs(r);
    return (list.find(({ job }) => job.name === p.state.job) || list.find(({ job }) => job.state === 'failed') || list[0] || {}).job;
  };
  const logKey = (r, j) => JSON.stringify([identity(r), j.name]);
  function logOf(r, j) {
    const key = logKey(r, j);
    if (!state().logs[key]) {
      state().logs[key] = r.id === 8841 && r.attempt === 1 && j.name === 'test:integration' && j.state === 'failed'
        ? W.log.map((line) => [...line])
        : [['', `${j.name}, запуск #${r.id}, попытка ${r.attempt}`], ['', `Ref ${r.ref}, SHA ${r.sha}`], [j.state === 'failed' ? 'err' : '', labels[j.state] || j.state]];
    }
    return state().logs[key];
  }
  function remember(p) {
    const saved = { ...p.state, filter: p.filter };
    const restore = () => { const { filter, ...rest } = saved; Object.assign(p.state, rest); p.filter = filter; };
    S.mark(p, restore);
  }
  function chooseRun(p, r) {
    if (!r) return;
    p.state.run = keyOf(r);
    p.state.job = (allJobs(r).find(({ job }) => job.state === 'failed') || allJobs(r).find(({ job }) => active(job.state)) || allJobs(r)[0] || { job: {} }).job.name || null;
    p.state.line = 0;
  }
  function openRun(p, key) {
    const r = key ? runsOf(p).find((v) => keyOf(v) === key) : runAt(p);
    if (!r) return;
    remember(p); chooseRun(p, r); p.state.area = 'run'; p.filter = ''; S.render();
  }
  function openJob(p, name) {
    const r = runAt(p); const j = name ? (allJobs(r).find((v) => v.job.name === name) || {}).job : jobAt(p);
    if (!r || !j) return;
    if (p.state.area === 'list') openRun(p, keyOf(r));
    if (p.state.area !== 'job') remember(p);
    p.state.area = 'job'; p.state.job = j.name; p.state.line = 0; p.filter = ''; S.render();
  }
  function back(p) {
    const last = S.st.history[S.st.history.length - 1];
    if (last && last.panel === p.id && last.restore) { S.back(); return; }
    p.state.area = p.state.area === 'job' ? 'run' : 'list'; p.filter = ''; S.render();
  }
  function recompute(r) {
    const list = allJobs(r).map(({ job }) => job);
    r.state = list.some((j) => j.state === 'failed') ? 'failed'
      : list.some((j) => active(j.state)) ? 'running'
        : list.some((j) => j.state === 'manual') ? 'manual'
          : list.some((j) => j.state === 'canceled') ? 'canceled' : 'ok';
    r.failed = (list.find((j) => j.state === 'failed') || {}).name;
  }
  function schedule(r) {
    const key = keyOf(r); const attempt = identity(r);
    if (timers.has(key) || state().offline || r.state !== 'running') return;
    timers.set(key, setTimeout(() => {
      timers.delete(key);
      if (!W.pipelines.includes(r) || !W.repos[r.repo]) return;
      if (state().offline || identity(r) !== attempt || r.state !== 'running') return;
      const running = allJobs(r).filter(({ job }) => job.state === 'running');
      running.forEach(({ job }) => { job.state = 'ok'; job.dur = '2 с'; delete job.code; logOf(r, job).push(['', 'Задание завершено, код выхода 0.']); });
      const next = (W.jobs[r.id] || []).find((stage) => stage.jobs.some((j) => j.state === 'pending'));
      if (next) next.jobs.filter((j) => j.state === 'pending').forEach((j) => { j.state = 'running'; logOf(r, j).push(['', 'Задание выполняется.']); });
      recompute(r); S.render();
      if (r.state === 'running') schedule(r);
      else S.notify({ ws: S.repoWs(r.repo), project: S.repoProject(r.repo), title: `Запуск #${r.id}: ${labels[r.state].toLowerCase()}`, text: `${r.repo}, попытка ${r.attempt}, ${r.ref}, SHA ${r.sha}`, tone: tones[r.state], action: { label: 'Открыть запуск', run: () => S.open('pipelines', { wb: S.project() && S.project().repos.includes(r.repo) ? S.st.route.wb : Object.values(W.workbenches).find((w) => W.projects[w.project].repos.includes(r.repo)).id, run: r.id, repo: r.repo }) } });
    }, 2200));
  }
  function invalidate(r) { clearTimeout(timers.get(keyOf(r))); timers.delete(keyOf(r)); }
  S.stopRunWatch = invalidate;
  // Состояние решает, есть ли кнопка. Без аккаунта она приглушена, а обработчик отказывает.
  const stateAllows = (r, j, kind) => !!r && (kind === 'retry' ? terminal(j ? j.state : r.state) : kind === 'cancel' ? active(j ? j.state : r.state) || (!j && r.state === 'manual') : !!j && j.state === 'manual');
  const operationAllowed = (r, j, kind) => stateAllows(r, j, kind) && !!S.forgeAccount(r.repo);
  const muted = (r) => state().offline || !S.forgeAccount(r.repo);
  const pausedNote = (r) => (r && !S.forgeAccount(r.repo) ? `<span class="pl-paused" title="В этом воркспейсе нет аккаунта для ${esc(W.repos[r.repo].host)}">Нет аккаунта</span>${K.btn('Настройки', { cmd: 'settings', small: true, kind: 'flat' })}` : '');
  function operate(p, kind, onJob = false) {
    const r = runAt(p); const j = onJob || kind === 'manual' || p.state.area === 'job' ? jobAt(p) : null;
    if (!operationAllowed(r, j, kind)) return;
    const captured = identity(r); const jobState = j && j.state; const runState = r.state;
    const title = kind === 'retry' ? 'Перезапустить' : kind === 'cancel' ? 'Отменить' : 'Запустить ручной шаг';
    const details = { ...source(r), id: r.id, attempt: r.attempt, ref: r.ref, sha: r.sha, repo: r.repo, job: j && j.name };
    const changed = () => !W.pipelines.includes(r) || identity(r) !== captured || r.state !== runState || (j && (j.state !== jobState || !allJobs(r).some((x) => x.job === j)));
    S.sheet({
      icon: kind === 'cancel' ? 'stop' : 'pipeline', title: `${title}${j ? ` ${esc(j.name)}` : ` запуск #${r.id}`}?`, width: 520,
      sub: kind === 'retry' ? `Будет создана попытка ${r.attempt + 1} для того же SHA.` : kind === 'cancel' ? 'Выполняемая работа будет прервана.' : 'Ручное задание выполнится для указанного запуска.',
      state: { error: '' },
      body: (o) => `${K.kv([['Хост', esc(details.host)], ['Проект', esc(details.project)], ['Репозиторий', esc(details.repo)], ['Запуск', `#${details.id}`], ['Попытка', details.attempt], ['Ref', esc(details.ref)], ['SHA', esc(details.sha)], ['Задание', details.job ? esc(details.job) : 'Весь запуск']])}${details.sha !== W.repos[r.repo].head ? K.banner('wait', 'Запуск старого SHA', `Текущий коммит ${esc(W.repos[r.repo].head)} этим действием не проверяется.`) : ''}${state().offline ? K.banner('err', 'Нет сети', 'Закройте подтверждение и восстановите соединение в панели.') : ''}${o.state.error || changed() ? K.banner('err', o.state.error || 'Запуск изменился', 'Закройте подтверждение и выберите запуск заново.') : ''}`,
      actions: [{ label: 'Закрыть', key: 'Esc' }, {
        label: title, kind: kind === 'cancel' ? 'danger' : 'primary', key: 'Enter', close: false, disabled: () => state().offline || changed(),
        run: (o) => {
          if (state().offline || changed()) { o.state.error = state().offline ? 'Соединение потеряно' : 'Запуск изменился'; S.render(); return; }
          invalidate(r);
          const targets = j ? [j] : allJobs(r).map((x) => x.job);
          if (kind === 'retry') {
            allJobs(r).forEach(({ job }) => logOf(r, job));
            const previous = r.attempt; r.attempt++;
            targets.forEach((job) => { if (j || job.state !== 'manual') { job.state = 'pending'; job.dur = ''; delete job.code; } });
            allJobs(r).filter(({ job }) => !targets.includes(job)).forEach(({ job }) => {
              state().logs[logKey(r, job)] = [['', `Результат сохранён из попытки ${previous}.`], ...state().logs[logKey({ ...r, attempt: previous }, job)].map((line) => [...line])];
            });
            const first = targets.find((job) => job.state === 'pending'); if (first) first.state = 'running';
          } else if (kind === 'cancel') {
            targets.filter((job) => active(job.state) || job.state === 'manual').forEach((job) => { job.state = 'canceled'; logOf(r, job).push(['', 'Отменено пользователем.']); });
          } else { j.state = 'running'; logOf(r, j).push(['', 'Ручной шаг запущен пользователем.']); }
          recompute(r); r.when = 'Сейчас'; r.dur = active(r.state) ? '0 с' : r.dur;
          p.state.line = 0; S.ov.pop(o); S.render(); schedule(r);
        },
      }],
    });
  }
  function actions(p) {
    const r = runAt(p); const j = p.state.area === 'job' ? jobAt(p) : null;
    if (!r) return '';
    return `${stateAllows(r, j, 'retry') ? K.btn('Перезапустить', { do: 'retry', small: true, key: 'r', disabled: muted(r) }) : ''}${stateAllows(r, j, 'cancel') ? K.btn('Отменить', { do: 'cancel', small: true, key: 'x', disabled: muted(r) }) : ''}${stateAllows(r, jobAt(p), 'manual') ? K.btn('Запустить шаг', { do: 'manual', small: true, key: 'm', disabled: muted(r) }) : ''}`;
  }
  function notices(p, r) {
    return `${state().offline ? K.banner('err', 'Нет сети', 'Показан сохранённый снимок. Опрос приостановлен.', K.btn('Подключиться', { do: 'refresh', small: true })) : ''}${r && !S.forgeAccount(r.repo) ? K.banner('wait', 'Операции с сервисом приостановлены', `В этом воркспейсе нет аккаунта для ${esc(W.repos[r.repo].host)}. Подключите его в настройках.`, K.btn('Настройки', { cmd: 'settings', small: true })) : ''}${r && r.sha !== W.repos[r.repo].head ? K.banner('wait', `Старый SHA ${esc(r.sha)}`, `Текущий коммит ${esc(W.repos[r.repo].head)} этим запуском не проверяется.`) : ''}`;
  }
  function header(p) {
    const r = runAt(p);
    return K.ctx(`${p.state.area !== 'list' ? K.iconBtn('chevL', { do: 'back', tip: p.state.area === 'job' ? 'К заданиям' : 'К запускам' }) : ''}${K.crumbs(p.state.area === 'list' ? [esc(W.projects[W.workbenches[p.wb].project].title), 'Запуски'] : [esc(r ? r.repo : ''), r ? `#${r.id}` : 'Запуск', ...(p.state.area === 'job' ? [esc(p.state.job)] : [])])}`, K.iconBtn('refresh', { do: 'refresh', tip: 'Обновить запуски' }));
  }
  function empty(p) { return K.empty({ icon: 'pipeline', title: p.filter ? 'Ничего не найдено' : 'Запусков пока нет', text: p.filter ? 'Измените фильтр или покажите все записи.' : 'Обновите список запусков репозиториев проекта.', actions: K.btn(p.filter ? 'Сбросить фильтр' : 'Обновить', { do: p.filter ? 'clearFilter' : 'refresh' }) }); }
  function runList(p, table = false) {
    const selected = runAt(p); const list = runs(p);
    if (!list.length) return empty(p);
    return reposOf(p).map((id) => {
      const group = list.filter((r) => r.repo === id); if (!group.length) return '';
      const row = (r) => ({ k: keyOf(r), on: r === selected, do: 'openRun', arg: keyOf(r) });
      return `${K.sec(esc(id), S.icon(W.repos[id].provider, 14))}${table ? K.table({ cols: [{ t: 'Запуск', w: '90px' }, { t: 'Состояние', w: '130px' }, { t: 'SHA и ветка' }, { t: 'Попытка', w: '75px', num: true }], rows: group.map((r) => ({ ...row(r), cells: [`#${r.id}`, status(r.state), `${esc(r.sha)}${r.sha !== W.repos[id].head ? ' старый' : ''} <span style="color:var(--text-3)">${esc(r.ref)}</span>`, r.attempt] })) }) : group.map((r) => K.row({ ...row(r), st: tones[r.state], title: `#${r.id} ${esc(r.title)}`, sub: `${esc(r.ref)} / ${esc(r.sha)}${r.sha !== W.repos[id].head ? ' / старый SHA' : ' / текущий SHA'}`, trail: esc(r.dur), tip: `${labels[r.state]}, попытка ${r.attempt}` })).join('')}`;
    }).join('');
  }
  function graphOf(p, r, act = 'openJob') {
    const chosen = jobAt(p);
    const focused = S.focused() === p;
    const visible = jobs(p).map((x) => x.job);
    const stages = (W.jobs[r.id] || []).map((s) => ({ title: esc(s.stage), jobs: s.jobs.filter((j) => !p.filter || visible.includes(j)).map((j) => ({ k: j.name, name: esc(j.name), state: j.state, on: act === 'selectJob' && j === chosen, kb: focused && !!S.st.kbNav && p.state.area === 'run' && j === chosen, do: act, arg: j.name, retry: j.state === 'failed' && !state().offline ? 'retryJob' : null, retryOff: !S.forgeAccount(r.repo) })) })).filter((s) => s.jobs.length);
    return K.graph(stages);
  }
  function jobList(p, table = false, graph = false) {
    const r = runAt(p); const chosen = jobAt(p); const list = jobs(p);
    if (!list.length) return empty(p);
    const row = ({ job }) => ({ k: job.name, on: job === chosen, do: graph || table ? 'selectJob' : 'openJob', arg: job.name });
    if (table) return K.table({ cols: [{ t: 'Задание' }, { t: 'Состояние', w: '125px' }, { t: 'Время', w: '70px', num: true }], rows: list.map((x) => ({ ...row(x), cells: [esc(x.job.name), status(x.job.state), esc(x.job.dur) || '—'] })) });
    const groups = r.provider === 'github' ? [{ stage: `Workflow ${(W.pipelineSources[r.repo] || {}).workflow || 'Actions'}`, jobs: list }] : (W.jobs[r.id] || []).map((s) => ({ stage: s.stage, jobs: list.filter((j) => j.stage === s.stage) })).filter((s) => s.jobs.length);
    const stageHtml = (s, i) => `${K.sec(esc(s.stage), graph && i ? S.icon('chevR', 14) : '')}${s.jobs.map((x) => K.row({ ...row(x), st: tones[x.job.state], title: esc(x.job.name), sub: labels[x.job.state], trail: esc(x.job.dur) })).join('')}`;
    const checks = (W.pipelineChecks[r.id] || []).length ? `${K.sec('Сторонние проверки')}${W.pipelineChecks[r.id].map((c) => K.row({ title: esc(c.name), sub: esc(c.provider), st: tones[c.state], trail: labels[c.state], quiet: true })).join('')}` : '';
    return `${graph ? `<div style="padding:0 16px 12px">${graphOf(p, r, 'selectJob')}</div>` : groups.map(stageHtml).join('')}${checks}`;
  }
  function runInfo(p, compact = false) {
    const r = runAt(p); if (!r) return '';
    return `${K.ctx(`${status(r.state)}${K.chip(`Попытка ${r.attempt}`)}`, compact ? '' : actions(p))}${compact ? K.pad(actions(p)) : ''}${K.pad(K.kv([['Ref', esc(r.ref)], ['SHA', esc(r.sha)], ['Хост', esc(source(r).host)]]))}`;
  }
  function jobInfo(p, withLog = true, compact = false) {
    const r = runAt(p); const j = jobAt(p);
    if (!r || !j) return empty(p);
    const log = logOf(r, j); const filtered = log.map((line, i) => ({ line, i })).filter(({ line }) => matches(p, line[1]));
    const selectedLine = filtered.find((x) => x.i === p.state.line) || filtered[0];
    const ask = K.btn('Спросить', { cmd: 'ask', small: true, tip: 'Спросить агента с закреплённым логом' });
    const summary = `${r.sha !== W.repos[r.repo].head ? 'Старый SHA' : 'SHA'} ${r.sha} / #${r.id}, попытка ${r.attempt} / ${r.ref}`;
    return `${compact ? K.row({ st: tones[j.state], title: esc(j.name), sub: esc(summary), tip: `${labels[j.state]}, ${summary}`, trail: ask, quiet: true }) : `${K.ctx(`${status(j.state)}<span>${esc(j.name)}</span>`, ask)}${K.pad(K.kv([['Запуск', `#${r.id}, попытка ${r.attempt}`], ['Ref', esc(r.ref)], ['SHA', esc(r.sha)], ['Время', esc(j.dur) || '—'], j.code != null && ['Код выхода', j.code]]))}${K.pad(actions(p))}`}${withLog ? K.scroll(filtered.length ? K.code(filtered.map(({ line }) => line[1]), { cursor: p.state.area === 'job' ? filtered.indexOf(selectedLine) : null, gutter: (i) => `<span class="${filtered[i].line[0] === 'err' ? 'c-err' : 'c-dim'}">${filtered[i].line[0] === 'err' ? '!' : ' '}</span>`, onLine: 'line' }) : empty(p)) : K.pad(K.btn('Открыть лог', { do: 'log', key: 'L' }))}`;
  }
  function narrow(p, geo) {
    if (p.state.area === 'job') return `${header(p)}${geo.height < 240 ? (pausedNote(runAt(p)) ? K.ctx('', pausedNote(runAt(p))) : '') : notices(p, runAt(p))}${jobInfo(p, true, true)}`;
    return `${header(p)}${K.scroll(`${notices(p, runAt(p))}${p.state.area === 'list' ? runList(p) : `${runInfo(p, true)}${jobList(p)}`}`)}`;
  }
  function center(p, geo) {
    if (geo.size === 'Narrow') return narrow(p, geo);
    if (p.state.area === 'list') return `${header(p)}${K.scroll(`${notices(p, runAt(p))}${runList(p, true)}`)}`;
    if (geo.size !== 'Wide' && p.state.area === 'job') return `${header(p)}${notices(p, runAt(p))}${jobInfo(p)}`;
    const graph = `${runInfo(p)}${K.scroll(`${notices(p, runAt(p))}${jobList(p, false, true)}${geo.size !== 'Wide' ? K.pad(K.btn('Открыть лог', { do: 'log', key: 'L' })) : ''}`)}`;
    return `${header(p)}${geo.size === 'Wide' ? K.cols([{ html: graph }, { w: '42%', html: jobInfo(p) }]) : graph}`;
  }
  function headOf(p, r) {
    const acts = p.state.area === 'run' ? [] : [pausedNote(r)];
    if (stateAllows(r, null, 'cancel')) acts.push(K.btn('Отменить', { do: 'cancel', small: true, disabled: muted(r) }));
    if (r.state === 'failed' && r.failed) acts.push(K.btn(`Лог ${esc(r.failed)}`, { do: 'openJob', arg: r.failed, small: true }));
    if (stateAllows(r, null, 'retry')) acts.push(K.btn('Перезапустить', { do: 'retry', small: true, disabled: muted(r) }));
    return `<div class="pl-head">${K.pstat(r.state, labels[r.state])}<span class="pl-title">Пайплайн #${r.id}</span><span class="pl-sub">${esc(r.ref)}</span><span class="pl-acts">${acts.join('')}</span></div>`;
  }
  // Нижняя зона: запуски ветки слева, выбранный запуск справа.
  function bottom(p, geo) {
    if (geo.width < 620) return narrow(p, geo);
    const wb = W.workbenches[p.wb];
    const list = runs(p).filter((r) => r.repo === wb.repo && r.ref === wb.branch);
    const r = runAt(p);
    const rows = list.map((x) => K.plRow({ k: keyOf(x), id: `#${x.id}`, state: x.state, label: labels[x.state], branch: esc(x.ref), on: x === r, do: 'selectRun', arg: keyOf(x) })).join('');
    let detail;
    if (!r) detail = `<div class="pl-detail">${empty(p)}</div>`;
    else if (p.state.area === 'job') detail = `<div class="pl-detail log">${K.ctx(`${K.iconBtn('chevL', { do: 'back', tip: 'К заданиям' })}${K.crumbs([`#${r.id}`, esc(p.state.job)])}`, `${pausedNote(r)}${actions(p)}`)}${jobInfo(p, true, true)}</div>`;
    else detail = `<div class="pl-detail scroll">${p.state.area === 'run' ? notices(p, r) : ''}${headOf(p, r)}${graphOf(p, r)}</div>`;
    return `<div class="pl"><div class="pl-list scroll" aria-label="Пайплайны ветки">${rows || '<div class="row-s" style="padding:7px 8px">Для этой ветки запусков нет.</div>'}</div>${detail}</div>`;
  }
  function move(p, delta) {
    if (p.state.area === 'job') {
      const r = runAt(p); const j = jobAt(p); if (!r || !j) return;
      const list = logOf(r, j).map((line, i) => ({ line, i })).filter(({ line }) => matches(p, line[1]));
      const index = Math.max(0, list.findIndex((x) => x.i === p.state.line));
      p.state.line = (list[S.clamp(index + delta, 0, list.length - 1)] || {}).i || 0;
    } else if (p.state.area === 'run') {
      const list = jobs(p); const index = list.findIndex((x) => x.job === jobAt(p));
      p.state.job = (list[S.clamp(index + delta, 0, list.length - 1)] || { job: {} }).job.name;
    } else {
      const list = runs(p); const index = list.indexOf(runAt(p)); chooseRun(p, list[S.clamp(index + delta, 0, list.length - 1)]);
    }
    S.render();
  }
  function reopen(p, opts = {}) {
    const id = opts.run == null ? opts.target : opts.run;
    const r = runsOf(p).find((x) => String(x.id) === String(id) && (!opts.repo || x.repo === opts.repo));
    if (r) { remember(p); chooseRun(p, r); p.state.area = opts.job ? 'job' : 'run'; p.state.job = opts.job || p.state.job; p.filter = ''; }
  }
  S.plugin({
    id: 'pipelines', title: 'Пайплайны', icon: 'pipeline', placements: ['left', 'right', 'center', 'bottom'], defaultZone: 'center', cardinality: 'workbench', minSize: { w: 230, h: 90 }, inputDefault: false,
    init(p, opts) { Object.assign(p.state, { area: 'list', run: null, job: null, line: 0 }); chooseRun(p, runs(p)[0]); reopen(p, opts); }, reopen,
    tabStatus(p) { const list = runsOf(p); return list.some((r) => r.state === 'failed') ? 'err' : list.some((r) => active(r.state)) ? 'run' : list.some((r) => r.state === 'manual') ? 'wait' : null; },
    meter: () => ({ kind: 'estimate', text: '≈ 0,1% CPU · 6,8 МБ' }),
    context(p) {
      const r = runAt(p); const j = jobAt(p); if (!r || !j) return [];
      const lines = logOf(r, j).map((line) => line[1]);
      const key = JSON.stringify([identity(r), j.name, lines]);
      const label = `Лог ${j.name}, #${r.id}/${r.attempt}, строки 1–${lines.length}`;
      const pin = { ...source(r), repo: r.repo, run: r.id, attempt: r.attempt, ref: r.ref, sha: r.sha, job: j.name, log: lines.join('\n') };
      state().pins[key] = pin;
      return [{ label, icon: 'pin', ...pin, content: pin.log }, { label: `${source(r).host}/${source(r).project}`, icon: r.provider }, { label: `Ref ${r.ref}`, icon: 'branch' }, { label: `SHA ${r.sha}`, icon: 'commit' }];
    },
    view: (p, geo) => K.adapt(geo, { left: () => narrow(p, geo), right: () => narrow(p, geo), center: () => center(p, geo), bottom: () => bottom(p, geo) }),
    actions: {
      down: { title: 'Вниз', key: ['j', 'Down'], menu: false, run: (p) => move(p, 1) },
      up: { title: 'Вверх', key: ['k', 'Up'], menu: false, run: (p) => move(p, -1) },
      enter: { title: 'Открыть выбранное', key: ['Enter', 'l'], hint: 'Открыть', when: (p) => p.state.area !== 'job', run: (p) => p.state.area === 'list' ? openRun(p) : openJob(p) },
      back: { title: 'Назад', key: 'h', when: (p) => p.state.area !== 'list', menu: false, run: back },
      log: { title: 'Открыть лог задания', key: 'L', hint: 'Лог', when: (p) => !!jobAt(p), run: (p) => openJob(p) },
      retry: { title: 'Перезапустить запуск или задание', key: 'r', hint: 'Перезапустить', when: (p) => operationAllowed(runAt(p), p.state.area === 'job' ? jobAt(p) : null, 'retry'), run: (p) => operate(p, 'retry') },
      cancel: { title: 'Отменить запуск или задание', key: 'x', hint: 'Отменить', when: (p) => operationAllowed(runAt(p), p.state.area === 'job' ? jobAt(p) : null, 'cancel'), run: (p) => operate(p, 'cancel') },
      manual: { title: 'Запустить ручной шаг', key: 'm', hint: 'Запустить', when: (p) => operationAllowed(runAt(p), jobAt(p), 'manual'), run: (p) => operate(p, 'manual') },
      refresh: { title: 'Обновить запуски', key: 'u', run: (p) => { state().offline = false; runsOf(p).forEach(schedule); S.render(); } },
      source: { title: 'Открыть запуск у провайдера', key: 'o', when: (p) => !!runAt(p), run: (p) => { const r = runAt(p); if (!r) return; const src = source(r); const path = r.provider === 'github' ? `actions/runs/${r.id}` : `-/pipelines/${r.id}`; window.open(`https://${src.host}/${src.project}/${path}`, '_blank', 'noopener,noreferrer'); } },
      clearFilter: { title: 'Сбросить фильтр', when: (p) => !!p.filter, run: (p) => { p.filter = ''; S.render(); } },
      openRun: { title: '', menu: false, run: openRun }, openJob: { title: '', menu: false, run: openJob },
      selectRun: { title: '', menu: false, run: (p, key) => { const r = runsOf(p).find((x) => keyOf(x) === key); if (!r) return; chooseRun(p, r); p.state.area = 'list'; S.render(); } },
      retryJob: { title: '', menu: false, run: (p, name) => { p.state.job = name; operate(p, 'retry', true); } },
      selectJob: { title: '', menu: false, run: (p, name) => { p.state.job = name; p.state.line = 0; S.render(); } },
      line: { title: '', menu: false, run: (p, index) => { const r = runAt(p); const j = jobAt(p); if (!r || !j) return; const lines = logOf(r, j).map((line, i) => ({ line, i })).filter(({ line }) => matches(p, line[1])); const target = lines[+index]; openJob(p, j.name); p.state.line = target ? target.i : 0; S.render(); } },
    },
  });
  function scenario(kind) {
    const r = W.pipelines.find((x) => x.id === (kind === 'manual' ? 8840 : 8841) && x.repo === 'orbit-api');
    invalidate(r); state().offline = kind === 'offline';
    if (kind !== 'offline') {
      r.attempt = 1;
      allJobs(r).forEach(({ stage, job }) => { job.state = stage === 'deploy' ? (job.name === 'release:manual' ? 'manual' : 'skipped') : kind === 'failed' && job.name === 'test:integration' ? 'failed' : 'ok'; delete state().logs[logKey(r, job)]; });
      const failed = allJobs(r).find(({ job }) => job.state === 'failed'); if (failed) failed.job.code = 101;
      recompute(r);
    }
    const wb = Object.values(W.workbenches).find((w) => W.projects[w.project].repos.includes(r.repo));
    S.open('pipelines', { wb: wb.id, run: r.id, repo: r.repo, job: kind === 'failed' ? 'test:integration' : kind === 'manual' ? 'release:manual' : undefined });
  }
  S.scenario({ id: 'pipelines.failed', group: 'Пайплайны', title: 'Запуск упал', run: () => scenario('failed') });
  S.scenario({ id: 'pipelines.manual', group: 'Пайплайны', title: 'Ручной шаг', run: () => scenario('manual') });
  S.scenario({ id: 'pipelines.offline', group: 'Пайплайны', title: 'Нет сети', run: () => scenario('offline') });
})();
