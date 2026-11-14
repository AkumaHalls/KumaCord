#!/bin/sh
# Read-only probe of the KumaCord hub on the production VPS (147.15.124.229).
# Prints: repo commit, tag, dirty files, container age, and whether the running
# hub answers /api/admin/structure (404 = old build, 405/400/401/200 = new build).
set -u
KEY=/c/Users/Administrator/Documents/ssh-key-2026-03-23.key
SSH="ssh -i $KEY -o StrictHostKeyChecking=no -o BatchMode=yes -o ConnectTimeout=15 -o LogLevel=ERROR ubuntu@147.15.124.229"

echo "=== repo ==="
$SSH 'cd /home/ubuntu/KumaCord && git log --oneline -1 && git describe --tags 2>/dev/null; git status --short | head -5'

echo "=== container ==="
$SSH 'docker ps --filter name=kumacord --format "{{.Names}} {{.Image}} {{.Status}} {{.CreatedAt}}"'

echo "=== imagem build ==="
$SSH 'docker inspect kumacord --format "{{.Created}}"'

echo "=== rota structure no hub em execucao ==="
$SSH 'curl -s -o /dev/null -w "POST /api/admin/structure -> %{http_code}\n" -X POST -H "Content-Type: application/json" -d "{}" --max-time 8 http://127.0.0.1:7890/api/admin/structure'

echo "=== versao do hub no log ==="
$SSH 'docker logs kumacord 2>&1 | grep -i -m3 -E "listening|version|1\.[0-9]" | tail -3'
