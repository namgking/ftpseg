// Minimal DOM simulation exercises lifecycle, cache safety and bounded async batches.
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const html = fs.readFileSync('src/gui.html', 'utf8');
const source = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const KEY = 'ftpseg.queue.display.v1';
assert.match(html, /id="batchActions"[^>]*\bhidden\b/, 'action bar is hidden before JavaScript initializes');
assert.match(html, /\.batch-actions\{[^}]*flex-wrap:wrap/, 'batch actions wrap on narrow screens');
function harness(cache, storageThrows = false) {
  const elements = new Map(), nodes = [], storage = new Map(cache === undefined ? [] : [[KEY, cache]]);
  function make() {
    const classes = new Set();
    const e = { hidden: false, textContent: '', innerHTML: '', children: [], dataset: {}, style: {}, disabled: false, checked: false,
      classList: { add: c => classes.add(c), remove: c => classes.delete(c), contains: c => classes.has(c), toggle(c, force) { const on = force === undefined ? !classes.has(c) : force; if (on) classes.add(c); else classes.delete(c); return on; } },
      replaceChildren(...children) { this.children = children; }, append(...children) { this.children.push(...children); },
      setAttribute(k, v) { this[k] = v; }, querySelector(s) { if (!this.queries) this.queries = new Map(); if (!this.queries.has(s)) this.queries.set(s, make()); return this.queries.get(s); },
      close() { this.open = false; }, showModal() { this.open = true; }, reset() {},
    }; nodes.push(e); return e;
  }
  const element = s => { if (!elements.has(s)) elements.set(s, make()); return elements.get(s); };
  const tabs = ['all', 'running', 'queued', 'paused', 'done', 'failed', 'history'].map(filter => { const e = make(); e.dataset.filter = filter; return e; });
  function descendants(e) { return e.children.flatMap(c => [c, ...descendants(c)]); }
  const document = { title: '', hidden: false, querySelector: element, createElement: make,
    querySelectorAll(s) {
      if (s === '[data-filter]') return tabs;
      if (s === 'dialog[open]') return nodes.filter(n => n.open);
      if (s.startsWith('.jobCheck')) return descendants(element('#jobs')).filter(n => n.className === 'jobCheck');
      if (s.startsWith('.entryCheck')) return descendants(element('#entries')).filter(n => n.className === 'entryCheck' && (!s.includes(':checked') || n.checked) && (!s.includes(':disabled') || !n.disabled));
      return [];
    } };
  let backend = { instance_id: 'A', sites: [], settings: {}, jobs: [] }, fetchHook = null, confirms = 0;
  const requests = [];
  const context = vm.createContext({ document, console, AbortController, performance,
    setTimeout: () => 0, clearTimeout() {}, setInterval: () => 0,
    confirm() { confirms++; return true; },
    localStorage: { getItem(k) { if (storageThrows) throw Error('blocked'); return storage.get(k) ?? null; }, setItem(k, v) { if (storageThrows) throw Error('quota'); storage.set(k, v); }, removeItem(k) { if (storageThrows) throw Error('blocked'); storage.delete(k); } },
    fetch: async (url, options) => { requests.push({ url, options }); if (fetchHook) return fetchHook(url, options); return { ok: true, json: async () => backend }; },
  });
  const run = s => vm.runInContext(s, context);
  new vm.Script(source).runInContext(context);
  return { run, element, tabs, document, storage, requests, get confirms() { return confirms; }, setBackend(v) { backend = v; }, setFetch(v) { fetchHook = v; } };
}
const job = (id, status = 'running') => ({ id, site_id: 7, remote: '/file' + id, local: 'D:/private/file' + id, total: 100, downloaded: 20, status });
const cached = (instance, jobs, history = []) => JSON.stringify({ version: 1, instance_id: instance, jobs, history });
const tick = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  const h = harness(); await tick();
  h.run('showClosed()');
  assert.equal(h.element('.shell').hidden, true);
  assert.equal(h.element('#closedView').hidden, false);
  assert.equal(h.document.title, 'ftpseg · 应用已关闭');
  assert.match(h.element('#closedMessage').textContent, /安全关闭此页面/);
  h.run('showClosed("服务连接已断开")');
  assert.equal(h.element('#closedMessage').textContent, '服务连接已断开');
  h.run('showConnected()');
  assert.equal(h.element('.shell').hidden, false);
  assert.equal(h.element('#closedView').hidden, true);
  assert.equal(h.run('appClosed'), false);
  h.setBackend({ instance_id: 'A', sites: [], settings: {}, jobs: [job(1, 'paused')] });
  await h.run('refresh(true)');
  assert.equal(h.run('eligible(state.jobs[0],"resume")'), true);

  const corrupt = harness('{bad json');
  assert.equal(corrupt.run('cacheEnabled'), false);
  await tick(); assert.equal(corrupt.run('liveVerified'), true);
  const blocked = harness(undefined, true); await tick();
  assert.equal(blocked.run('cacheEnabled'), false);
  assert.equal(blocked.run('liveVerified'), true);

  const c = harness(cached('old', [{ ...job(1, 'done'), site_name: 'safe', password: 'secret', user: 'account' }, job(2)]));
  assert.equal(c.run('state.jobs.length'), 2, 'cache displays synchronously before state response');
  assert.equal(c.run('eligible(state.jobs[1],"pause")'), false, 'cached active job is unverified');
  assert.equal(c.element('#jobSelectAllLabel').hidden, true);
  assert.equal(c.element('#batchActions').hidden, true);
  assert.equal(c.run('"password" in state.jobs[0]'), false);
  c.setBackend({ instance_id: 'new', sites: [{ id: 7, name: 'safe', user: 'account', password: 'secret' }], settings: {}, jobs: [job(1, 'running')] });
  await tick(); await c.run('refresh()');
  assert.equal(c.run('historyJobs.length'), 2);
  assert.equal(c.run('eligible(historyJobs[0],"cancel")'), false, 'colliding old id is never actionable');
  assert.equal(c.run('eligible(state.jobs[0],"pause")'), true);
  c.run('jobFilter="history";renderJobs(false)');
  assert.ok(c.element('#jobs').children.every(row => row.children[0].children[0].disabled));
  assert.equal(c.element('#jobSelectAllLabel').hidden, true);
  assert.equal(c.element('#batchActions').hidden, true);
  const saved = c.storage.get(KEY);
  assert.ok(!saved.includes('password') && !saved.includes('secret') && !saved.includes('account'));
  assert.ok(saved.includes('D:/private'), 'path privacy warning is required because paths are cached');
  c.element('#clearHistory').onclick(); assert.equal(c.run('historyJobs.length'), 0);
  c.setBackend({ instance_id: 'new', sites: [], settings: {}, jobs: [] }); await c.run('refresh()');
  assert.equal(c.run('state.jobs.length'), 0, 'same instance backend is authoritative');

  const cap = harness(cached('old', Array.from({ length: 1005 }, (_, i) => job(i, 'done'))));
  assert.equal(cap.run('state.jobs.length'), 1005, 'live/display cache is not truncated');
  await tick(); assert.equal(cap.run('historyJobs.length'), 1000);

  const b = harness(); await tick();
  b.setBackend({ instance_id: 'A', sites: [], settings: {}, jobs: ['queued','running','paused','pausing','cancelling','done','failed','cancelled'].map((s,i) => job(i+1,s)) });
  await b.run('refresh()');
  assert.equal(b.run('state.jobs.map(j=>eligible(j,"pause")).join(",")'), 'true,true,false,false,false,false,false,false');
  assert.equal(b.run('state.jobs.map(j=>eligible(j,"resume")).join(",")'), 'false,false,true,false,false,false,false,false');
  assert.match(b.element('#queueSummary').textContent, /3 活跃/);
  assert.equal(b.element('#batchActions').hidden, true, 'default action bar is hidden');
  assert.equal(b.element('#jobSelectAllLabel').hidden, false, 'quiet select-all entry remains discoverable');
  b.tabs.find(t => t.dataset.filter === 'paused').onclick();
  b.element('#jobSelectAll').onchange({ target: { checked: true } });
  assert.equal(b.run('[...jobSelection].join(",")'), '3');
  assert.equal(b.element('#batchActions').hidden, false);
  assert.equal(b.element('#batchPause').hidden, true);
  assert.equal(b.element('#batchResume').textContent, '继续 (1)');
  assert.equal(b.element('#jobs').children[0].classList.contains('selected'), true);
  b.element('#clearJobSelection').onclick();
  assert.equal(b.element('#batchActions').hidden, true, 'clear selection hides action bar');
  b.element('#jobSelectAll').onchange({ target: { checked: true } });
  b.tabs.find(t => t.dataset.filter === 'running').onclick();
  assert.equal(b.run('jobSelection.size'), 0, 'changing category clears selection');
  b.element('#jobSelectAll').onchange({ target: { checked: true } });
  assert.equal(b.run('[...jobSelection].sort().join(",")'), '2', 'select all applies only to actionable current classification');
  await b.run('refresh()'); assert.equal(b.run('[...jobSelection].join(",")'), '2', 'poll preserves still-actionable visible selection');
  b.tabs.find(t => t.dataset.filter === 'done').onclick();
  assert.equal(b.element('#jobSelectAllLabel').hidden, true, 'done remains non-selectable');
  assert.equal(b.element('#batchActions').hidden, true);
  b.tabs.find(t => t.dataset.filter === 'all').onclick();
  b.run('entries=[{name:"remote.txt",is_dir:false,size:20}];renderEntries()');
  b.element('#selectAll').onchange({ target: { checked: true } });
  assert.equal(b.document.querySelectorAll('.entryCheck')[0].checked, true, 'remote select-all remains independent');
  assert.equal(b.run('jobSelection.size'), 0);
  const firstCheck = b.document.querySelectorAll('.jobCheck').find(check => check.dataset.id === '2');
  firstCheck.checked = true; firstCheck.onchange();
  assert.equal(b.element('#batchActions').hidden, false, 'individual checkbox reveals action bar');
  assert.equal(firstCheck.row.classList.contains('selected'), true);

  await b.run('runJobActions("pause",state.jobs)');
  assert.equal(b.requests.filter(r => r.url === '/api/jobs/pause').length, 2);
  await b.run('runJobActions("resume",state.jobs)');
  assert.equal(b.requests.filter(r => r.url === '/api/jobs/resume').length, 1);
  assert.equal(b.confirms, 0, 'pause/resume preserve files without destructive confirmation');
  b.setBackend({ instance_id: 'B', sites: [], settings: {}, jobs: [job(1)] });
  await b.run('refresh()');
  b.run('jobSelection.clear()');
  assert.equal(b.run('eligible(historyJobs[0],"pause")'), false);
  assert.equal(b.run('eligible(state.jobs[0],"pause")'), true);
  // Four workers, per-item failures, one confirmation, duplicate suppression, final refresh.
  const many = Array.from({ length: 9 }, (_, i) => job(i+1));
  b.setBackend({ instance_id: 'A', sites: [], settings: {}, jobs: [...many, job(99, 'done')] });
  await b.run('refresh()'); b.run('jobFilter="all";renderJobs(false)');
  b.element('#jobSelectAll').onchange({ target: { checked: true } });
  let inflight = 0, max = 0, stateReads = 0; const releases = [];
  b.setFetch(async (url, options) => {
    if (url === '/api/state') { stateReads++; return { ok: true, json: async () => ({ instance_id: 'A', sites: [], settings: {}, jobs: many }) }; }
    inflight++; max = Math.max(max, inflight); const id = JSON.parse(options.body).id;
    await new Promise(resolve => releases.push(resolve)); inflight--;
    return { ok: id !== 2, status: 500, json: async () => id === 2 ? { error: 'individual failure' } : {} };
  });
  const promise = b.run('runJobActions("cancel",state.jobs)');
  assert.equal(inflight, 4); assert.equal(b.confirms, 1);
  assert.equal(b.element('#batchActions').hidden, false, 'busy keeps selected action bar visible');
  assert.equal(b.element('#clearJobSelection').disabled, true);
  assert.equal(b.element('#batchStatus').hidden, false);
  b.element('#clearJobSelection').onclick();
  assert.equal(b.run('jobSelection.size'), 9, 'busy clear cannot discard selection');
  b.tabs.find(t => t.dataset.filter === 'done').onclick();
  assert.equal(b.run('jobFilter'), 'all', 'busy prevents category switch');
  b.setBackend({ instance_id: 'A', sites: [], settings: {}, jobs: many.map(j => ({ ...j, status: 'cancelling' })) });
  b.run('syncState({instance_id:"A",sites:[],settings:{},jobs:state.jobs.map(j=>({...j,status:"cancelling"}))});renderJobs(false)');
  assert.equal(b.element('#batchActions').hidden, false, 'in-flight state transition does not flash away toolbar');
  assert.equal(b.run('jobSelection.size'), 9);
  b.run('syncState({instance_id:"A",sites:[],settings:{},jobs:state.jobs.map(j=>({...j,status:"running"}))})');
  await b.run('runJobActions("cancel",state.jobs)'); assert.equal(inflight, 4);
  while (b.run('batchBusy')) { const ready = releases.splice(0); ready.forEach(resolve => resolve()); await tick(); }
  await promise;
  assert.equal(max, 4); assert.equal(stateReads, 1); assert.equal(b.confirms, 1);
  assert.match(b.element('#toast').textContent, /成功 8，跳过 1，失败 1/);
  assert.match(b.element('#toast').textContent, /individual failure/);
  assert.equal(b.requests.filter(r => r.url === '/api/jobs/cancel').length, 9);
  assert.equal(b.run('[...jobSelection].join(",")'), '2', 'successful selection is cleared; failed task stays selected for retry');
  assert.equal(b.element('#batchActions').hidden, false);
  assert.equal(b.element('#batchStatus').hidden, true);
  assert.equal(b.element('#clearJobSelection').disabled, false);
  b.setFetch(null); b.setBackend({ instance_id: 'A', sites: [], settings: {}, jobs: many });
  await b.run('runJobActions("pause",selectedJobs())');
  assert.equal(b.run('jobSelection.size'), 0);
  assert.equal(b.element('#batchActions').hidden, true, 'all-success batch hides action bar');
  console.log('GUI lifecycle, cache isolation, pause eligibility and bounded batch tests passed');
})().catch(e => { console.error(e); process.exitCode = 1; });
