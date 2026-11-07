#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
# SPDX-License-Identifier: Apache-2.0
"""Manage the local-only Compose stack, without any personal credentials."""
import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
STATE = ROOT / 'var/services'
COMPOSE = ROOT / 'etc/services/compose.yml'


def run(args, capture=False):
    result = subprocess.run(args, cwd=ROOT, text=True, capture_output=capture)
    if result.returncode:
        # Captured commands may contain local fixture credentials. Never echo their output.
        raise RuntimeError(f'Command failed: {args[0]} (exit {result.returncode})')
    return result.stdout if capture else None


def ui_initial_hash(password_file):
    compose('build', 'orchestrator')
    return run(['docker', 'run', '--rm', '--pull', 'never', '--network', 'none',
                '--user', f'{os.getuid()}:{os.getgid()}',
                '--mount', f'type=bind,src={password_file},dst=/password,ro',
                'artidezi:local', 'ui-hash-password', '--password-file', '/password'],
               capture=True).strip()


def settings():
    values = {'ARTIDEZI_UID': str(os.getuid()), 'ARTIDEZI_GID': str(os.getgid()),
              'ARTIDEZI_UI_PORT': '3080', 'ARTIDEZI_FORGEJO_PORT': '3081',
              'ARTIDEZI_LOCAL_HARNESS': 'fake', 'ARTIDEZI_LOCAL_PI_CREDENTIAL': 'venice'}
    env_file = STATE / 'env'
    if env_file.exists():
        for line in env_file.read_text().splitlines():
            if '=' in line:
                name, value = line.split('=', 1)
                if name in values:
                    values[name] = value
    values['ARTIDEZI_LOCAL_HARNESS'] = os.environ.get('ARTIDEZI_LOCAL_HARNESS', values['ARTIDEZI_LOCAL_HARNESS'])
    if values['ARTIDEZI_LOCAL_HARNESS'] not in {'fake', 'pi'}:
        raise RuntimeError('ARTIDEZI_LOCAL_HARNESS must be fake or pi')
    values['ARTIDEZI_LOCAL_PI_CREDENTIAL'] = os.environ.get(
        'ARTIDEZI_LOCAL_PI_CREDENTIAL', values['ARTIDEZI_LOCAL_PI_CREDENTIAL'])
    if not values['ARTIDEZI_LOCAL_PI_CREDENTIAL'] or '/' in values['ARTIDEZI_LOCAL_PI_CREDENTIAL']:
        raise RuntimeError('ARTIDEZI_LOCAL_PI_CREDENTIAL must be a credential reference name')
    for name in ['ARTIDEZI_UI_PORT', 'ARTIDEZI_FORGEJO_PORT']:
        values[name] = os.environ.get(name, values[name])
        if not 1024 <= int(values[name]) <= 65535:
            raise RuntimeError(f'{name} must be a port in 1024..65535')
    if values['ARTIDEZI_UI_PORT'] == values['ARTIDEZI_FORGEJO_PORT']:
        raise RuntimeError('The UI and Forgejo need different ports')
    return values


def prepare():
    os.umask(0o077)
    for name in ['config', 'secrets', 'credentials', 'data', 'workspaces']:
        (STATE / name).mkdir(parents=True, exist_ok=True)
    values = settings()
    credential_path = STATE / 'credentials' / values['ARTIDEZI_LOCAL_PI_CREDENTIAL'] / 'api_key'
    if values['ARTIDEZI_LOCAL_HARNESS'] == 'pi' and not credential_path.is_file():
        raise RuntimeError(f"Pi mode needs {credential_path.relative_to(ROOT)} (an untracked local API key)")
    if values['ARTIDEZI_LOCAL_HARNESS'] == 'pi':
        credential_path.parent.chmod(0o700)
    (STATE / 'env').write_text(''.join(f'{k}={v}\n' for k, v in values.items()))
    for name in ['admin-token', 'worker-token', 'ui-password', 'forgejo-password']:
        path = STATE / 'secrets' / name
        if not path.exists():
            path.write_text(secrets.token_hex(32) + '\n')
        path.chmod(0o600)
    for name in ['orchestrator.yaml', 'worker.yaml']:
        source = ROOT / 'etc/services' / name
        if values['ARTIDEZI_LOCAL_HARNESS'] == 'pi':
            source = ROOT / 'etc/services' / ('pi-' + name)
        # Optional local configuration survives rebuilds without changing the credential-free
        # demo defaults. It contains policy/credential references, never credential values.
        if name == 'orchestrator.yaml' and (STATE / 'orchestrator.local.yaml').exists():
            source = STATE / 'orchestrator.local.yaml'
        text = source.read_text().replace('${ARTIDEZI_FORGEJO_PORT}', values['ARTIDEZI_FORGEJO_PORT'])
        if name == 'orchestrator.yaml' and '\nui_access:' not in '\n' + text:
            initial_hash = ui_initial_hash(STATE / 'secrets/ui-password')
            text += ('\nui_access:\n  schema_version: 1\n  accounts:\n'
                     '    local-admin:\n      display_name: Local administrator\n'
                     f'      initial_password_hash: "{initial_hash}"\n'
                     '      instance_admin: true\n')
        (STATE / 'config' / name).write_text(text)
    return values


