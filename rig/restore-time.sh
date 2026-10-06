#!/usr/bin/env bash
# How long does building indexes during the import cost? Pad A's tables
# with ROWS synthetic rows by SQL, dump once, and time graphman restore into
# a fresh B on stock graph-node, on PATCHED, a build that creates the
# tables bare and indexes after the import, and with waxwing restore.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
ROWS=${ROWS:-5000000}
PATCHED=${PATCHED:-waxwing/graph-node:restore-indexes}

psql() { docker compose exec -T postgres-$1 psql -U graph-node -Atc "$2"; }

setup
NSP=$(psql a "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
B=$(head_block)
psql a "insert into $NSP.ping_event (block\$, id, child, n, block)
  select $B, int8send(i), int8send(i % 1000), i, $B from generate_series(1000, 999 + $ROWS) i" >/dev/null
psql a "insert into $NSP.child (block_range, id, pings, created_at)
  select int4range($B, null), int8send(i), i, $B from generate_series(1000, 999 + $ROWS) i" >/dev/null
echo "padded with $ROWS pings and $ROWS children: $(psql a "select pg_size_pretty(sum(pg_total_relation_size(c.oid))) from pg_class c join pg_namespace n on n.oid = c.relnamespace where nspname = '$NSP'")"
dump >/dev/null

timed_restore() {
  docker compose rm -sfv graph-node-b postgres-b >/dev/null 2>&1
  GRAPH_NODE_IMAGE=$1 docker compose up -d graph-node-b >/dev/null 2>&1
  wait_http $B_INDEX
  docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
  local start=$SECONDS
  docker compose exec -T graph-node-b graphman --config /config/b.toml restore /dumps/rig --name $NAME >/dev/null
  echo "$1: restore took $((SECONDS - start))s"
}
timed_restore graphprotocol/graph-node:v0.45.0
timed_restore $PATCHED

docker compose rm -sfv graph-node-b postgres-b >/dev/null 2>&1
docker compose up -d graph-node-b >/dev/null 2>&1
wait_http $B_INDEX
docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
rm -rf work/dumps/waxwing
start=$SECONDS
waxwing restore $DUMP --db postgresql://graph-node:let-me-in@localhost:25432/graph-node \
  --config config/b.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default >/dev/null
echo "waxwing restore on stock v0.45.0: took $((SECONDS - start))s"
