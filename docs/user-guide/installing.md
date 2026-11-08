<!--
SPDX-FileCopyrightText: 2026 Slavi Pantaleev

SPDX-License-Identifier: Apache-2.0
-->

# Installing

An instance has 1 orchestrator and at least 1 worker.
Run them on separate machines: the worker runs repository code and a coding agent, so treat its machine as expendable and keep it away from anything you care about.
See [what each boundary guarantees](../architecture/security-and-storage.md).

Use the same release on both.
Pin a release rather than `latest`, and upgrade on purpose. There is no fixed release schedule.

| Tag | Meaning |
| --- | --- |
| `vYYYY.MM.DD.N`, e.g. `v2026.11.07.0` | a release (the same Git tag) |
| `latest` | the newest release |
| `edge` | the newest commit on `main` that passed CI (not a release) |
| `edge-<full commit hash>` | that commit |
| `<tag>-amd64`, `<tag>-arm64` | 1 architecture of a release or `edge-<full commit hash>` tag |

[Releases](../maintainers/releases.md#what-a-release-publishes) explains how to verify what you download.

## With Ansible

The [orchestrator](https://github.com/mother-of-all-self-hosting/ansible-role-artidezi-orchestrator) and [worker](https://github.com/mother-of-all-self-hosting/ansible-role-artidezi-worker) roles do the steps below for you.
[mash-playbook](https://github.com/mother-of-all-self-hosting/mash-playbook) uses them: see its [orchestrator](https://github.com/mother-of-all-self-hosting/mash-playbook/blob/main/components/artidezi_orchestrator/README.md) and [worker](https://github.com/mother-of-all-self-hosting/mash-playbook/blob/main/components/artidezi_worker/README.md) guides.
You still need a [forge account](#forge-account).

## Forge account

ArtiDezi acts as its own account. For GitHub, follow [GitHub enrollment](github-enrollment.md) instead.
On Forgejo:

1. Create a user for the bot, and put its name in the forge's `bot_login`.
2. Give it write access to every repository it may change, and read access to the rest.
3. Create an access token for it with the scopes `write:repository`, `write:issue`, `read:user` and `read:organization`.

The bot can then push workflow files, so read [how to prepare a repository's CI](projects.md#prepare-the-repositorys-ci) before you enroll one.

## Orchestrator

The image's default command runs `artidezi serve` with:

| Path | Purpose |
| --- | --- |
| `/etc/artidezi/artidezi.yaml` | the [configuration](operations.md) |
| `/etc/artidezi/secrets/` | 1 file per `credential_ref` |
| `/var/lib/artidezi/` | a suitable `instance.data_dir`, for the database, artifacts and Git caches |

The container runs as UID and GID 2003, so that user needs to read the first 2 paths and own the data directory.

1. Write the configuration, starting from the [example](../../etc/example.yaml). The rest of this page matches it.

2. Create the secrets. At least the admin token, 1 token per worker and the forge credential:

   ```sh
   mkdir -p /etc/artidezi/secrets /var/lib/artidezi
   openssl rand -hex 32 > /etc/artidezi/secrets/admin-token
   openssl rand -hex 32 > /etc/artidezi/secrets/worker-1-token
   cp bot-token /etc/artidezi/secrets/forgejo-token # the bot's Forgejo token
   chown -R 2003:2003 /etc/artidezi /var/lib/artidezi
   chmod 0700 /etc/artidezi/secrets
   ```

3. Validate the configuration, then start the orchestrator:

   ```sh
   image=ghcr.io/devture/artidezi:v2026.11.07.0

   docker run --rm --volume /etc/artidezi:/etc/artidezi:ro "$image" validate-config /etc/artidezi/artidezi.yaml

   docker run --detach --name artidezi --restart unless-stopped \
     --volume /etc/artidezi:/etc/artidezi:ro \
     --volume /var/lib/artidezi:/var/lib/artidezi \
     --publish 127.0.0.1:8080:8080 \
     "$image"
   ```

4. Put a reverse proxy with TLS in front of it, preserving the `Host` header. Workers refuse plain HTTP to anything but loopback.

The web UI is off until you [declare accounts](operations.md#enabling-the-web-ui) and add `--env ARTIDEZI_UI_ENABLED=true`.

## Worker

A worker needs:

- `git`;
- the harness its profiles use, at a version matching `harness_version_constraint`. A job waits in the queue while no worker has a matching version;
- whatever the project's verification commands need;
- HTTPS access to the orchestrator. Workers only make outgoing connections.

It doesn't need the orchestrator's configuration or any forge credential.

1. Install the binary from the [GitHub release](https://github.com/devture/artidezi/releases):

   ```sh
   version=v2026.11.07.0
   arch=amd64 # or arm64
   base=https://github.com/devture/artidezi/releases/download/$version
   curl -fsSLO "$base/artidezi-linux-$arch"
   curl -fsSLO "$base/SHA256SUMS"
   sha256sum --check --ignore-missing SHA256SUMS
   install -m 0755 "artidezi-linux-$arch" /usr/local/bin/artidezi
   ```

2. Create a dedicated user, and copy this worker's token from the orchestrator:

   ```sh
   useradd --system --create-home --home-dir /var/lib/artidezi-worker artidezi-worker
   install -d -o artidezi-worker -m 0700 /etc/artidezi-worker
   install -o artidezi-worker -m 0600 worker-1-token /etc/artidezi-worker/token
   ```

3. Write `/etc/artidezi-worker/worker.yaml`:

   ```yaml
   orchestrator_url: https://artidezi.example.com
   token_file: /etc/artidezi-worker/token
   worker_pool: maintenance
   worker_id: worker-1
   workspaces_root: /var/lib/artidezi-worker/workspaces
   credentials_root: /var/lib/artidezi-worker/credentials
   codex_binary: codex
   ```

   | Field | Meaning |
   | --- | --- |
   | `orchestrator_url` | The orchestrator's address |
   | `allow_insecure_orchestrator_url` | Permits sending the token over plain HTTP to a non-loopback host. Default `false` |
   | `token_file` | File holding this worker's token |
   | `worker_pool` | The `worker_pools` key this worker serves |
   | `worker_id` | This worker's name under `workers` in that pool |
   | `tags` | What this worker offers, such as `docker-support`. See [Worker tags](projects.md#worker-tags) |
   | `workspaces_root` | Where attempts run. Unique per worker |
   | `credentials_root` | Model credentials, by `credential_ref`. Unique per worker |
   | `poll_seconds` | How often to ask for work. Default 15 |
   | `max_log_bytes` | Limit for each captured log. Default 16 MiB |
   | `codex_binary`, `pi_binary` | The harnesses this worker offers, by name or path. Set at least 1 |

4. Provide the model credentials, in `<credentials_root>/<credential_ref>/`, owned by the worker's user:

   | Harness | `auth_mode` | File |
   | --- | --- | --- |
   | Codex | `openai_api_key` | `api_key` |
   | Codex | `chatgpt_login` | `auth.json` |
   | Pi | `openai_api_key` | `api_key` |

   ```sh
   install -d -o artidezi-worker -m 0700 /var/lib/artidezi-worker/credentials/openai
   install -o artidezi-worker -m 0600 api_key /var/lib/artidezi-worker/credentials/openai/api_key
   ```

   For `chatgpt_login`, sign in on each worker separately, as its user, with `CODEX_HOME` set to the credential directory, then run `codex login`.
   Don't copy `auth.json` between workers: [token rotation](operations.md#configuration-layers-and-current-support) on one invalidates the others.

5. Run it under systemd, in `/etc/systemd/system/artidezi-worker.service`:

   ```ini
   [Unit]
   Description=ArtiDezi worker
   After=network-online.target
   Wants=network-online.target

   [Service]
   User=artidezi-worker
   ExecStart=/usr/local/bin/artidezi worker --config /etc/artidezi-worker/worker.yaml
   Restart=on-failure

   [Install]
   WantedBy=multi-user.target
   ```

   ```sh
   systemctl daemon-reload
   systemctl enable --now artidezi-worker
   ```

The worker doesn't isolate itself from the network or the host. [Enforce that outside it](../architecture/security-and-storage.md), for example with a VM and its firewall.

## First task

From any machine that can reach the orchestrator:

```sh
export ARTIDEZI_URL=https://artidezi.example.com
export ARTIDEZI_ADMIN_TOKEN_FILE=/path/to/admin-token
artidezi task create --project myapp --instructions 'Add a small script and test it.'
```

Next, [choose your projects](projects.md) and [prepare their CI](projects.md#prepare-the-repositorys-ci) before enrolling real repositories.