def compose(*args, capture=False):
    return run(['docker', 'compose', '--env-file', str(STATE / 'env'), '-f', str(COMPOSE), *args], capture=capture)


def api(url, token=None, body=None, method=None, basic=None):
    headers = {'Accept': 'application/json'}
    if token:
        headers['Authorization'] = 'token ' + token
    if basic:
        headers['Authorization'] = 'Basic ' + base64.b64encode((':'.join(basic)).encode()).decode()
    if body is not None:
        headers['Content-Type'] = 'application/json'
    request = urllib.request.Request(url, headers=headers, data=None if body is None else json.dumps(body).encode(), method=method)
    with urllib.request.urlopen(request, timeout=20) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def bootstrap(values):
    base = f"http://127.0.0.1:{values['ARTIDEZI_FORGEJO_PORT']}/api/v1"
    for _ in range(60):
        try:
            api(base + '/version')
            break
        except (urllib.error.URLError, TimeoutError):
            time.sleep(1)
    else:
        raise RuntimeError('Local Forgejo did not become ready')
    password = (STATE / 'secrets/forgejo-password').read_text().strip()
    accounts = compose('exec', '-T', '--user', 'git', 'forgejo', 'forgejo', 'admin', 'user', 'list', capture=True)
    if not any(len(row.split()) > 1 and row.split()[1] == 'artidezi' for row in accounts.splitlines()):
        compose('exec', '-T', '--user', 'git', 'forgejo', 'forgejo', 'admin', 'user', 'create',
                '--username', 'artidezi', '--password', password, '--email', 'artidezi@local.invalid',
                '--must-change-password=false', capture=True)
    token_path = STATE / 'secrets/forgejo-token'
    if not token_path.exists():
        token = compose('exec', '-T', '--user', 'git', 'forgejo', 'forgejo', 'admin', 'user',
                        'generate-access-token', '--username', 'artidezi', '--token-name', 'local-orchestrator',
                        '--scopes', 'write:repository,write:issue,read:user,read:organization', '--raw', capture=True).strip()
        if not token or any(c not in '0123456789abcdef' for c in token):
            raise RuntimeError('Forgejo did not return a valid token')
        token_path.write_text(token + '\n')
        token_path.chmod(0o600)
    token = token_path.read_text().strip()
    try:
        repo = api(base + '/repos/artidezi/playground', token)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        repo = api(base + '/user/repos', body={'name': 'playground', 'private': True,
                   'auto_init': True, 'default_branch': 'main', 'description': 'Private local ArtiDezi fixture'},
                   basic=('artidezi', password))
    if not repo['private'] or repo['default_branch'] != 'main':
        raise RuntimeError('The local playground must remain private with main as its default branch')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['prepare', 'start', 'stop', 'logs', 'status', 'credentials', 'reset'])
    args = parser.parse_args()
    if args.command in ['prepare', 'start']:
        values = prepare()
        if args.command == 'start':
            compose('up', '-d', '--wait', 'forgejo')
            bootstrap(values)
            compose('up', '-d', '--build', '--wait', 'orchestrator', 'worker')
            print(f"UI: http://artidezi.127.0.0.1.nip.io:{values['ARTIDEZI_UI_PORT']}")
            print(f"Forgejo: http://artidezi-forgejo.127.0.0.1.nip.io:{values['ARTIDEZI_FORGEJO_PORT']}")
            print('Local login details: just services-credentials')
            print('Run the full lifecycle check: just services-test')
    elif args.command == 'credentials':
        print('These credentials belong only to the throwaway local stack.')
        print('UI username: local-admin')
        print('UI password:', (STATE / 'secrets/ui-password').read_text().strip())
        print('Forgejo username: artidezi')
        print('Forgejo password:', (STATE / 'secrets/forgejo-password').read_text().strip())
    elif args.command == 'reset':
        if input('Delete all local ArtiDezi demo data? Type reset: ') != 'reset':
            raise RuntimeError('Reset cancelled')
        compose('down', '--volumes', '--remove-orphans')
        shutil.rmtree(STATE)
    elif args.command == 'stop':
        compose('down', '--remove-orphans')
    elif args.command == 'logs':
        compose('logs', '--follow', '--tail=80', 'orchestrator', 'worker')
    else:
        compose('ps')


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, urllib.error.URLError) as error:
        print(f'Local services: {error}', file=sys.stderr)
        sys.exit(1)
