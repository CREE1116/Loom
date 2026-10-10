// Run with Node; no repository npm dependencies required. Override
// PLAYWRIGHT_MODULE_PATH, PLAYWRIGHT_BROWSER_CHANNEL or PLAYWRIGHT_EXECUTABLE_PATH.
// CI: PLAYWRIGHT_MODULE_PATH=/path/to/node_modules/playwright node --test loom-tui/tests/remote_browser.mjs
// The module path may point to an entry file, package directory or node_modules directory.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { test, before, after } from 'node:test';

const require = createRequire(import.meta.url);
const modulePath = process.env.PLAYWRIGHT_MODULE_PATH || join(homedir(),
  '.cache/codex-runtimes/codex-primary-runtime/dependencies/node/node_modules/playwright');
const moduleRoot = modulePath.startsWith('file:') ? fileURLToPath(modulePath) : resolve(modulePath);
let moduleEntry;
for (const candidate of [moduleRoot, join(moduleRoot, 'playwright'), join(moduleRoot, 'node_modules/playwright')]) {
  try { moduleEntry = require.resolve(candidate); break; } catch {}
}
if (!moduleEntry) throw new Error('Cannot resolve Playwright at PLAYWRIGHT_MODULE_PATH: ' + modulePath);
const playwright = await import(pathToFileURL(moduleEntry).href);
const { chromium } = playwright.default || playwright;
const PAGE = await readFile(new URL('../src/remote.html', import.meta.url), 'utf8');
let browser;
before(async () => {
  const options = { headless: true,
    ...(process.env.PLAYWRIGHT_EXECUTABLE_PATH
      ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH }
      : process.env.PLAYWRIGHT_BROWSER_CHANNEL ? { channel: process.env.PLAYWRIGHT_BROWSER_CHANNEL } : {}) };
  try { browser = await chromium.launch(options); }
  catch (error) {
    if (process.env.PLAYWRIGHT_EXECUTABLE_PATH || process.env.PLAYWRIGHT_BROWSER_CHANNEL ||
      !error.message.includes("Executable doesn't exist")) throw error;
    // Some bundled runtimes have the module but only a system Chrome installation.
    browser = await chromium.launch({ ...options, channel: 'chrome' });
  }
});
after(async () => { await browser?.close(); });

const baseState = () => ({ title: 'Browser fixture', status: 'Ready', busy: false,
  model: 'first', model_label: 'First model', effort: null,
  models: [{ id: 'first', label: 'First model', group: 'Fixture' },
    { id: 'second', label: 'Second model', group: 'Fixture' }],
  efforts: [{ value: 'low', description: 'Fast' }, { value: 'high', description: 'Careful' }],
  entries: [], approvals: [], questions: [], queued: 0, public: false });

async function until(predicate, message = 'condition timed out', timeout = 5000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  assert.fail(message);
}

