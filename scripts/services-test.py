#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
# SPDX-License-Identifier: Apache-2.0
"""Exercise only the local stack. No URL or credential overrides are accepted."""
import http.cookiejar
import importlib.util
import json
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('services', Path(__file__).with_name('services.py'))
services = importlib.util.module_from_spec(spec)
spec.loader.exec_module(services)
values = services.settings()
base = f"http://127.0.0.1:{values['ARTIDEZI_UI_PORT']}"
forge = f"http://127.0.0.1:{values['ARTIDEZI_FORGEJO_PORT']}/api/v1"
state = services.STATE
browser = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
admin_token = (state / 'secrets/admin-token').read_text().strip()
forge_token = (state / 'secrets/forgejo-token').read_text().strip()


def page(path, form=None):
    headers = {'Origin': base}
    data = None
    if form is not None:
        data = urllib.parse.urlencode(form).encode()
    with browser.open(urllib.request.Request(base + path, headers=headers, data=data), timeout=30) as response:
        return response.url, response.read().decode()


def csrf(html):
    return re.search(r'name="csrf" value="([0-9a-f]+)"', html)[1]


def admin(path, body=None):
    headers = {'Authorization': 'Bearer ' + admin_token, 'Content-Type': 'application/json'}
    data = None if body is None else json.dumps(body).encode()
    with urllib.request.urlopen(urllib.request.Request(base + '/admin/v1' + path, headers=headers, data=data), timeout=30) as response:
        return json.load(response)


def wait(task_id, expected):
    for _ in range(90):
        detail = admin('/tasks/' + task_id)
        status = detail['task']['state']
        if status == expected:
            return detail
        if status in ['needs_human', 'closed', 'done'] and status != expected:
            raise AssertionError(f'Task {task_id} unexpectedly reached {status}: {[a["validation_problems"] for a in detail["attempts"]]}')
        time.sleep(1)
    raise AssertionError(f'Task {task_id} did not reach {expected}')


def create(instructions, profile='demo'):
    _, html = page('/tasks/new')
    url, html = page('/tasks', {'csrf': csrf(html), 'project': 'playground', 'instructions': instructions, 'agent_profile': profile, 'delivery_mode': 'merge_request'})
    number = urllib.parse.urlparse(url).path.rsplit('/', 1)[-1]
    assert number.isdigit(), 'Creation did not redirect to a numbered task'
    task_id = re.search(r'data-task-id="(tsk_[a-z0-9]+)"', html)[1]
    return task_id


def merge_when_ready(number, expected_head):
    # Forgejo computes mergeability asynchronously after a branch update. Waiting for the
    # controller's awaiting_review state alone does not establish forge readiness.
    path = forge + f'/repos/artidezi/playground/pulls/{number}'
    for _ in range(60):
        pull = services.api(path, forge_token)
        assert pull['state'] == 'open' and not pull.get('draft'), 'Expected an open, non-draft local fixture PR'
        assert pull['head']['sha'] == expected_head, 'Refuse to merge a changed fixture head'
        if pull.get('mergeable') is True:
            try:
                services.api(path + '/merge', forge_token, {'Do': 'merge', 'head_commit_id': expected_head})
                return
            except urllib.error.HTTPError as error:
                if error.code != 405:
                    raise
                # Readiness can change between GET and POST. Recheck instead of forcing a merge.
        time.sleep(1)
    raise AssertionError(f'Local fixture PR #{number} did not become ready to merge')


def main():
    assert services.api(forge + '/repos/artidezi/playground', forge_token)['private']
    url, html = page('/tasks')
    assert urllib.parse.urlparse(url).path == '/login', 'Task data must require login'
    password = (state / 'secrets/ui-password').read_text().strip()
    url, html = page('/login', {'username': 'local-admin', 'password': password})
    assert urllib.parse.urlparse(url).path == '/tasks'
    try:
        page('/tasks', {'csrf': 'wrong', 'project': 'playground', 'instructions': 'must be rejected'})
    except urllib.error.HTTPError as error:
        assert error.code == 403
    else:
        raise AssertionError('CSRF protection failed')
    task_id = create('Document the local development workflow and its verification steps.')
    first = wait(task_id, 'awaiting_review')
    mr = first['merge_requests'][0]
    assert first['attempts'][0]['state'] == 'valid'
    _, html = page('/tasks/' + task_id)
    assert 'Ready for review' in html and 'fixture-check' in html
    # Follow a log link through browser auth and check it is served as inert text.
    logs = re.findall(r'href="(/tasks/[^\"]+/logs/[0-9a-f]+)"', html)
    assert logs, 'The task should expose evidence links'
    with browser.open(base + logs[0], timeout=20) as response:
        assert response.headers['Content-Type'].startswith('text/plain')
    page('/tasks/' + task_id + '/revisions', {'csrf': csrf(html), 'feedback': 'Also include the restart and cleanup workflow.'})
    revised = wait(task_id, 'awaiting_review')
    assert len(revised['attempts']) == 2
    assert revised['merge_requests'][0]['number'] == mr['number']
    assert revised['merge_requests'][0]['head_commit'] != mr['head_commit']
    assert all(a['state'] == 'valid' for a in revised['attempts'])
    # This is a simulated human merge on the local forge, not an orchestrator automerge.
    merge_when_ready(mr['number'], revised['merge_requests'][0]['head_commit'])
    done = wait(task_id, 'done')
    assert done['merge_requests'][0]['state'] == 'merged'
    print('PASS: browser login → create → worker → merge request → revision → same merge request → merge → done')
    needs = create('Clarify the compatibility policy before making this change.', 'demo-needs-input')
    detail = wait(needs, 'needs_human')
    assert not detail['merge_requests']
    _, html = page('/tasks/' + needs)
    assert 'Which compatibility constraints' in html
    print('PASS: needs-input outcome and question visible in the UI')
    failed = create('Exercise a failed verification without publishing a proposal.', 'demo-failing-check')
    detail = wait(failed, 'needs_human')
    assert detail['attempts'][0]['state'] == 'invalid' and not detail['merge_requests']
    _, html = page('/tasks/' + failed)
    assert 'Failed' in html
    print('PASS: failed check blocks publication and is visible in the UI')
    closed = create('Close this queued task without publishing a change.')
    _, html = page('/tasks/' + closed)
    _, confirmation = page('/tasks/' + closed + '/close', {'csrf': csrf(html)})
    assert 'Close this task?' in confirmation
    assert admin('/tasks/' + closed)['task']['state'] == 'working'
    page('/tasks/' + closed + '/close', {'csrf': csrf(confirmation), 'confirmed': 'true'})
    detail = wait(closed, 'closed')
    assert not detail['merge_requests']
    print('PASS: close cancels queued work')
    review = create('Prepare a short maintenance note for review.')
    wait(review, 'awaiting_review')
    print('PASS: an open proposal is ready to explore in the local UI')
    _, html = page('/tasks')
    page('/logout', {'csrf': csrf(html)})
    url, _ = page('/tasks')
    assert urllib.parse.urlparse(url).path == '/login'
    print('PASS: logout revokes the browser session')
    print('Local lifecycle checks complete.')


if __name__ == '__main__':
    main()
