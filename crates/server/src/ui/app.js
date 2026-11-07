// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0
'use strict';

const appShell = document.querySelector('.app-shell');
const browserTimezone = document.querySelector('#browser-timezone');
const detectedTimezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
if (browserTimezone && detectedTimezone && [...document.querySelector('#account-timezone').options].some(option => option.value === detectedTimezone)) {
  browserTimezone.disabled = false;
  browserTimezone.title = `Use browser time zone (${detectedTimezone})`;
  browserTimezone.querySelector('span').textContent = browserTimezone.title;
  browserTimezone.addEventListener('click', () => {
    const field = document.querySelector('#account-timezone');
    field.value = detectedTimezone;
    field.dispatchEvent(new Event('change', {bubbles: true}));
  });
}

// Existing accounts keep UTC after migration. Only newly prepared accounts set their
// browser zone automatically, and a later account edit remains authoritative.
if (appShell?.dataset.timezoneInitialized === 'false' && detectedTimezone) {
  const csrf = document.querySelector('.signout input[name=csrf]')?.value;
  if (csrf) {
    fetch('/account/timezone', {
      method: 'POST',
      credentials: 'same-origin',
      headers: {'Content-Type': 'application/x-www-form-urlencoded'},
      body: new URLSearchParams({csrf, timezone: detectedTimezone}),
    }).then(response => { if (response.ok) location.reload(); }).catch(() => {});
  }
}

function updateRelativeTimes() {
  const zone = appShell?.dataset.timezone || 'UTC';
  const absolute = new Intl.DateTimeFormat('en-US', {
    timeZone: zone, month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
    hour12: false, timeZoneName: 'short',
  });
  const relative = new Intl.RelativeTimeFormat('en-US', {numeric: 'always'});
  const now = Date.now();
  for (const element of document.querySelectorAll('time[data-relative-time]')) {
    const when = Date.parse(element.dateTime);
    if (!Number.isFinite(when)) continue;
    const seconds = Math.round((now - when) / 1000);
    if (seconds < -60) element.textContent = relative.format(Math.ceil(-seconds / 60), 'minute');
    else if (seconds < 60) element.textContent = 'just now';
    else if (seconds < 3600) element.textContent = relative.format(-Math.floor(seconds / 60), 'minute');
    else if (seconds < 86400) element.textContent = relative.format(-Math.floor(seconds / 3600), 'hour');
    else if (seconds < 604800) element.textContent = relative.format(-Math.floor(seconds / 86400), 'day');
    else element.textContent = absolute.format(new Date(when));
  }
}
updateRelativeTimes();
setInterval(updateRelativeTimes, 60_000);

const passwordToggle = document.querySelector('.password-toggle');
passwordToggle?.addEventListener('click', () => {
  const input = document.querySelector('#password');
  const show = input.type === 'password';
  input.type = show ? 'text' : 'password';
  passwordToggle.textContent = show ? 'Hide' : 'Show';
  passwordToggle.setAttribute('aria-label', show ? 'Hide password' : 'Show password');
});