async function fixture(t, options = {}) {
  const f = { state: baseState(), version: 1, actions: [], plans: [], polls: [],
    pollStatus: 200, pollHang: false, errors: [], receipts: new Map() };
  const json = (res, status, body) => {
    if (!res.destroyed) { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)); }
  };
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname === '/api/state') {
      f.polls.push({ client: url.searchParams.get('client'), since: url.searchParams.get('since') });
      if (f.pollHang) return;
      // Controlled long polling without spinning a busy browser loop.
      setTimeout(() => {
        if (f.pollRaw != null) {
          if (!res.destroyed) { res.writeHead(f.pollStatus, { 'Content-Type': 'text/plain' }); res.end(f.pollRaw); }
        } else json(res, f.pollStatus, f.pollStatus === 200
          ? { version: f.version, ...(Number(url.searchParams.get('since')) !== f.version ? { state: f.state } : {}) }
          : { ok: false, error: 'State unavailable' });
      }, 75);
    } else if (url.pathname === '/api/action') {
      let raw = '';
      for await (const chunk of req) raw += chunk;
      const body = JSON.parse(raw);
      f.actions.push(body);
      const previous = f.deduplicate && f.receipts.get(body.request_id);
      if (previous) {
        json(res, previous.body === raw ? 200 : 409, previous.body === raw
          ? { ok: true } : { ok: false, error: 'Request ID payload changed' });
        return;
      }
      const plan = f.plans.shift() || {};
      const respond = () => {
        if (f.deduplicate && (!plan.status || plan.status === 200) && (!plan.body || plan.body.ok === true)) {
          f.receipts.set(body.request_id, { body: raw });
        }
        // Send a partial response before dropping it: Chrome may replay an HTTP
        // request itself if a reused socket closes before any response bytes.
        if (plan.drop) {
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.write('{"ok":');
          setTimeout(() => res.destroy(), 20);
          return;
        }
        if (plan.hang) return;
        json(res, plan.status || 200, plan.body || (plan.status && plan.status !== 200
          ? { ok: false, error: plan.error || 'Rejected by fixture' } : { ok: true }));
      };
      if (plan.hold) plan.release = respond;
      else respond();
    } else if (url.pathname === '/' || url.pathname.startsWith('/t/')) {
      res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(PAGE);
    } else { res.writeHead(404); res.end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  f.origin = `http://127.0.0.1:${server.address().port}`;
  f.context = await browser.newContext({ viewport: { width: 1000, height: 900 }, ...options });
  f.page = await f.context.newPage();
  f.page.setDefaultTimeout(5000);
  f.page.on('pageerror', error => f.errors.push(error.message));
  f.update = patch => { Object.assign(f.state, patch); f.version++; };
  f.open = async () => {
    await f.page.goto(f.origin + '/t/ABC123');
    await f.page.waitForFunction(() => document.querySelector('#title').textContent === 'Browser fixture');
  };
  t.after(async () => {
    await f.context.close();
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    assert.deepEqual(f.errors, [], 'no unhandled browser exceptions');
  });
  return f;
}

test('failed send preserves newer draft and offers explicit retry', async t => {
  const f = await fixture(t);
  const plan = { status: 409, error: 'Session not ready', hold: true };
  f.plans.push(plan);
  await f.open();
  await f.page.locator('#text').fill('first message');
  await f.page.locator('#send').click();
  await until(() => f.actions.length === 1);
  await f.page.locator('#text').fill('newer draft');
  plan.release();
  await f.page.waitForFunction(() => document.querySelector('.failed-send'));
  assert.equal(await f.page.locator('#text').inputValue(), 'newer draft');
  assert.equal(await f.page.locator('.msg-user.pending').count(), 0);
  assert.match(await f.page.locator('.failed-send').textContent(), /first message/);
  assert.match(await f.page.locator('#toast').textContent(), /Session not ready/);
  assert.equal(f.actions.length, 1, 'failed POST must not retry automatically');
  await f.page.locator('.retry-send').click();
  await until(() => f.actions.length === 2);
  await f.page.waitForFunction(() => !document.querySelector('.failed-send'));
  assert.equal(f.actions[1].text, 'first message');
  assert.deepEqual(f.actions[1], f.actions[0], 'failed send body and ID remain unchanged');
  assert.equal(f.actions[1].request_id, f.actions[0].request_id, 'explicit retry reuses identity to avoid duplicates');
  assert.ok(f.actions[0].request_id);
  assert.equal(await f.page.locator('#text').inputValue(), 'newer draft');
});

