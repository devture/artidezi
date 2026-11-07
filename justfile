# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
#
# SPDX-License-Identifier: Apache-2.0

mise_data_dir := env("MISE_DATA_DIR", justfile_directory() / "var/mise")
mise_trusted_config_paths := justfile_directory() / "mise.toml"

# Show help by default
default:
	@just --list --justfile {{ justfile() }}

# Builds a development binary (target/debug/*)
build-dev *extra_args:
	RUST_BACKTRACE=1 cargo build --workspace {{ extra_args }}

# Builds an optimized release binary (target/release/*)
build-release *extra_args: (build-dev "--release")

# Runs the clippy linter
clippy *extra_args:
	cargo clippy --workspace --all-targets {{ extra_args }} -- -D warnings

# Checks formatting
fmt-check:
	cargo fmt --all -- --check

# Runs tests
test *extra_args:
	cargo test --workspace {{ extra_args }}

# Runs the forge contract suite against a throwaway Forgejo container (needs Docker)
forgejo-test *extra_args:
	scripts/forgejo-test.sh {{ extra_args }}

# Builds the container image (also the source of the worker binary, see extract-binary)
image tag="artidezi:dev" version="dev":
	docker build --build-arg ARTIDEZI_VERSION={{ version }} --tag {{ tag }} .

# Ships a locally built image to a host over SSH without a registry (fast iteration path)
deploy-image host tag="artidezi:dev":
	docker save {{ tag }} | ssh {{ host }} docker load

# Copies the release binary out of a built image, for installing on a worker VM
extract-binary tag="artidezi:dev" dest="target/artidezi-linux-amd64":
	#!/usr/bin/env sh
	set -eu
	id="$(docker create {{ tag }})"
	trap 'docker rm -f "$id" >/dev/null' EXIT
	docker cp "$id":/usr/local/bin/artidezi {{ dest }}
	sha256sum {{ dest }}

# Runs the REUSE compliance check
reuse-lint:
	reuse lint

# Checks dependencies for advisories, licenses and sources (needs cargo-deny and network access; CI runs it)
deny:
	cargo deny --locked check

# Runs everything CI would run (except the image build, deny and forgejo-test)
check: fmt-check clippy test release-test reuse-lint

# Tests the release planning scripts
release-test:
	python3 -m unittest discover -s release -p 'test_*.py'

# Creates local configuration and demo-only credentials (no personal accounts)
services-prepare:
	python3 scripts/services.py prepare

# Starts local Forgejo, the authenticated UI/orchestrator and a scripted worker
services-start:
	python3 scripts/services.py start

# Stops the local stack, preserving its data for next time
services-stop:
	python3 scripts/services.py stop

# Shows the local stack's status
services-status:
	python3 scripts/services.py status

# Tails local orchestrator and worker logs
services-tail-logs:
	python3 scripts/services.py logs

# Prints credentials belonging only to the local demo stack
services-credentials:
	python3 scripts/services.py credentials

# Deletes only the local demo data, after an explicit confirmation
services-reset:
	python3 scripts/services.py reset

# Exercises the running local stack through creation, revision, merge and failure paths
services-test:
	python3 scripts/services-test.py

# Checks real browser login, automatic updates and mobile layouts; saves screenshots under var/
services-browser-test:
	node scripts/services-browser-test.mjs

# Invokes mise with the project-local data directory
mise *args: _ensure_mise_data_directory
	#!/bin/sh
	export MISE_DATA_DIR="{{ mise_data_dir }}"
	export MISE_TRUSTED_CONFIG_PATHS="{{ mise_trusted_config_paths }}"
	mise {{ args }}

# Runs prek (pre-commit hooks manager) with the given arguments
prek *args: _ensure_mise_tools_installed
	@just --justfile {{ justfile() }} mise exec -- prek {{ args }}

# Runs pre-commit hooks on staged files
prek-run-on-staged *args: _ensure_mise_tools_installed
	@just --justfile {{ justfile() }} mise exec -- prek run {{ args }}

# Runs pre-commit hooks on all files
prek-run-on-all *args: _ensure_mise_tools_installed
	@just --justfile {{ justfile() }} mise exec -- prek run --all-files {{ args }}

# Installs the git pre-commit hook (runs prek automatically before each commit)
prek-install-git-pre-commit-hook: _ensure_mise_tools_installed
	#!/usr/bin/env sh
	set -eu
	just --justfile {{ justfile() }} mise exec -- prek install
	# The installed git hooks run later under Git, outside this just/mise environment,
	# so they need to be told how to find their tooling:
	#
	# - MISE_DATA_DIR / MISE_TRUSTED_CONFIG_PATHS make mise resolve against this project's
	#   own data directory. Without them mise falls back to the global one and silently
	#   installs a second copy of the tool there.
	# - prek bakes the full path of the currently installed version into the hook
	#   (var/mise/installs/prek/<version>/...), which stops working as soon as the pinned
	#   version changes or old versions are pruned. Pointing at mise's shim instead makes
	#   the hook resolve whatever mise.toml pins, at the time it runs.
	#
	# Which hook files prek installs depends on `default_install_hook_types` in
	# .pre-commit-config.yaml, so patch every hook file that prek generated.
	for hook in "{{ justfile_directory() }}"/.git/hooks/*; do
		[ -f "$hook" ] || continue
		grep -q 'generated by prek' "$hook" || continue
		grep -q '^export MISE_DATA_DIR=' "$hook" || sed -i '2iexport MISE_DATA_DIR="{{ mise_data_dir }}"' "$hook"
		grep -q '^export MISE_TRUSTED_CONFIG_PATHS=' "$hook" || sed -i '3iexport MISE_TRUSTED_CONFIG_PATHS="{{ mise_trusted_config_paths }}"' "$hook"
		sed -i 's#^PREK=".*"$#PREK="{{ mise_data_dir }}/shims/prek"#' "$hook"
	done

# Internal - ensures var/mise directory exists
_ensure_mise_data_directory:
	#!/bin/sh
	if [ ! -d "{{ mise_data_dir }}" ]; then
		mkdir -p "{{ mise_data_dir }}"
	fi

# Internal - ensures mise tools are installed
_ensure_mise_tools_installed: _ensure_mise_data_directory
	@just --justfile {{ justfile() }} mise install --quiet