document.addEventListener('keydown', (event) => {
  if (event.key === '/' && !event.ctrlKey && !event.metaKey && !['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName)) {
    const search = document.querySelector('.list-search input[name=q]');
    if (search) { event.preventDefault(); search.focus(); }
  }
});

// Full Markdown remains visible without JavaScript. Instructions are outside polling regions,
// so expanding them never fetches another page or loses the reader's choice on an update.
const instructionToggle = document.querySelector('#toggle-instructions');
const instructionPreview = document.querySelector('#instructions-preview');
const instructionContent = document.querySelector('#instructions-content');
if (instructionToggle && instructionPreview && instructionContent) {
  function showInstructions(expanded) {
    instructionContent.hidden = !expanded;
    instructionPreview.hidden = expanded;
    instructionToggle.setAttribute('aria-expanded', String(expanded));
    instructionToggle.textContent = expanded ? 'Show less' : 'Read full instructions';
  }
  instructionToggle.addEventListener('click', () => {
    showInstructions(instructionToggle.getAttribute('aria-expanded') !== 'true');
  });
  showInstructions(location.hash === '#instructions-content');
  instructionToggle.hidden = false;
}

let dirty = !!document.querySelector('#feedback')?.value;
document.addEventListener('input', (event) => {
  if (event.target.matches('textarea')) dirty = true;
  if (event.target.id === 'instructions') {
    document.querySelector('#character-count').textContent = `${[...event.target.value].length.toLocaleString()} / 32,000`;
  }
});

document.addEventListener('submit', (event) => {
  const form = event.target;
  if (form.dataset.confirm) {
    if (!window.confirm(form.dataset.confirm)) { event.preventDefault(); return; }
    // The server renders a confirmation page when this enhancement is unavailable.
    let confirmed = form.querySelector('input[name=confirmed]');
    if (!confirmed) {
      confirmed = document.createElement('input');
      confirmed.type = 'hidden';
      confirmed.name = 'confirmed';
      form.append(confirmed);
    }
    confirmed.value = 'true';
  }
  if (form.method === 'post' && !form.matches('.scan-incoming')) {
    const button = event.submitter;
    if (button) { button.disabled = true; button.textContent = 'Working…'; }
    document.body.classList.add('busy');
  }
});
window.addEventListener('pageshow', () => {
  document.body.classList.remove('busy');
  document.querySelectorAll('button').forEach(button => { if (button.textContent === 'Working…') window.location.reload(); });
});

const project = document.querySelector('#project');
const profiles = document.querySelector('#agent_profile');
const deliveryInputs = [...document.querySelectorAll('input[name=delivery_mode]')];
function projectOptions() {
  return [...document.querySelectorAll('#project-options > div')].find(row => row.dataset.project === project?.value);
}
project?.addEventListener('change', () => {
  const config = projectOptions();
  profiles.replaceChildren(...[...config.querySelectorAll('[data-profile]')].map(row => new Option(row.dataset.profile, row.dataset.profile, false, row.dataset.profile === config.dataset.defaultProfile)));
  const allowed = [...config.querySelectorAll('[data-mode]')].map(row => row.dataset.mode);
  for (const input of deliveryInputs) {
    input.disabled = !allowed.includes(input.value);
    input.checked = !input.disabled && input.value === config.dataset.defaultMode;
    input.closest('label').title = input.dataset.explanation + (input.disabled ? ' Not enabled for this project.' : '');
  }
  document.querySelector('#project-description').textContent = config.dataset.description;
});

// Keep short inputs compact, including restored drafts and textareas replaced by polling.
function sizeTextarea(input) {
  if (!input.checkVisibility()) return;
  input.classList.add('auto-grow');
  const scrollY = window.scrollY;
  input.style.height = 'auto';
  const border = input.offsetHeight - input.clientHeight;
  input.style.height = `${input.scrollHeight + border}px`;
  if (window.scrollY !== scrollY) window.scrollTo(window.scrollX, scrollY);
}
function sizeTextareas() { document.querySelectorAll('textarea').forEach(sizeTextarea); }
document.addEventListener('input', event => { if (event.target.matches('textarea')) sizeTextarea(event.target); });
window.addEventListener('resize', sizeTextareas);
window.addEventListener('pageshow', sizeTextareas);
sizeTextareas();

// Search uses ordinary GET forms without JavaScript. Enhanced search keeps the input focused,
// updates the URL, and discards stale responses when typing or polling overlaps a request.
let requestVersion = 0;
let searchTimer;
let searching = false;
let scanning = false;
const scanButtonContent = document.querySelector('.scan-incoming button')?.innerHTML;
const searchForm = document.querySelector('.list-search');
const searchInput = searchForm?.querySelector('input[name=q]');
const projectFilter = document.querySelector('#project-filter');
async function fetchPage(url) {
  const response = await fetch(url, {headers: {'Accept': 'text/html'}});
  if (new URL(response.url).pathname === '/login') { window.location.assign('/login'); return null; }
  if (!response.ok) throw new Error('Page request failed');
  return new DOMParser().parseFromString(await response.text(), 'text/html');
}
async function search(url, focusFilter = false) {
  const version = ++requestVersion;
  searching = true;
  document.querySelector('#live-content')?.setAttribute('aria-busy', 'true');
  try {
    const page = await fetchPage(url);
    if (!page || version !== requestVersion) return;
    const content = page.querySelector('#live-content');
    if (!content) throw new Error('Missing search results');
    document.querySelector('#live-content').replaceWith(content);
    syncScanButton();
    updateRelativeTimes();
    history.replaceState(null, '', url);
    searchForm.querySelector('[name=filter]').value = new URL(url).searchParams.get('filter') || '';
    document.querySelector('#search-error').hidden = true;
    if (focusFilter) document.querySelector('.filter-tab.active')?.focus({preventScroll: true});
  } catch {
    if (version === requestVersion) document.querySelector('#search-error').hidden = false;
  } finally {
    if (version === requestVersion) {
      searching = false;
      document.querySelector('#live-content')?.removeAttribute('aria-busy');
    }
  }
}
function searchUrl(form, submitter) {
  const url = new URL(form.action);
  url.search = new URLSearchParams(new FormData(form, submitter));
  // The filter form may still reflect the last response while a new query is being typed.
  url.searchParams.set('q', searchInput.value);
  if (projectFilter) url.searchParams.set('project', projectFilter.value);
  url.searchParams.delete('page');
  return url;
}
searchInput?.addEventListener('input', () => {
  clearTimeout(searchTimer);
  ++requestVersion;
  searching = true;
  searchTimer = setTimeout(() => search(searchUrl(searchForm)), 250);
});
document.addEventListener('submit', event => {
  if (!event.target.matches('.list-search, #task-filters')) return;
  event.preventDefault();
  clearTimeout(searchTimer);
  search(searchUrl(event.target, event.submitter), event.target.id === 'task-filters');
});

function syncScanButton() {
  const button = document.querySelector('.scan-incoming button');
  if (!button) return;
  button.disabled = scanning;
  if (scanning) button.textContent = 'Scanning…';
  else if (scanButtonContent) button.innerHTML = scanButtonContent;
}
document.addEventListener('submit', async event => {
  const form = event.target;
  if (!form.matches('.scan-incoming')) return;
  event.preventDefault();
  if (scanning) return;
  scanning = true;
  syncScanButton();
  const status = document.querySelector('#scan-status');
  const project = new FormData(form).get('project');
  status.hidden = false;
  status.classList.remove('error');
  status.textContent = `Scanning ${project || 'all projects'}… You can keep using this page.`;
  try {
    const response = await fetch(form.action, {
      method: 'POST', credentials: 'same-origin',
      headers: {'Accept': 'application/json'}, body: new URLSearchParams(new FormData(form)),
    });
    if (!response.ok) throw new Error(`Scan failed (${response.status}). Please retry or check the service logs.`);
    const report = await response.json();
    const count = report.queued.length;
    status.textContent = `Scan complete. ${count} ${count === 1 ? 'evaluation' : 'evaluations'} queued.`;
    if (report.errors.length) {
      status.classList.add('error');
      status.textContent += ` ${report.errors.length} ${report.errors.length === 1 ? 'error' : 'errors'}: ${report.errors.join('; ')}`;
    }
    await search(new URL(location.href));
  } catch (error) {
    status.classList.add('error');
    status.textContent = error instanceof TypeError ? 'Scan failed. Check your connection and retry.' : error.message;
  } finally {
    scanning = false;
    syncScanButton();
  }
});

function replaceRegion(page, selector) {
  const current = document.querySelector(selector);
  const updated = page.querySelector(selector);
  if (!current || !updated) return;
  const focusedFilter = current.contains(document.activeElement) && document.activeElement.matches('.filter-tab') ? document.activeElement.value : null;
  if (current.contains(document.activeElement) && focusedFilter === null) return;
  current.replaceWith(updated);
  updateRelativeTimes();
  updated.querySelectorAll('textarea').forEach(sizeTextarea);
  if (focusedFilter !== null) [...updated.querySelectorAll('.filter-tab')].find(button => button.value === focusedFilter)?.focus({preventScroll: true});
}
async function refresh() {
  const current = document.querySelector('#live-content');
  if (!current || current.dataset.refresh !== 'true' || document.hidden || searching || scanning || document.body.classList.contains('busy')) return;
  const version = ++requestVersion;
  try {
    const page = await fetchPage(window.location.href);
    if (!page || version !== requestVersion || window.getSelection()?.toString()) return;
    replaceRegion(page, '#live-content');
    // Feedback is outside the live result region. Polling must never discard a draft.
    if (!dirty) replaceRegion(page, '#task-actions');
    replaceRegion(page, '#activity');
    replaceRegion(page, '#task-overview');
  } catch {
    // A transient connection failure leaves the last known content and all drafts intact.
  }
}
setInterval(refresh, 5000);

// Enhance native selects with searchable disclosures. The select remains the form value and
// remains usable when JavaScript is absent. Popup options are ordinary keyboard buttons.
for (const projectFilter of document.querySelectorAll('select')) {
  const picker = document.createElement('details');
  picker.className = projectFilter.id === 'project-filter' || projectFilter.id === 'project'
    ? 'project-picker'
    : 'select-picker';
  const summary = document.createElement('summary');
  const popup = document.createElement('div');
  popup.className = 'picker-popup';
  const input = document.createElement('input');
  input.type = 'search';
  const pickerLabel = projectFilter.getAttribute('aria-label')
    || document.querySelector(`label[for="${projectFilter.id}"]`)?.textContent.trim()
    || 'option';
  input.placeholder = `Find ${pickerLabel.toLowerCase()}`;
  input.setAttribute('aria-label', `Find ${pickerLabel.toLowerCase()}`);
  const options = document.createElement('div');
  options.className = 'picker-options';
  const empty = document.createElement('p');
  empty.className = 'muted';
  empty.textContent = `No matching ${pickerLabel.toLowerCase()}s`;
  empty.hidden = true;
  function sync() {
    summary.textContent = projectFilter.selectedOptions[0]?.textContent || 'Select an option';
    summary.setAttribute('aria-label', `${pickerLabel}: ${summary.textContent}`);
    for (const button of options.children) button.setAttribute('aria-pressed', String(button.dataset.value === projectFilter.value));
  }
  for (const option of projectFilter.options) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = option.textContent;
    button.dataset.value = option.value;
    button.addEventListener('click', () => {
      projectFilter.value = option.value;
      sync();
      picker.open = false;
      summary.focus();
      projectFilter.dispatchEvent(new Event('change', {bubbles: true}));
    });
    options.append(button);
  }
  input.addEventListener('input', () => {
    for (const button of options.children) button.hidden = !button.textContent.toLowerCase().includes(input.value.trim().toLowerCase());
    empty.hidden = [...options.children].some(button => !button.hidden);
  });
  picker.addEventListener('toggle', () => {
    if (picker.open) { input.value = ''; input.dispatchEvent(new Event('input')); input.focus(); }
  });
  picker.addEventListener('keydown', event => {
    if (event.key === 'Escape') { picker.open = false; summary.focus(); event.preventDefault(); }
    if (['ArrowDown', 'ArrowUp'].includes(event.key) && picker.open) {
      const visible = [...options.children].filter(button => !button.hidden);
      const index = visible.indexOf(document.activeElement);
      const next = index < 0 ? (event.key === 'ArrowDown' ? 0 : visible.length - 1) : index + (event.key === 'ArrowDown' ? 1 : -1);
      if (visible.length) visible[Math.max(0, Math.min(visible.length - 1, next))].focus();
      event.preventDefault();
    }
    if (event.key === 'Enter' && event.target === input) {
      event.preventDefault();
      [...options.children].find(button => !button.hidden)?.click();
    }
  });
  document.addEventListener('click', event => { if (!picker.contains(event.target)) picker.open = false; });
  projectFilter.addEventListener('change', sync);
  popup.append(input, options, empty);
  picker.append(summary, popup);
  projectFilter.before(picker);
  projectFilter.hidden = true;
  sync();
}
if (projectFilter && searchForm) {
  projectFilter.addEventListener('change', () => searchForm.requestSubmit());
}