test('transport ambiguity never retries POST automatically', async t => {
  const f = await fixture(t);
  f.deduplicate = true;
  f.plans.push({ drop: true });
  await f.open();
  await f.page.locator('#text').fill('ambiguous message');
  await f.page.locator('#send').click();
  await f.page.waitForFunction(() => document.querySelector('.failed-send'));
  assert.match(await f.page.locator('.failed-send').textContent(), /전송 여부/);
  await until(() => f.polls.length > 5);
  assert.equal(f.actions.length, 1);
  await f.page.locator('.retry-send').click();
  await until(() => f.actions.length === 2);
  assert.equal(f.actions[1].request_id, f.actions[0].request_id);
  assert.deepEqual(f.actions[1], f.actions[0], 'manual retry uses the exact accepted payload');
  assert.equal(f.receipts.size, 1, 'ambiguous retry is served from the acceptance receipt');
  await f.page.waitForFunction(() => !document.querySelector('.failed-send, .msg-user.pending'));
  await f.page.locator('#text').fill('ambiguous message');
  await f.page.locator('#send').click();
  await until(() => f.actions.length === 3);
  assert.notEqual(f.actions[2].request_id, f.actions[0].request_id, 'new explicit send gets a new ID even with identical text');
  assert.equal(f.receipts.size, 2);
});

test('HTTP 200 without ok:true is rejected and remains recoverable', async t => {
  const f = await fixture(t);
  f.plans.push({ body: { ok: false, error: 'Not accepted' } });
  await f.open();
  await f.page.locator('#text').fill('keep rejected text');
  await f.page.locator('#send').click();
  await f.page.waitForFunction(() => document.querySelector('.failed-send'));
  assert.match(await f.page.locator('.failed-send').textContent(), /keep rejected text.*Not accepted/s);
  assert.equal(await f.page.locator('.msg-user.pending').count(), 0);
  assert.equal(f.actions.length, 1);
});

test('queued send is acknowledged behind blocking questions and count clears when drained', async t => {
  const f = await fixture(t);
  f.state.questions = questions();
  const plan = { hold: true };
  f.plans.push(plan);
  await f.open();
  await f.page.locator('#text').fill('work after these questions');
  await f.page.locator('#send').click();
  await until(() => f.actions.length === 1);
  assert.doesNotMatch(await f.page.locator('#toast').textContent(), /접수/);
  f.update({ queued: 1 });
  await f.page.waitForFunction(() => document.querySelector('#queue-count')?.textContent.includes('1개'));
  plan.release();
  await f.page.waitForFunction(() => document.querySelector('#toast').textContent.includes('접수'));
  assert.equal(await f.page.locator('#queue-count').isVisible(), true);
  assert.match(await f.page.locator('#queue-count').textContent(), /질문/);
  assert.equal(await f.page.locator('.msg-user.pending').count(), 0);
  assert.equal(await f.page.locator('.submit-answers').isDisabled(), true, 'queued send must not answer questions implicitly');
  assert.equal(f.actions.length, 1);
  f.update({ queued: 0, questions: [], entries: [
    { id: 'delivered', kind: 'user', body: 'work after these questions', status: 'completed' },
  ] });
  await f.page.waitForFunction(() => document.querySelector('#queue-count').hidden);
  assert.match(await f.page.locator('#thread').textContent(), /work after these questions/);
});

test('failed approval re-enables choices and accepts explicit retry', async t => {
  const f = await fixture(t);
  f.state.approvals = [{ id: 'approve-1', title: 'Run command?', summary: 'Review first',
    detail: 'echo fixture', answering: false,
    choices: [{ label: 'Allow', result: 'opaque-allow' }, { label: 'Deny', result: 'opaque-deny' }] }];
  const plan = { status: 409, error: 'Approval temporarily unavailable', hold: true };
  f.plans.push(plan);
  await f.open();
  const allow = f.page.locator('#approvals button').filter({ hasText: 'Allow' });
  await allow.click();
  await until(() => f.actions.length === 1);
  assert.equal(await allow.isDisabled(), true);
  assert.equal(await f.page.locator('#approvals button').filter({ hasText: 'Deny' }).isDisabled(), true);
  f.update({ status: 'Unrelated update' });
  await f.page.waitForFunction(() => document.querySelector('#status').textContent === 'Unrelated update');
  assert.equal(await allow.isDisabled(), true, 'poll must not unlock an in-flight decision');
  plan.release();
  await f.page.waitForFunction(() => !document.querySelector('#approvals button').disabled);
  assert.match(await f.page.locator('#toast').textContent(), /Approval temporarily unavailable/);
  assert.equal(f.actions.length, 1);
  await allow.click();
  await until(() => f.actions.length === 2);
  assert.deepEqual({ ...f.actions[1], request_id: undefined },
    { action: 'reply', id: 'approve-1', choice: 'opaque-allow', request_id: undefined });
  assert.ok(f.actions.every(action => action.request_id));
});

