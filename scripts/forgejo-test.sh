#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
#
# SPDX-License-Identifier: Apache-2.0
#
# Runs the forge contract suite against a throwaway Forgejo container. Requires Docker.
# Usage: scripts/forgejo-test.sh [extra cargo test args]

set -euo pipefail

IMAGE="${ARTIDEZI_FORGEJO_IMAGE:-data.forgejo.org/forgejo/forgejo:17.0.1}"
PORT="${ARTIDEZI_FORGEJO_PORT:-$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')}"
NAME="artidezi-forgejo-$$"
USER_NAME=artidezi
PASSWORD="artidezi-test-password-$$"
URL="http://127.0.0.1:${PORT}"

cleanup() {
	docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --rm --name "$NAME" \
	-p "127.0.0.1:${PORT}:3000" \
	-e FORGEJO__security__INSTALL_LOCK=true \
	-e FORGEJO__database__DB_TYPE=sqlite3 \
	-e FORGEJO__server__ROOT_URL="${URL}/" \
	-e FORGEJO__server__HTTP_PORT=3000 \
	-e FORGEJO__service__DISABLE_REGISTRATION=true \
	-e FORGEJO__repository__DEFAULT_BRANCH=main \
	"$IMAGE" >/dev/null

echo "waiting for Forgejo at ${URL}"
for _ in $(seq 1 120); do
	if curl -fsS "${URL}/api/v1/version" >/dev/null 2>&1; then
		break
	fi
	sleep 1
done
curl -fsS "${URL}/api/v1/version" >/dev/null

docker exec -u git "$NAME" forgejo admin user create \
	--admin --username "$USER_NAME" --password "$PASSWORD" \
	--email "${USER_NAME}@artidezi.invalid" --must-change-password=false >/dev/null

TOKEN="$(docker exec -u git "$NAME" forgejo admin user generate-access-token \
	--username "$USER_NAME" --token-name "artidezi-test-$$" --scopes all --raw)"

export ARTIDEZI_FORGEJO_URL="$URL"
export ARTIDEZI_FORGEJO_TOKEN="$TOKEN"
export ARTIDEZI_FORGEJO_USER="$USER_NAME"

cargo test -p artidezi-forge --test forgejo "$@" -- --nocapture
cargo test -p artidezi-orchestrator --test forgejo_slice "$@" -- --nocapture
