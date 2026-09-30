#!/bin/sh
set -e

# Derive host/port from DATABASE_URL (the one var set consistently across
# docker-compose, the Helm chart and the operator). Explicit DB_HOST/DB_PORT
# still win; the old compose default is only a last resort.
#
# DATABASE_URL looks like: postgres://user:pass@host:port/dbname
if [ -n "$DATABASE_URL" ]; then
  hostport=$(printf '%s' "$DATABASE_URL" | sed -E 's#^[a-zA-Z]+://##; s#^[^@]*@##; s#/.*$##')
  url_host=$(printf '%s' "$hostport" | cut -d: -f1)
  case "$hostport" in
    *:*) url_port=$(printf '%s' "$hostport" | cut -d: -f2) ;;
    *)   url_port="" ;;
  esac
fi

DB_HOST="${DB_HOST:-${url_host:-tidasone-db-v2}}"
DB_PORT="${DB_PORT:-${url_port:-5432}}"

echo "⏳ Waiting for Postgres at ${DB_HOST}:${DB_PORT}..."

# Loop until the TCP port accepts connections.
while ! nc -z "$DB_HOST" "$DB_PORT"; do
  echo "   Postgres not ready yet, retrying..."
  sleep 2
done

echo "✅ Postgres is up! Starting API..."
exec /app/api