const questions = () => [{ id: 'questions-1', blocking: true, answering: false, questions: [
  { id: 'route', header: 'Route', prompt: 'Choose a route', allow_custom: true, secret: false,
    options: [{ label: 'Direct', description: 'Go directly' }, { label: 'Scenic', description: 'Take your time' }] },
  { id: 'notes', header: 'Notes', prompt: 'Add details', options: [], allow_custom: true, secret: false },
  { id: 'key', header: 'Key', prompt: 'Private key', options: [], allow_custom: true, secret: true },
] }];

test('question answers require explicit values; polls preserve drafts, focus and secret inputs', async t => {
  const f = await fixture(t);
  f.state.questions = questions();
  await f.open();
  const submit = f.page.locator('.submit-answers');
  assert.equal(await submit.isDisabled(), true);
  assert.equal(await f.page.locator('#questions input:checked').count(), 0);
  assert.equal(await f.page.locator('#questions input[type=password]').inputValue(), '');
  await f.page.locator('#questions fieldset').nth(0).getByLabel('Scenic', { exact: false }).check();
  await f.page.getByLabel('Notes 직접 입력', { exact: true }).fill('browser draft\nsecond line');
  await f.page.getByLabel('Key 직접 입력', { exact: true }).fill('secret-from-this-browser');
  assert.equal(await submit.isDisabled(), false);
  await f.page.evaluate(() => { window.savedInput = document.querySelector('[aria-label="Notes 직접 입력"]'); window.savedInput.focus(); window.savedInput.setSelectionRange(2, 6); });
  f.update({ status: 'Streaming update', questions: structuredClone(f.state.questions) });
  await f.page.waitForFunction(() => document.querySelector('#status').textContent === 'Streaming update');
  assert.equal(await f.page.getByLabel('Notes 직접 입력', { exact: true }).inputValue(), 'browser draft\nsecond line');
  assert.equal(await f.page.getByLabel('Key 직접 입력', { exact: true }).inputValue(), 'secret-from-this-browser');
  assert.equal(await f.page.evaluate(() => window.savedInput === document.activeElement && window.savedInput.selectionStart === 2 && window.savedInput.selectionEnd === 6), true);
  assert.equal(await f.page.locator('#questions input:checked').count(), 3);
  assert.equal(await f.page.evaluate(() => [...Object.values(localStorage), ...Object.values(sessionStorage)].some(value => value.includes('secret-from-this-browser'))), false);
  const rejected = { status: 422, error: 'Question changed', hold: true };
  f.plans.push(rejected);
  await submit.click();
  await until(() => f.actions.length === 1);
  assert.equal(await submit.isDisabled(), true);
  rejected.release();
  await f.page.waitForFunction(() => !document.querySelector('.submit-answers').disabled);
  assert.equal(await f.page.getByLabel('Key 직접 입력', { exact: true }).inputValue(), 'secret-from-this-browser');
  await submit.click();
  await until(() => f.actions.length === 2);
  assert.deepEqual(f.actions[1].answers, [
    { question_id: 'route', option: 1 }, { question_id: 'notes', text: 'browser draft\nsecond line' },
    { question_id: 'key', text: 'secret-from-this-browser' },
  ]);
  assert.equal(f.actions[1].action, 'answer');
  assert.equal(f.actions[1].id, 'questions-1');
  f.update({ questions: [{ ...f.state.questions[0], answering: true }], status: 'Answering' });
  await f.page.waitForFunction(() => document.querySelector('#status').textContent === 'Answering');
  assert.equal(await f.page.locator('.cancel-questions').isDisabled(), true);
  assert.equal(await f.page.getByLabel('Key 직접 입력', { exact: true }).isDisabled(), true);
  f.update({ questions: [], status: 'Resolved' });
  await f.page.waitForFunction(() => !document.querySelector('.question-form'));
  f.update({ questions: questions(), status: 'New questions' });
  await f.page.waitForFunction(() => document.querySelector('.question-form'));
  assert.equal(await submit.isDisabled(), true);
  assert.equal(await f.page.getByLabel('Key 직접 입력', { exact: true }).inputValue(), '', 'resolved drafts must be discarded');
});

