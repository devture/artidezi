#!/usr/bin/env node
// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0
// Optional browser check: Node 22+, Chromium, and a running local Compose stack.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const state = path.join(root, 'var/services');
const settings = Object.fromEntries(fs.readFileSync(path.join(state, 'env'), 'utf8').trim().split('\n').map(line => line.split('=')));
const port = Number(settings.ARTIDEZI_UI_PORT);
assert(Number.isInteger(port) && port >= 1024 && port <= 65535, 'Invalid local UI port');
// Keep browser automation and its credentials strictly on loopback.
const localApi = `http://127.0.0.1:${port}`;
const base = `http://artidezi.127.0.0.1.nip.io:${port}`;
const screenshots = path.join(root, 'var/screenshots');
fs.mkdirSync(screenshots, {recursive: true});
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'artidezi-browser-'));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const chromium = spawn(process.env.CHROMIUM || 'chromium', [
  '--headless', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu',
  '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
  '--no-proxy-server', '--host-resolver-rules=MAP artidezi.127.0.0.1.nip.io 127.0.0.1, MAP artidezi-forgejo.127.0.0.1.nip.io 127.0.0.1',
  '--remote-debugging-port=0', '--remote-debugging-address=127.0.0.1',
  `--user-data-dir=${profile}`, 'about:blank',
], {stdio: 'ignore', detached: true});
let launchError;
chromium.on('error', error => { launchError = error; });
const exited = new Promise(resolve => chromium.once('exit', resolve));
let socket;
try {
  const activePort = path.join(profile, 'DevToolsActivePort');
  for (let i = 0; !fs.existsSync(activePort); i++) {
    if (launchError) throw new Error('Chromium could not start. Set CHROMIUM to its executable path.');
    assert(i < 150 && chromium.exitCode === null, 'Chromium did not start within 15 seconds');
    await delay(100);
  }
  const debugPort = Number(fs.readFileSync(activePort, 'utf8').split('\n')[0]);
  const targets = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
  socket = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, {once: true});
    socket.addEventListener('error', reject, {once: true});
  });
  let next = 0;
  const pending = new Map();
  const browserErrors = [];
  const dialogs = [];
  socket.addEventListener('message', ({data}) => {
    const message = JSON.parse(data);
    if (message.id) {
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id);
      clearTimeout(request.timeout);
      if (message.error) request.reject(new Error('Browser command failed'));
      else request.resolve(message.result);
    }
    if (message.method === 'Page.javascriptDialogOpening') dialogs.push(message.params);
    if (message.method === 'Runtime.exceptionThrown') browserErrors.push('JavaScript exception');
    if (message.method === 'Log.entryAdded' && message.params.entry.source === 'security' && message.params.entry.level === 'error') browserErrors.push('Browser security or CSP error');
  });
  function call(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++next;
      const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`Browser command timed out: ${method}`)); }, 10000);
      pending.set(id, {resolve, reject, timeout});
      socket.send(JSON.stringify({id, method, params}));
    });
  }
  async function evaluate(expression) {
    const result = await call('Runtime.evaluate', {expression, returnByValue: true, awaitPromise: true});
    // Expressions can contain the local login password; never include them in diagnostics.
    if (result.exceptionDetails) throw new Error('Browser evaluation failed');
    return result.result.value;
  }
  async function until(expression, description) {
    for (let i = 0; i < 600; i++) {
      try { if (await evaluate(expression)) return; } catch { /* The page may be navigating. */ }
      await delay(100);
    }
    throw new Error(`Timed out waiting for ${description}`);
  }
  let authenticated = false;
  async function go(urlPath) {
    await evaluate('window.__oldDocument = true');
    await call('Page.navigate', {url: base + urlPath});
    await until(`!window.__oldDocument && location.pathname === ${JSON.stringify(new URL(urlPath, base).pathname)} && document.readyState === 'complete'`, `navigation to ${urlPath}`);
    if (authenticated) {
      assert(await evaluate("['/tasks', '/incoming-changes'].every(path => !!document.querySelector(`.app-nav a[href=\"${path}\"] .nav-count`))"), `${urlPath} keeps both navigation counts`);
    }
  }
  async function screenshot(name) {
    // Include navigation consistently in full-page screenshots.
    await evaluate('window.scrollTo(0, 0)');
    await delay(200);
    assert(!await evaluate('document.documentElement.scrollWidth > innerWidth'), `${name} overflows horizontally`);
    const smallText = await evaluate(`Array.from(document.querySelectorAll('body *')).filter(element =>
      element.checkVisibility() && ([...element.childNodes].some(node => node.nodeType === Node.TEXT_NODE && node.textContent.trim()) || element.matches('input, textarea, select')) &&
      parseFloat(getComputedStyle(element).fontSize) < 16
    ).map(element => element.tagName + '.' + element.className)`);
    assert.deepEqual(smallText, [], `${name} contains text smaller than 16px`);
    if (await evaluate("!!document.querySelector('.app-nav')")) {
      assert(await evaluate("document.querySelectorAll('.app-nav .nav-link').length === 5 && [...document.querySelectorAll('.app-nav .nav-link')].every(link => getComputedStyle(link).whiteSpace === 'nowrap' && link.getBoundingClientRect().height >= 44) && document.querySelector('.signout button').checkVisibility()"), `${name} keeps navigation labels together and sign-out accessible`);
    }
    const result = await call('Page.captureScreenshot', {format: 'png', captureBeyondViewport: true});
    fs.writeFileSync(path.join(screenshots, `${name}.png`), Buffer.from(result.data, 'base64'));
  }
  async function create(instructions, agentProfile) {
    await go('/tasks/new?project=playground');
    await evaluate(`document.querySelector('#instructions').value = ${JSON.stringify(instructions)};
      document.querySelector('#instructions').dispatchEvent(new Event('input', {bubbles: true}));
      document.querySelector('#agent_profile').value = ${JSON.stringify(agentProfile)};
      document.querySelector('#create-task button[type=submit]').click();`);
    await until("(/^[/]tasks[/][0-9]+$/).test(location.pathname) && !!document.querySelector('#live-content')", 'task creation');
    return evaluate("'/tasks/' + document.querySelector('#task-overview').dataset.taskId");
  }
  async function checkGrowingTextarea(selector) {
    const original = await evaluate(`document.querySelector(${JSON.stringify(selector)}).value`);
    async function fill(text) {
      return evaluate(`(() => { const input = document.querySelector(${JSON.stringify(selector)});
        input.value = ${JSON.stringify(text)}; input.dispatchEvent(new Event('input', {bubbles:true}));
        return {height: input.getBoundingClientRect().height, scroll: input.scrollHeight, client: input.clientHeight}; })()`);
    }
    const small = await fill('Short input.');
    assert(small.height <= 140, 'Short textareas should stay compact');
    const large = await fill('A line of instructions that should remain visible.\n'.repeat(24));
    assert(large.height > small.height + 300 && large.scroll <= large.client + 1, 'Long textareas grow without internal scrolling');
    assert.equal((await fill('Short again.')).height, small.height, 'Removing content shrinks the textarea');
    await fill(original);
  }
  await call('Page.enable');
  await call('Runtime.enable');
  await call('Log.enable');
  await call('Emulation.setDeviceMetricsOverride', {width: 1440, height: 1050, deviceScaleFactor: 1, mobile: false});
  await go('/login');
  const assets = await evaluate(`Promise.all([
    document.querySelector('link[rel=stylesheet]').href, document.querySelector('script[src]').src,
    document.querySelector('link[rel=icon]').href
  ].map(async url => { const response = await fetch(url); return {
    path: new URL(url).pathname, cache: response.headers.get('cache-control'), body: await response.text()
  }; }))`);
  for (const asset of assets) {
    const extension = asset.path.split('.').at(-1);
    assert.equal(asset.path, '/assets/app.' + createHash('sha256').update(asset.body).digest('hex') + '.' + extension, 'Asset URL follows the actual embedded content');
    assert.equal(asset.cache, 'public, max-age=31536000, immutable', 'Content-addressed assets can be cached across page visits');
  }
  await screenshot('login-desktop');
  const password = fs.readFileSync(path.join(state, 'secrets/ui-password'), 'utf8').trim();
  await evaluate(`document.querySelector('#username').value = 'local-admin'; document.querySelector('#password').value = ${JSON.stringify(password)}; document.querySelector('button[type=submit]').click();`);
  await until("location.pathname === '/tasks' && !!document.querySelector('.task-board')", 'browser login');
  authenticated = true;
  await go('/account');
  assert.equal(await evaluate("document.querySelectorAll('.password-change input[type=password]').length"), 3, 'Account page has current, new and confirmation password fields');
  assert(await evaluate("document.body.textContent.includes('local-admin')"), 'Account page shows the named principal');
  assert(await evaluate("!!document.querySelector('#account-timezone option[value=\"Europe/Sofia\"]')"), 'Account offers IANA time zones');
  assert(await evaluate("!!document.querySelector('#account-timezone').previousElementSibling.matches('details.select-picker')"), 'Account time zone uses the searchable select');
  await evaluate("document.querySelector('#account-timezone').previousElementSibling.querySelector('summary').click()");
  await evaluate("(() => { const filterInput = document.querySelector('#account-timezone').previousElementSibling.querySelector('input[type=search]'); filterInput.value = 'Europe/Sofia'; filterInput.dispatchEvent(new Event('input')); })()");
  assert.equal(await evaluate("[...document.querySelector('#account-timezone').previousElementSibling.querySelectorAll('.picker-options button')].filter(button => !button.hidden).length"), 1, 'Time zone search narrows the list');
  await evaluate("document.querySelector('#account-timezone').previousElementSibling.open = false");
  assert(await evaluate("!document.querySelector('#browser-timezone').disabled"), 'Browser zone shortcut is ready');
  assert(await evaluate("Math.abs(document.querySelector('#account-timezone').previousElementSibling.getBoundingClientRect().top - document.querySelector('#browser-timezone').getBoundingClientRect().top) < 2"), 'Time zone selector and browser shortcut share a row');
  await screenshot('account-desktop');
  await go('/tasks');
  await screenshot('tasks-desktop');
  await go('/tasks?filter=done');
  const usageProject = await evaluate("document.querySelector('.task-row:has(.task-meta .usage) .task-project')?.getAttribute('href')");
  if (usageProject) {
    await go(usageProject);
    assert(await evaluate("[...document.querySelectorAll('.task-meta .usage')].every(pill => pill.title.includes('across recorded attempts'))"), 'Task rows label aggregate token counts');
    await screenshot('tasks-usage-desktop');
    await go('/tasks');
  }
  assert(await evaluate("document.querySelector('.task-board').getBoundingClientRect().top < 350"), 'The task list should start near the top');
  assert.equal(await evaluate("document.querySelectorAll('.filter-count').length"), 7, 'Each task filter needs a counter');
  assert.equal(await evaluate("document.querySelectorAll('.filter-tab > .icon').length"), 7, 'Each task filter has a status icon');
  assert(await evaluate("document.querySelector('.filter-tab[value=waiting]').textContent.includes('Waiting for CI')"), 'CI waiting has its own filter');
  const beforeHover = await evaluate("JSON.stringify(document.querySelector('.filter-tab').getBoundingClientRect().toJSON())");
  const hover = await evaluate("({x: document.querySelector('.filter-tab').getBoundingClientRect().x + 15, y: document.querySelector('.filter-tab').getBoundingClientRect().y + 15})");
  await call('Input.dispatchMouseEvent', {type: 'mouseMoved', ...hover});
  await delay(250);
  assert.equal(await evaluate("JSON.stringify(document.querySelector('.filter-tab').getBoundingClientRect().toJSON())"), beforeHover, 'Hover must not move controls');
  assert(await evaluate("[...document.querySelectorAll('.filter-count')].every(counter => (Number(counter.textContent) > 0) === counter.classList.contains('nonzero'))"), 'Nonzero counts must stand out');
  // Active styling must not change the width/position of any status tab.
  const tabGeometry = await evaluate("JSON.stringify([...document.querySelectorAll('.filter-tab')].map(button => { const r=button.getBoundingClientRect(); return [r.x,r.y,r.width,r.height]; }))");
  for (const filter of ['active', 'review', 'attention', 'done', 'closed', '']) {
    await evaluate(`document.querySelector('.filter-tab[value="${filter}"]').click()`);
    await until(`new URL(location.href).searchParams.get('filter') === ${JSON.stringify(filter)}`, 'stable filter selection');
    assert.equal(await evaluate("JSON.stringify([...document.querySelectorAll('.filter-tab')].map(button => { const r=button.getBoundingClientRect(); return [r.x,r.y,r.width,r.height]; }))"), tabGeometry, 'Switching tabs must not shift buttons');
    assert(await evaluate("[...document.querySelectorAll('.filter-tab')].every(button => getComputedStyle(button).borderLeftWidth === '1px' && getComputedStyle(button).borderRightWidth === '1px')"), 'Every tab reserves its border');
  }
  await go('/tasks/new?project=playground');
  assert(await evaluate("document.querySelector('#project-description').textContent.includes('scripted')"), 'Explain the scripted worker before task creation');
  assert.equal(await evaluate("document.querySelectorAll('#create-task > .form-card').length"), 3, 'Project, instructions and execution settings have separate cards');
  assert(await evaluate("Math.abs(document.querySelector('#create-task').getBoundingClientRect().right - (document.querySelector('#main').getBoundingClientRect().right - parseFloat(getComputedStyle(document.querySelector('#main')).paddingRight))) < 2"), 'New-task form uses the full main pane');
  assert(await evaluate("document.querySelector('.form-actions button').getBoundingClientRect().left - document.querySelector('.form-actions a').getBoundingClientRect().right > 300"), 'Cancel and Create task are at opposite ends');
  assert(await evaluate("document.querySelectorAll('input[name=delivery_mode]').length === 3 && document.querySelectorAll('input[name=delivery_mode]:disabled').length === 2 && document.querySelector('input[name=delivery_mode]:checked').value === 'merge_request' && [...document.querySelectorAll('.delivery-choice')].every(label => label.title) && !document.querySelector('#delivery-note')"), 'Delivery choices show supported modes while preserving project policy');
  await checkGrowingTextarea('#instructions');
  await screenshot('create-desktop');
  const reviewPath = await create('Prepare a maintenance note for browser review.\n\n## Acceptance checks\n\n**Preserve compatibility** and record the checks.\n\n' + Array.from({length: 24}, (_, index) => `- Requirement ${index + 1}: keep existing defaults.`).join('\n'), 'demo');
  await until("document.querySelector('#task-overview .awaiting_review')", 'proposal and automatic updates');
  assert(await evaluate("!!document.querySelector('#task-instructions .markdown h2') && !!document.querySelector('#task-instructions .markdown strong') && !!document.querySelector('#task-instructions .markdown ul')"), 'Instructions render Markdown headings, emphasis and lists');
  await screenshot('proposal-desktop');
  assert(await evaluate("!!document.querySelector('.breadcrumb a[href=\"/tasks\"]') && /^Task #\\d+$/.test(document.querySelector('.breadcrumb [aria-current]').textContent) && !document.querySelector('.back-link, .detail-heading .eyebrow, .private-label')"), 'Task navigation fits into one useful breadcrumb');
  assert(await evaluate("!document.querySelector('#task-overview .execution-facts') && !!document.querySelector('.attempt > .attempt-metadata .execution-facts') && document.querySelectorAll('.attempt .usage[title]').length === 3"), 'Worker, runtime and token use belong to the attempt card');
  assert(await evaluate("(() => { const title = document.querySelector('.attempt-heading h2'); const subsection = document.querySelector('.attempt-section h3'); return parseFloat(getComputedStyle(title).fontSize) > parseFloat(getComputedStyle(subsection).fontSize) && ['#task-instructions', '#activity', '.revision-panel'].every(selector => { const h = document.querySelector(selector + ' > .card-header h2'); return h && getComputedStyle(h).fontSize === getComputedStyle(title).fontSize; }); })()"), 'Card titles are consistently more prominent than attempt subsections');

  assert(await evaluate("!!document.querySelector('.log-link.stdout') && !!document.querySelector('.log-link.stderr.empty') && !document.querySelector('details.log-links') && [...document.querySelectorAll('.log-link')].every(link => link.getBoundingClientRect().height >= 44)"), 'Logs are directly reachable, counted and comfortably sized');
  const taskNumber = await evaluate("document.querySelector('.breadcrumb [aria-current]').textContent.split('#')[1]");
  assert(await evaluate("document.querySelector('.breadcrumb a[href=\"/tasks?project=playground\"]').textContent === 'playground'"), 'Breadcrumbs link directly to the project task list');
  const canonicalID = await evaluate("document.querySelector('#task-overview').dataset.taskId");
  await go('/tasks/' + taskNumber);
  assert.equal(await evaluate("document.querySelector('#task-overview').dataset.taskId"), canonicalID, 'Numeric and old task links resolve to the same task');
  await evaluate("document.querySelector('.agent-link').click()");
  await until("location.pathname === '/agent-profiles' && !!document.querySelector('.agent-card:target')", 'linked agent profile');
  await screenshot('agent-profiles-desktop');
  assert(await evaluate("document.querySelector('.agent-card:target').textContent.includes('Worker pool')"), 'Profile details explain the worker settings');
  await go(reviewPath);

  assert(await evaluate("!!document.querySelector('.detail-main #task-instructions h2') && !document.querySelector('.detail-aside')"), 'Task instructions belong in the main pane');
  assert(await evaluate("!!document.querySelector('.detail-main #feedback')"), 'Feedback belongs in the main pane');
  assert(await evaluate("['inline-flex', 'flex'].includes(getComputedStyle(document.querySelector('.status')).display) && !!document.querySelector('#task-overview .status[title] .icon') && !document.querySelector('.state-banner')"), 'Status is a badge');
  assert(await evaluate("!!document.querySelector('.proposal') && !document.querySelector('#feedback').disabled"), 'Proposal and revision controls should appear without reloading');
  assert(await evaluate("document.querySelector('#toggle-instructions').checkVisibility() && document.querySelector('#toggle-instructions').getBoundingClientRect().height >= 44 && document.querySelector('#instructions-content').hidden && !document.querySelector('#instructions-preview').hidden && !document.querySelector('main details')"), 'Long instructions have an accessible inline expansion button');
  await evaluate("window.__instructionPath = location.pathname; window.__instructions = document.querySelector('#task-instructions'); window.__activity = document.querySelector('#activity'); document.querySelector('#toggle-instructions').click()");
  assert(await evaluate("location.pathname === window.__instructionPath && document.querySelector('#toggle-instructions').getAttribute('aria-expanded') === 'true' && document.querySelector('#instructions-content').checkVisibility() && document.querySelector('#instructions-content').textContent.includes('Requirement 24:') && document.querySelector('#instructions-preview').hidden"), 'The full Markdown expands without leaving the task');
  await until("document.querySelector('#activity') !== window.__activity", 'activity polling while instructions are expanded');
  assert(await evaluate("document.querySelector('#task-instructions') === window.__instructions && document.querySelector('#toggle-instructions').getAttribute('aria-expanded') === 'true' && !document.querySelector('#instructions-content').hidden && document.querySelector('#activity') !== window.__activity"), 'Normal polling preserves expanded instructions while updating activity');
  await screenshot('instructions-desktop');
  await evaluate("document.querySelector('#toggle-instructions').focus()");
  await call('Input.dispatchKeyEvent', {type: 'keyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, text: '\r', unmodifiedText: '\r'});
  await call('Input.dispatchKeyEvent', {type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13});
  assert(await evaluate("document.querySelector('#toggle-instructions').getAttribute('aria-expanded') === 'false' && document.querySelector('#instructions-content').hidden && !document.querySelector('#instructions-preview').hidden"), 'Keyboard activation collapses instructions back to the preview');
  await call('Emulation.setScriptExecutionDisabled', {value: true});
  await go(reviewPath);
  assert(await evaluate("document.querySelector('#instructions-content').checkVisibility() && document.querySelector('#instructions-content').textContent.includes('Requirement 24:') && document.querySelector('#instructions-preview').hidden && document.querySelector('#toggle-instructions').hidden"), 'Without JavaScript, full instructions are visible and the inactive expansion control is hidden');
  await screenshot('instructions-no-javascript');
  await call('Emulation.setScriptExecutionDisabled', {value: false});
  await go(reviewPath);
  const patchPath = await evaluate("document.querySelector('.log-link.patch').getAttribute('href')");
  await evaluate("document.querySelector('.log-link.patch').click()");
  await until("location.pathname.includes('/patches/') && !!document.querySelector('.diff-table')", 'formatted patch');
  assert(await evaluate("!!document.querySelector('.diff-added') && !!document.querySelector('.diff-number') && [...document.querySelectorAll('main a')].some(link => link.textContent.includes('Raw patch'))"), 'Patch viewer shows additions, line numbers and the original');
  assert(await evaluate("(async () => { const link = [...document.querySelectorAll('main a')].find(link => link.textContent.includes('Raw patch')); const response = await fetch(link.href); return response.ok && response.headers.get('content-type').startsWith('text/plain') && (await response.text()).includes('diff --git'); })()"), 'Raw patch remains available as inert text');
  assert(await evaluate(`document.querySelector('.breadcrumb a[href*="?attempt=1#attempt-"]').textContent === 'Attempt #1' && document.querySelectorAll('.breadcrumb a').length === 4 && document.querySelector('.breadcrumb [aria-current]').textContent.endsWith('.patch') && !document.querySelector('main').textContent.includes('Back to attempt')`), 'Patch breadcrumbs include the project, task, exact attempt and filename');
  await screenshot('patch-desktop');
  await go(reviewPath);
  await checkGrowingTextarea('#feedback');
  // Polling must preserve a draft, even when the user moves focus elsewhere.
  await evaluate("document.querySelector('#feedback').value = 'Keep this draft'; document.querySelector('#feedback').dispatchEvent(new Event('input', {bubbles:true})); document.querySelector('#feedback').blur();");
  await delay(5500);
  assert.equal(await evaluate("document.querySelector('#feedback').value"), 'Keep this draft');
  const needsPath = await create('Clarify compatibility constraints before making a change.', 'demo-needs-input');
  await until("document.body.textContent.includes('Which compatibility constraints') && !document.querySelector('#feedback').disabled", 'question and enabled revision controls');
  assert(await evaluate("document.querySelector('#activity').tagName === 'SECTION' && !document.querySelector('main details')"), 'Activity is permanently visible');
  await evaluate("window.__activity = document.querySelector('#activity')");
  await delay(5500);
  assert(await evaluate("document.querySelector('#activity') !== window.__activity && document.querySelector('#activity h2').textContent === 'Activity'"), 'Visible activity keeps receiving updates');
  await evaluate("document.querySelector('.event-link').click()");
  await until("location.pathname.includes('/events/') && !!document.querySelector('.event-payload')", 'full event details');
  await screenshot('event-desktop');
  await go(needsPath);
  await screenshot('detail-desktop');
  assert(await evaluate("!!document.querySelector('.event-attention') && !!document.querySelector('.event-progress')"), 'Activity should distinguish progress and decisions');
  // A real revision retains earlier results through ordinary, linkable attempt navigation.
  await evaluate("document.querySelector('#feedback').value = 'Preserve the current interface and existing defaults.'; document.querySelector('#task-actions form').requestSubmit();");
  await until("document.querySelectorAll('.attempt-navigation a').length === 2 && document.querySelector('.attempt-heading h2').textContent === 'Attempt 2'", 'second attempt');
  await until("!document.querySelector('#feedback').disabled && !!document.querySelector('.attempt-state.valid')", 'revised attempt completion');
  assert(await evaluate("document.querySelectorAll('.attempt').length === 1 && !document.querySelector('main details')"), 'Only the selected result is shown, with no accordion');
  await evaluate("document.querySelector('.attempt-navigation a:last-child').click()");
  await until("new URL(location.href).searchParams.get('attempt') === '1' && document.querySelector('.attempt-heading h2').textContent === 'Attempt 1'", 'earlier attempt');
  assert(await evaluate("!document.querySelector('#feedback') && !!document.querySelector('.attempt > .attempt-metadata .execution-facts')"), 'Earlier results show their own execution details and cannot accidentally receive feedback');
  await delay(5500);
  assert(await evaluate("document.querySelector('.attempt-heading h2').textContent === 'Attempt 1'"), 'Polling preserves selected attempt');
  await screenshot('earlier-attempt-desktop');
  await go(needsPath);
  const failedPath = await create('Exercise the failed verification result in the browser.', 'demo-failing-check');
  await until("!!document.querySelector('.check-row.failed') && !document.querySelector('#feedback').disabled", 'failed check and revision controls');
  assert(await evaluate("!document.querySelector('.proposal')"), 'A failed check cannot have a published proposal');
  assert(await evaluate("!!document.querySelector('.check-card.failed .check-command code') && !!document.querySelector('.check-card.failed .check-logs .stdout.empty') && !!document.querySelector('.check-card.failed .check-logs .stderr.empty') && document.querySelector('.check-card.failed').textContent.includes('Exit code 1') && !!document.querySelector('.check-card.failed .empty-output') && document.querySelector('.attempt-checks .section-note').textContent.includes('Run by the worker')"), 'Failed checks explain their origin, command, exit and genuinely empty output together');
  assert(await evaluate("!!document.querySelector('.attempt-patches .log-link.patch') && !document.querySelector('.attempt-patches .stdout') && !!document.querySelector('.agent-logs .stdout') && !document.querySelector('.agent-logs .patch') && !document.querySelector('.attempts-panel') && document.querySelector('.attempt-heading h2').textContent === 'Attempt 1'"), 'Patches, agent logs and check output are separate, within the attempt card');
  assert(await evaluate("[...document.querySelectorAll('button')].some(button => button.textContent === 'Retry evaluation' && button.form.querySelector('input[name=feedback]'))"), 'Rejected execution offers a retry without typing feedback');
  await screenshot('failed-check-desktop');
  await go('/projects');
  await screenshot('projects-desktop');
  await go('/workers');
  await until("document.querySelectorAll('.individual-worker').length > 0", 'individual worker presence');
  assert(await evaluate("document.querySelector('.individual-worker').textContent.includes('Last contact') && document.querySelector('.individual-worker').textContent.includes('Current work') && !!document.querySelector('.individual-worker form[action=\"/workers/drain\"]')"), 'Workers show supervisor contact, current work and drain controls');
  await evaluate("document.querySelector('form[action=\"/workers/maintenance\"]').requestSubmit()");
  await until("document.querySelector('form[action=\"/workers/maintenance\"] button').textContent.includes('Resume')", 'automatic scheduling paused');
  await evaluate("document.querySelector('form[action=\"/workers/maintenance\"]').requestSubmit()");
  await until("document.querySelector('form[action=\"/workers/maintenance\"] button').textContent.includes('Pause')", 'automatic scheduling resumed');
  await screenshot('workers-desktop');
  await evaluate("document.querySelector('.individual-worker form').requestSubmit()");
  await until("document.querySelector('.individual-worker button').textContent === 'Resume assignments'", 'worker drained');
  assert(await evaluate("document.querySelector('.individual-worker .status').textContent.startsWith('Drain')"), 'Drain state is visible');
  await evaluate("document.querySelector('.individual-worker form').requestSubmit()");
  await until("document.querySelector('.individual-worker button').textContent === 'Drain worker'", 'worker resumed');
  await call('Emulation.setDeviceMetricsOverride', {width: 375, height: 812, deviceScaleFactor: 1, mobile: true});
  assert(await evaluate("document.documentElement.scrollWidth <= innerWidth"), 'Worker cards fit mobile viewport');
  await screenshot('workers-mobile');
  await call('Emulation.clearDeviceMetricsOverride');
  await go('/projects');

  assert(await evaluate("[...document.querySelectorAll('.project-heading .button')].every(button => button.classList.contains('primary'))"), 'Project creation actions are primary');
  assert(await evaluate("!!document.querySelector('.repository-count.single') && (!document.querySelector('.repository-count.multiple') || getComputedStyle(document.querySelector('.repository-count.single')).backgroundColor !== getComputedStyle(document.querySelector('.repository-count.multiple')).backgroundColor)"), 'Configured single and multiple repository counts have distinct badges');
  assert(await evaluate("[...document.querySelectorAll('.delivery')].every(badge => badge.title) && new Set([...document.querySelectorAll('.delivery')].map(badge => getComputedStyle(badge).backgroundColor)).size === new Set([...document.querySelectorAll('.delivery')].map(badge => badge.className)).size"), 'Delivery modes have explanatory, distinct badges');
  assert(await evaluate("!document.querySelector('main details:not(.project-guidance)') && !document.querySelector('.workspace, .private-label') && [...document.querySelectorAll('.repository-link')].every(link => ['127.0.0.1', 'artidezi-forgejo.127.0.0.1.nip.io'].includes(new URL(link.href).hostname))"), 'Projects expose repositories as browser-facing links without unrelated disclosures or redundant workspace badges');
  assert(await evaluate("[...document.querySelectorAll('.repository-link')].every(link => link.querySelector('.forge-icon') && link.querySelector('.external-icon')) && [...document.querySelectorAll('.project-tasks, .discovery-link, .project-meta .agent-link')].every(link => link.querySelector('.icon'))"), 'Project links identify forge repositories, task lists, incoming changes and agent profiles consistently');
  const discoveryPath = await evaluate("document.querySelector('.discovery-link')?.getAttribute('href')");
  if (discoveryPath) {
    await go(discoveryPath);
    assert(await evaluate("document.querySelector('.filter-tab[value=\"\"]').getAttribute('aria-pressed') === 'true' && !document.querySelector('.incoming-controls .delivery') && !!document.querySelector('.scan-incoming .primary') && !document.querySelector('main details:not(.project-picker)')"), 'Incoming changes default to open items with a primary scan action and no global mode claim');
    await screenshot('incoming-changes-desktop');
    assert(await evaluate("[...document.querySelectorAll('.discovery-source .source-end')].every(end => end.lastElementChild.matches('.status') && (!end.querySelector('.source-actions') || end.lastElementChild.getBoundingClientRect().right >= end.querySelector('.source-actions').getBoundingClientRect().right))"), 'Incoming status stays to the right of task and evaluation actions');
    assert(await evaluate("[...document.querySelectorAll('.discovery-source a[href^=\"/tasks/\"]')].every(link => /^Task #\\d+$/.test(link.textContent.trim()) && !!link.querySelector('.icon'))"), 'Assessment actions use Task numbers and the Tasks icon');
    await evaluate("document.querySelector('.filter-tab[value=history]').click()");
    await until("new URL(location.href).searchParams.get('filter') === 'history'", 'closed PR history');
    await screenshot('incoming-history-desktop');
    const selectedProject = new URL(base + discoveryPath).searchParams.get('project');
    assert.equal(await evaluate("new URL(location.href).searchParams.get('project')"), selectedProject, 'Incoming status filters preserve project scope');
    await go('/incoming-changes');
    assert(await evaluate("!!document.querySelector('.nav-link.selected[href=\"/incoming-changes\"]') && document.querySelector('#project-filter').value === '' && [...document.querySelectorAll('.discovery-source')].every(row => row.querySelector('.delivery.shadow[title]')?.textContent.trim() === 'Dry run')"), 'Incoming changes is a main navigation item with all projects and clear dry-run mode');
    assert(await evaluate("[...document.querySelectorAll('.discovery-source')].every(row => { const repo = row.querySelector('a.source-repository'); const number = row.querySelector('a.source-number'); const title = row.querySelector('a.source-title'); return repo?.querySelector('.forge-icon') && number && title && number.href === title.href && repo.href !== number.href; }) && [...document.querySelectorAll('.source-actions form[action$=pause] button')].every(button => button.classList.contains('warning'))"), 'Incoming rows link repository, PR number and title separately');
    await screenshot('incoming-all-desktop');
    await evaluate(`window.__scanFetch = window.fetch;
    window.__scanPromise = new Promise(resolve => { window.__resolveScan = resolve; });
    window.fetch = async (...args) => {
      if (new URL(args[0], location.href).pathname === '/incoming-changes/scan') {
        return window.__scanPromise;
      }
      return window.__scanFetch(...args);
    }; document.querySelector('.scan-incoming').requestSubmit();`);
    assert(await evaluate("location.pathname === '/incoming-changes' && document.querySelector('.scan-incoming button').disabled && document.querySelector('#scan-status').textContent.includes('Scanning all projects') && !document.body.classList.contains('busy')"), 'Scan shows progress without blocking navigation');
    await screenshot('incoming-scan-progress');
    await evaluate("document.querySelector('.filter-tab[value=history]').click()");
    await until("new URL(location.href).searchParams.get('filter') === 'history' && document.querySelector('.scan-incoming button').disabled", 'filter while scan runs');
    await evaluate("window.__resolveScan(new Response(JSON.stringify({queued: ['first', 'second'], errors: []}), {status: 200, headers: {'Content-Type': 'application/json'}}))");
    await until("document.querySelector('#scan-status').textContent.includes('2 evaluations queued') && !document.querySelector('.scan-incoming button').disabled", 'scan completion');
    await screenshot('incoming-scan-complete');
    assert(await evaluate("location.pathname === '/incoming-changes' && new URL(location.href).searchParams.get('filter') === 'history' && document.querySelector('.scan-incoming button').textContent.includes('Scan now')"), 'Scan completion preserves view and restores button');
    await evaluate("window.fetch = window.__scanFetch");
    await go('/incoming-changes');
    await evaluate(`document.querySelector('#project-filter').value = ${JSON.stringify(selectedProject)}; document.querySelector('#project-filter').dispatchEvent(new Event('change', {bubbles:true}));`);
    await until(`new URL(location.href).searchParams.get('project') === ${JSON.stringify(selectedProject)} && !document.querySelector('#live-content').hasAttribute('aria-busy')`, 'incoming project filter');
    assert(await evaluate(`[...document.querySelectorAll('.source-project')].every(link => link.textContent.trim() === ${JSON.stringify(selectedProject)}) && document.querySelector('.scan-incoming input[name=project]').value === ${JSON.stringify(selectedProject)}`), 'Incoming rows and scan scope follow the project picker');
    await evaluate("document.querySelector('.filter-tab[value=history]').click()");
    await until("new URL(location.href).searchParams.get('filter') === 'history'", 'filtered incoming history');
    assert.equal(await evaluate("document.querySelector('.scan-incoming input[name=filter]').value"), 'history');
    await call('Emulation.setDeviceMetricsOverride', {width:320,height:950,deviceScaleFactor:1,mobile:true});
    await go('/incoming-changes?project=' + encodeURIComponent(selectedProject));
    assert(await evaluate("!!document.querySelector('.discovery-source')"), 'Mobile incoming preview has an open change');
    assert(await evaluate("[...document.querySelectorAll('.discovery-source .source-end')].every(end => end.lastElementChild.matches('.status') && Math.abs(end.lastElementChild.getBoundingClientRect().right - end.getBoundingClientRect().right) < 2)"), 'Mobile incoming status remains at the right edge');
    await screenshot('incoming-mobile');
    await call('Emulation.setDeviceMetricsOverride', {width:1440,height:1050,deviceScaleFactor:1,mobile:false});

    await go('/projects');
  }
  const projectCount = await evaluate("document.querySelectorAll('.project-card').length");
  async function typeSearch(query) {
    await evaluate(`document.querySelector('.list-search input[name=q]').value = ${JSON.stringify(query)}; document.querySelector('.list-search input[name=q]').dispatchEvent(new Event('input', {bubbles: true}));`);
    await until(`new URL(location.href).searchParams.get('q') === ${JSON.stringify(query)} && !document.querySelector('#live-content').hasAttribute('aria-busy')`, 'live search');
  }
  await typeSearch('PLAYGROUND');
  assert(await evaluate("[...document.querySelectorAll('.project-card h2')].some(heading => heading.textContent === 'playground')"), 'Project search is case insensitive');
  await typeSearch('missing-project-browser-fixture');
  assert.equal(await evaluate("document.querySelectorAll('.project-card').length"), 0, 'Unmatched search has no projects');
  await typeSearch('');
  assert.equal(await evaluate("document.querySelectorAll('.project-card').length"), projectCount, 'Clearing search restores all projects');
  // A slow older query must never replace newer search results.
  await evaluate(`window.__originalFetch = window.fetch; window.fetch = async (...args) => {
    const result = await window.__originalFetch(...args);
    if (String(args[0]).includes('q=slow-stale-fixture')) await new Promise(resolve => setTimeout(resolve, 1200));
    return result;
  }; document.querySelector('.list-search input[name=q]').value = 'slow-stale-fixture'; document.querySelector('.list-search').requestSubmit();`);
  await typeSearch('PLAYGROUND');
  await delay(1500);
  assert(await evaluate("new URL(location.href).searchParams.get('q') === 'PLAYGROUND' && document.querySelectorAll('.project-card').length > 0"), 'A stale response cannot overwrite the latest search');
  await evaluate("window.fetch = window.__originalFetch");
  // Project cards open exact project filters; typing in the picker only narrows its options.
  await go('/projects');
  await evaluate("document.querySelector('.project-card:last-child .project-tasks').click()");
  await until("location.pathname === '/tasks' && document.querySelector('#project-filter')?.value === 'playground'", 'project task link');
  assert(await evaluate("document.querySelector('.filter-tab[value=\"\"]').getAttribute('aria-pressed') === 'true'"), 'Project task links start with all statuses');
  await evaluate("document.querySelector('.project-picker summary').click()");
  await until("document.activeElement.matches('.picker-popup input')", 'picker keyboard focus');
  await evaluate("document.querySelector('.picker-popup input').value = 'adguard'; document.querySelector('.picker-popup input').dispatchEvent(new Event('input'))");
  assert(await evaluate("[...document.querySelectorAll('.picker-options button')].filter(button => !button.hidden).length === 2"), 'The project picker searches names');
  await evaluate("document.querySelector('.picker-popup input').dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))");
  assert(await evaluate("!document.querySelector('.project-picker').open && document.activeElement.matches('.project-picker summary')"), 'Escape dismisses the picker and restores focus');
  await evaluate("document.querySelector('.project-picker summary').click()");
  await until("document.activeElement.matches('.picker-popup input')", 'picker reopened');
  await evaluate("document.querySelector('.picker-options button[data-value=\"\"]').click()");
  await until("new URL(location.href).searchParams.get('project') === ''", 'all projects restored');
  await evaluate("document.querySelector('#project-filter').value = 'playground'; document.querySelector('#project-filter').dispatchEvent(new Event('change'))");
  await until("new URL(location.href).searchParams.get('project') === 'playground'", 'project selected');
  await typeSearch('browser');
  const matchingCount = await evaluate("document.querySelector('.filter-count').textContent");
  await evaluate("document.querySelector('.filter-tab[value=review]').click()");
  await until("new URL(location.href).searchParams.get('filter') === 'review'", 'task filter');
  assert.equal(await evaluate("document.querySelector('.list-search input[name=q]').value"), 'browser', 'Switching filters preserves search');
  assert.equal(await evaluate("new URL(location.href).searchParams.get('project')"), 'playground', 'Switching status preserves the exact project');
  assert.equal(await evaluate("document.querySelector('.filter-count').textContent"), matchingCount, 'Counts describe the searched task set');
  const reviewCount = await evaluate("Number(document.querySelector('.filter-tab[value=review] .filter-count').textContent)");
  assert.equal(await evaluate("document.querySelectorAll('.task-row').length"), reviewCount, 'Task counter matches rendered results');
  await screenshot('tasks-filtered-desktop');
  // If local real-model experiments exist, check their record-only UI without starting more work.
  const adminToken = fs.readFileSync(path.join(state, 'secrets/admin-token'), 'utf8').trim();
  async function admin(urlPath) {
    const response = await fetch(localApi + '/admin/v1' + urlPath, {headers: {Authorization: `Bearer ${adminToken}`}});
    assert(response.ok, 'Local admin request failed');
    return response.json();
  }
  const completedSources = [];
  for (const source of await admin('/discovery')) {
    if (!source.last_task_id) continue;
    const detail = await admin('/tasks/' + source.last_task_id);
    if (detail.task.state === 'done' && detail.task.delivery_mode === 'shadow') completedSources.push(source);
  }
  for (const [index, source] of completedSources.entries()) {
    await go('/tasks/' + source.last_task_id);
    assert(await evaluate("document.body.textContent.includes('Evaluation recorded') && !document.querySelector('.proposal') && !!document.querySelector('#feedback') && !document.querySelector('#feedback').disabled"), 'Dry-run tasks need source-aware completion and follow-up controls');
    assert(await evaluate(`Array.from(document.links).some(link => link.href === ${JSON.stringify(source.url)})`), 'Shadow task should link its source PR');
    assert(await evaluate("!!document.querySelector('.detail-main #task-instructions') && !document.querySelector('#task-instructions details')"), 'Source instructions belong in the main pane');
    assert(await evaluate("!!document.querySelector('a.task-origin .icon') && document.querySelector('a.task-origin').textContent.trim() === 'Incoming change'"), 'Automatically created tasks show their origin');
    await screenshot(`assessment-${index + 1}-desktop`);
    await evaluate("document.querySelector('a.task-origin').click()");
    await until("location.pathname === '/incoming-changes' && !!document.querySelector(location.hash)", 'task origin links to its tracking row');
    assert(await evaluate(`document.querySelector(location.hash).textContent.includes(${JSON.stringify(source.remote + ' #' + source.number)})`), 'Task origin finds its source, including closed history');
    await go('/tasks?project=' + encodeURIComponent(source.project));
    assert(await evaluate(`!!document.querySelector('.task-row a.task-link[href="/tasks/${(await admin('/tasks/' + source.last_task_id)).task.number}"]')?.closest('.task-row')?.querySelector('span.task-origin .icon')`), 'Task lists attribute discovered tasks without nesting links');
  }
  const inspectTask = process.env.ARTIDEZI_BROWSER_TASK_ID;
  if (inspectTask) {
    assert(/^tsk_[a-z0-9]+$/.test(inspectTask), 'Invalid local task ID');
    await go('/tasks/' + inspectTask);
    await screenshot('existing-task-desktop');
  }
  await call('Emulation.setDeviceMetricsOverride', {width: 390, height: 844, deviceScaleFactor: 1, mobile: true});
  await go('/account');
  await screenshot('account-mobile');
  assert(await evaluate("document.querySelector('#browser-timezone').getBoundingClientRect().top > document.querySelector('#account-timezone').previousElementSibling.getBoundingClientRect().top"), 'Time zone controls stack on mobile');
  if (inspectTask) {
    await go('/tasks/' + inspectTask);
    await screenshot('existing-task-mobile');
  }
  for (const [index, source] of completedSources.entries()) {
    await go('/tasks/' + source.last_task_id);
    await screenshot(`assessment-${index + 1}-mobile`);
  }
  await go('/agent-profiles');
  await screenshot('agent-profiles-mobile');
  await go('/projects');
  await screenshot('projects-mobile');
  await go('/tasks');
  await screenshot('tasks-mobile');
  assert(await evaluate("!!document.querySelector('.task-usage .usage[title^=\"Total input across recorded attempts: 0\"]') && !!document.querySelector('.task-usage .usage[title^=\"Total output across recorded attempts: 0\"]')"), 'Scripted tasks show input and output zero-token badges');
  if (usageProject) {
    await go(usageProject);
    await screenshot('tasks-usage-mobile');
    await go('/tasks');
  }
  assert(await evaluate("document.querySelector('.signout').getBoundingClientRect().height > 0"), 'Mobile sign-out must be available');
  await go('/tasks/new?project=playground');
  await screenshot('create-mobile');
  await go(reviewPath);
  await screenshot('detail-mobile');
  await evaluate("document.querySelector('#toggle-instructions').click()");
  await screenshot('instructions-mobile');
  await go(failedPath);
  await screenshot('failed-check-mobile');
  // Check narrow mobile and tablet layouts using existing records, without more tasks.
  for (const width of [320, 768, 1024, 1920, 2560]) {
    await call('Emulation.setDeviceMetricsOverride', {width, height: 900, deviceScaleFactor: 1, mobile: width < 720});
    await go('/tasks');
    await screenshot(`tasks-${width}`);
    const headerHeight = await evaluate("document.querySelector('.topbar').getBoundingClientRect().height");
    assert(headerHeight >= 80, 'Every page retains its header');
    if (width >= 1920) {
      assert(await evaluate("Math.abs(document.querySelector('.task-board').getBoundingClientRect().right - (document.querySelector('#main').getBoundingClientRect().right - parseFloat(getComputedStyle(document.querySelector('#main')).paddingRight))) < 2"), 'Task list uses available width on large screens');
      assert(await evaluate("Math.abs(document.querySelector('.list-search').getBoundingClientRect().right - document.querySelector('.task-board').getBoundingClientRect().right) < 2"), 'Narrow search aligns with the right edge');
    }
    await go(needsPath);
    await screenshot(`detail-${width}`);
    if (width >= 768) assert.equal(await evaluate("document.querySelector('.topbar').getBoundingClientRect().height"), headerHeight, 'Root and inner headers have consistent height');
    if (width >= 1920) assert(await evaluate("Math.abs(document.querySelector('.instructions-panel').getBoundingClientRect().right - (document.querySelector('#main').getBoundingClientRect().right - parseFloat(getComputedStyle(document.querySelector('#main')).paddingRight))) < 2 && document.querySelector('#task-overview').getBoundingClientRect().bottom <= document.querySelector('.instructions-panel').getBoundingClientRect().top"), 'Task details use a top strip and the main pane fills the width');
    for (const [name, route] of [['create', '/tasks/new?project=playground'], ['profiles', '/agent-profiles'], ['projects', '/projects'], ['patch', patchPath]]) {
      await go(route);
      await screenshot(`${name}-${width}`);
      if (name === 'create') {
        await checkGrowingTextarea('#instructions');
        if (width >= 768) assert(await evaluate("Math.abs(document.querySelector('#create-task').getBoundingClientRect().right - (document.querySelector('#main').getBoundingClientRect().right - parseFloat(getComputedStyle(document.querySelector('#main')).paddingRight))) < 2"), 'New-task form also fills wide screens');
      }
    }
  }
  await call('Emulation.setDeviceMetricsOverride', {width: 390, height: 844, deviceScaleFactor: 1, mobile: true});
  // Exercise the actual confirmation dialog in both directions, using only this run's task.
  await go(failedPath);
  assert(await evaluate("document.querySelector('.close-task button').checkVisibility() && !document.querySelector('.close-task').closest('details')"), 'Close is directly available');
  async function confirmClose(accept) {
    const before = dialogs.length;
    const clicking = evaluate("document.querySelector('.close-task button').click()");
    for (let i = 0; dialogs.length === before; i++) {
      assert(i < 50, 'Close must open a confirmation dialog');
      await delay(100);
    }
    assert.equal(dialogs.at(-1).type, 'confirm');
    assert(dialogs.at(-1).message.includes('Existing PRs and history will be kept'));
    await call('Page.handleJavaScriptDialog', {accept});
    await clicking;
  }
  await confirmClose(false);
  assert.equal((await admin(failedPath)).task.state, 'needs_human', 'Cancelling close leaves the task unchanged');
  await confirmClose(true);
  await until("!!document.querySelector('#task-overview .status.closed') && !document.querySelector('.close-task')", 'confirmed task closure');
  assert.equal((await admin(failedPath)).task.state, 'closed');
  assert(!await evaluate("!!document.querySelector('main a[href=\"/tasks/new\"]')"), 'Task details do not offer an unrelated New task action');
  // Counts should refresh even while a filter keeps keyboard focus. Close only this run's
  // review fixture; existing task history and its local PR remain available.
  await go('/tasks?filter=review');
  await evaluate("document.querySelector('.filter-tab[value=review]').focus()");
  const beforeClose = await evaluate("Number(document.querySelector('.filter-tab[value=review] .filter-count').textContent)");
  const closeResponse = await fetch(localApi + '/admin/v1' + reviewPath + '/close', {method: 'POST', headers: {Authorization: `Bearer ${adminToken}`, 'Content-Type': 'application/json'}, body: JSON.stringify({actor: 'browser-test'})});
  assert(closeResponse.ok, 'Close only this run’s browser fixture');
  await until(`Number(document.querySelector('.filter-tab[value=review] .filter-count').textContent) === ${beforeClose - 1}`, 'live counts while filter focused');
  assert(await evaluate("document.activeElement.matches('.filter-tab[value=review]')"), 'Refreshing counts preserves keyboard focus');
  await evaluate("document.querySelector('.signout button').click()");
  await until("location.pathname === '/login' && !!document.querySelector('#password')", 'mobile logout');
  await screenshot('login-mobile');
  assert.deepEqual(browserErrors, [], 'No JavaScript or CSP errors expected');
  console.log('PASS: browser login, self-service account page, hashed asset caching, task/revision/failure flows, live search and counters, stale search protection, inline instructions and no-JavaScript fallback, draft and attempt-selection preservation, stable status tabs, searchable project filters, numeric task links, profile navigation, direct logs, visible activity and evidence pages, close confirmation, 16px minimum text and responsive layouts through 2560px');
  console.log(`PASS: ${completedSources.length} existing shadow assessments checked without submitting work`);
  console.log(`Screenshots: ${screenshots}`);
} finally {
  socket?.close();
  if (chromium.pid) {
    try { process.kill(-chromium.pid, 'SIGTERM'); } catch { /* Already exited. */ }
    await Promise.race([exited, delay(2000)]);
    try { process.kill(-chromium.pid, 'SIGKILL'); } catch { /* Already exited. */ }
  }
  fs.rmSync(profile, {recursive: true, force: true, maxRetries: 5, retryDelay: 100});
}
