#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
# SPDX-License-Identifier: Apache-2.0
"""Deterministic demo: task prose is data and is never evaluated as a command."""
import json
import os
from pathlib import Path
import subprocess
import time

workspace = Path(os.environ['ARTIDEZI_WORKSPACE'])
job = json.loads((workspace / 'task/job.json').read_text())
profile = os.environ['ARTIDEZI_FAKE_MODEL']
# Long enough for the UI to show the queued/working states; no model calls.
time.sleep(3)
result = {'schema_version': 1, 'outcome': 'proposal', 'summary': 'Recorded the requested work as a local demo change.', 'repositories': []}
if profile == 'needs_input':
    result.update(outcome='needs_human', summary='This demo is waiting for a human decision.', questions_for_human=['Which compatibility constraints should the change preserve?'])
    result['repositories'] = [{'id': 'playground', 'changed': False}]
else:
    notes = Path('notes')
    notes.mkdir(exist_ok=True)
    task_id = job['task_id']
    assert task_id.replace('_', '').isalnum()
    note = notes / f'{task_id}.md'
    task_document = Path(os.environ['ARTIDEZI_TASK_DOCUMENT']).read_text()
    note.write_text('# Local maintenance note\n\n' + task_document + '\n')
    subprocess.run(['git', 'add', str(note)], check=True)
    if profile == 'failing_check':
        Path('FAIL_CHECK').write_text('Intentional failure for the local demonstration.\n')
        subprocess.run(['git', 'add', 'FAIL_CHECK'], check=True)
        result['summary'] = 'Prepared a demonstration change whose verification intentionally fails.'
    subprocess.run(['git', 'commit', '-q', '-m', 'Record local maintenance request'], check=True)
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    result['repositories'] = [{'id': 'playground', 'changed': True, 'head_commit': head}]
Path(os.environ['ARTIDEZI_RESULT_PATH']).write_text(json.dumps(result))
print('model=scripted-demo')
print('input_tokens=0')
print('output_tokens=0')