test('question cancel works without answers; rejects blank and oversized custom answers', async t => {
  const f = await fixture(t);
  f.state.questions = [{ id: 'text-only', blocking: false, answering: false, questions: [
    { id: 'text', header: 'Text', prompt: 'Explicit text', options: [], allow_custom: true, secret: false },
  ] }];
  await f.open();
  const input = f.page.getByLabel('Text 직접 입력', { exact: true });
  await input.fill('   ');
  assert.equal(await f.page.locator('.submit-answers').isDisabled(), true);
  await input.fill('가'.repeat(5462)); // 16386 UTF-8 bytes, not 5462.
  assert.equal(await f.page.locator('.submit-answers').isDisabled(), true);
  await input.fill('explicit');
  assert.equal(await f.page.locator('.submit-answers').isDisabled(), false);
  await input.fill('');
  f.plans.push({ status: 409, error: 'Cancel failed' });
  await f.page.locator('.cancel-questions').click();
  await f.page.waitForFunction(() => document.querySelector('#toast').textContent === 'Cancel failed');
  assert.equal(await f.page.locator('.cancel-questions').isDisabled(), false);
  await f.page.locator('.cancel-questions').click();
  await until(() => f.actions.length === 2);
  assert.deepEqual({ ...f.actions[1], request_id: undefined },
    { action: 'cancel_questions', id: 'text-only', request_id: undefined });
});

test('same-version reconnect restores controls and stable client survives reload', async t => {
  const f = await fixture(t);
  f.state.questions = questions();
  await f.open();
  await f.page.locator('#text').fill('offline draft');
  await f.page.getByLabel('Notes 직접 입력', { exact: true }).fill('question draft');
  const initialClient = f.polls[0].client;
  assert.ok(initialClient);
  const version = f.version;
  f.pollStatus = 503;
  await f.page.waitForFunction(() => document.querySelector('#dot').classList.contains('off'));
  assert.equal(await f.page.locator('#send').isDisabled(), true);
  assert.equal(await f.page.locator('.cancel-questions').isDisabled(), true);
  await f.page.locator('#text').press('Enter');
  assert.equal(f.actions.length, 0);
  assert.equal(await f.page.locator('#text').inputValue(), 'offline draft');
  f.pollStatus = 200;
  await f.page.waitForFunction(() => document.querySelector('#status').textContent === 'Ready' && !document.querySelector('#dot').classList.contains('off'));
  assert.equal(f.version, version);
  assert.equal(await f.page.locator('#send').isDisabled(), false);
  assert.equal(await f.page.locator('.cancel-questions').isDisabled(), false);
  assert.equal(await f.page.getByLabel('Notes 직접 입력', { exact: true }).inputValue(), 'question draft');
  await f.page.reload();
  await f.page.waitForFunction(() => document.querySelector('#title').textContent === 'Browser fixture');
  assert.ok(f.polls.every(poll => poll.client === initialClient));
});

test('model, effort, menu and stop actions honor acceptance and server errors', async t => {
  const f = await fixture(t);
  await f.open();
  f.plans.push({ status: 422, error: 'Unknown model' });
  await f.page.locator('#model-chip').click();
  await f.page.locator('#model-search').fill('Second');
  await f.page.locator('#model-list .option').click();
  await f.page.waitForFunction(() => document.querySelector('#toast').textContent === 'Unknown model');
  assert.equal(await f.page.locator('#model-sheet').evaluate(node => node.classList.contains('open')), true);
  assert.equal(await f.page.locator('#model-label').textContent(), 'First model');
  assert.equal(f.actions[0].model, 'second');
  await f.page.locator('#model-list .option').click();
  await f.page.waitForFunction(() => !document.querySelector('#model-sheet').classList.contains('open'));
  await f.page.locator('#effort-chip').click();
  await f.page.locator('#effort-list .option').filter({ hasText: 'high' }).click();
  await f.page.waitForFunction(() => !document.querySelector('#effort-sheet').classList.contains('open'));
  await f.page.locator('#effort-chip').click();
  await f.page.locator('#effort-list .option').filter({ hasText: 'runtime' }).click();
  await f.page.waitForFunction(() => !document.querySelector('#effort-sheet').classList.contains('open'));
  const compact = { status: 409, error: 'Busy: cannot compact', hold: true };
  f.plans.push(compact);
  await f.page.locator('#menu-btn').click();
  await f.page.locator('[data-action=compact]').click();
  await until(() => f.actions.length === 5);
  assert.equal(await f.page.locator('[data-action=compact]').isDisabled(), true);
  assert.doesNotMatch(await f.page.locator('#toast').textContent(), /대화를 압축합니다/);
  compact.release();
  await f.page.waitForFunction(() => document.querySelector('#toast').textContent === 'Busy: cannot compact');
  await f.page.locator('[data-action=compact]').click();
  await f.page.waitForFunction(() => !document.querySelector('#menu-sheet').classList.contains('open'));
  f.page.once('dialog', dialog => dialog.dismiss());
  await f.page.locator('#menu-btn').click();
  await f.page.locator('[data-action=new]').click();
  assert.equal(f.actions.length, 6, 'cancelled confirmation sends no command');
  f.page.once('dialog', dialog => dialog.accept());
  await f.page.locator('[data-action=new]').click();
  await f.page.waitForFunction(() => !document.querySelector('#menu-sheet').classList.contains('open'));
  f.update({ busy: true, status: 'Working' });
  await f.page.waitForFunction(() => document.querySelector('#send').getAttribute('aria-label') === '중지');
  await f.page.locator('#send').click();
  await until(() => f.actions.length === 8);
  assert.deepEqual(f.actions.map(action => action.action), ['model', 'model', 'effort', 'effort', 'compact', 'compact', 'new', 'interrupt']);
  assert.equal(f.actions[2].effort, 'high');
  assert.equal(f.actions[3].effort, '');
  assert.equal(new Set(f.actions.map(action => action.request_id)).size, 8);
});

test('phone layout contains long content and touch Enter keeps multiline draft', async t => {
  const f = await fixture(t, { viewport: { width: 320, height: 568 }, isMobile: true, hasTouch: true });
  f.state.entries = [{ id: 'code', kind: 'assistant', body: '```txt\n' + 'x'.repeat(500) + '\n```', status: 'completed' }];
  f.state.model_label = 'long-model-'.repeat(30);
  f.state.questions = questions();
  f.state.questions[0].questions[0].prompt = 'unbroken'.repeat(60);
  await f.open();
  assert.equal(await f.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  assert.equal(await f.page.locator('#requests').evaluate(node => node.scrollWidth <= node.clientWidth), true,
    'question controls must wrap instead of hiding in horizontal overflow');
  assert.equal(await f.page.locator('#send').evaluate(node => { const r = node.getBoundingClientRect(); return r.right <= innerWidth && r.bottom <= innerHeight; }), true);
  await f.page.locator('#text').fill('line one');
  await f.page.locator('#text').press('Enter');
  await f.page.locator('#text').press('l');
  assert.equal(await f.page.locator('#text').inputValue(), 'line one\nl');
  assert.equal(f.actions.length, 0);
  if (process.env.REMOTE_SCREENSHOT) await f.page.screenshot({ path: process.env.REMOTE_SCREENSHOT });
  await f.page.locator('#model-chip').click();
  assert.equal(await f.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await f.page.locator('#model-search').fill('First');
  assert.equal(await f.page.locator('#model-list .option').count(), 1);
});

test('desktop Shift+Enter and composition preserve multiline; Enter submits', async t => {
  const f = await fixture(t);
  await f.open();
  await f.page.locator('#text').fill('line one');
  await f.page.locator('#text').press('Shift+Enter');
  await f.page.locator('#text').press('l');
  await f.page.locator('#text').dispatchEvent('keydown', { key: 'Enter', isComposing: true });
  assert.equal(f.actions.length, 0);
  await f.page.locator('#text').press('Enter');
  await until(() => f.actions.length === 1);
  assert.equal(f.actions[0].text, 'line one\nl');
  await f.page.waitForFunction(() => !document.querySelector('.msg-user.pending'));
});

test('expired token disables actions and shows useful new-address instructions', async t => {
  const f = await fixture(t);
  await f.open();
  await f.page.locator('#text').fill('keep this draft');
  f.pollStatus = 401;
  f.pollRaw = 'Unauthorized';
  await f.page.waitForFunction(() => document.querySelector('#offline').classList.contains('show'));
  assert.match(await f.page.locator('#offline-text').textContent(), /\/remote.*새 QR.*새 주소/);
  assert.equal(await f.page.locator('#send').isDisabled(), true);
  assert.equal(await f.page.locator('#text').inputValue(), 'keep this draft');
  assert.equal(await f.page.locator('#reconnect').isVisible(), true);
});

test('bounded action and poll timeouts recover without automatic POST retry', { timeout: 45000 }, async t => {
  const f = await fixture(t);
  await f.open();
  f.pollHang = true;
  f.plans.push({ hang: true });
  await f.page.locator('#text').fill('timed out request');
  await f.page.locator('#send').click();
  await f.page.waitForFunction(() => document.querySelector('.failed-send'), null, { timeout: 13000 });
  assert.equal(f.actions.length, 1);
  assert.equal(await f.page.locator('.retry-send').isDisabled(), true);
  // The pending GET must also time out; once it does, the loop issues another GET.
  const initialPolls = f.polls.length;
  await until(() => f.polls.length > initialPolls, 'hung poll was not bounded at 30 seconds', 33000);
  assert.equal(f.actions.length, 1);
  f.pollHang = false;
  // Reload explicitly to abandon the second hung GET; client identity remains stable.
  await f.page.reload();
  await f.page.waitForFunction(() => document.querySelector('#status').textContent === 'Ready');
});

test('phone can recover a failed model request and control queued options', async t => {
  const f = await fixture(t, { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  f.update({ recovery: true, queued: 1, queue_paused: true, queue_revision: 'revision-a',
    queue: [{ index: 0, text: 'queued task', model: 'first', model_label: 'First model', effort: 'low' }] });
  await f.open();
  await f.page.locator('#text').fill('keep this draft');
  await f.page.getByRole('button', { name: '이어서 시도', exact: true }).click();
  await until(() => f.actions.some(a => a.action === 'retry'));
  assert.equal(await f.page.locator('#text').inputValue(), 'keep this draft');
  await f.page.getByRole('button', { name: '대기열 1개', exact: true }).click();
  await f.page.getByRole('button', { name: '옵션 변경', exact: true }).click();
  await f.page.locator('#queue-model').selectOption('second');
  await f.page.getByRole('button', { name: '옵션 저장', exact: true }).click();
  await until(() => f.actions.some(a => a.action === 'queue_update'));
  const edit = f.actions.find(a => a.action === 'queue_update');
  assert.equal(edit.model, 'second');
  assert.equal(edit.effort, '');
  assert.equal(edit.revision, 'revision-a');
  await f.page.getByRole('button', { name: '대기열 1개', exact: true }).click();
  await f.page.getByRole('button', { name: '즉시 보내기', exact: true }).click();
  await until(() => f.actions.some(a => a.action === 'queue_send'));
  assert.equal(f.actions.find(a => a.action === 'queue_send').index, 0);
  assert.equal(await f.page.locator('#text').inputValue(), 'keep this draft');
  assert.equal(await f.page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
});
