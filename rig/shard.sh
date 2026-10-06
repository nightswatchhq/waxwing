#!/usr/bin/env bash
# A receiver with two shards: the deployment goes to shard1, graph-node's
# catalogue of deployments stays in the primary. waxwing restore must load
# the shard, write the shard's metadata, and find the deployment in the
# primary; waxwing indexes must find it too.
set -euo pipefail
cd "$(dirname "$0")"
export CONFIG_SUFFIX=-sharded COMPOSE_FILE=docker-compose.yml:docker-compose.shard.yml
. ./lib.sh
PRIMARY=$B_DB
SHARD=postgresql://graph-node:let-me-in@localhost:35432/graph-node

psql() { docker compose exec -T $1 psql -U graph-node -Atc "$2"; }
indexes() {  # postgres service, namespace
  psql $1 "select indexdef from pg_indexes where schemaname = '$2' order by indexname" | sed "s/$2\./NSP./g"
}

setup
F=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
docker compose exec -T graph-node-b graphman --config /config/b-sharded.toml create $NAME >/dev/null
waxwing restore $DUMP --db $SHARD --primary-db $PRIMARY --shard shard1 \
  --config config/b-sharded.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default | tail -1

placed=$(psql postgres-b "select shard || ' ' || name from deployment_schemas where subgraph = '$DEPLOYMENT'")
echo "restored into: $placed"
check "restored into shard1" "$(cut -d' ' -f1 <<<"$placed")" shard1
NSP_A=$(psql postgres-a "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
NSP_B=$(cut -d' ' -f2 <<<"$placed")
check "index set" "$(indexes postgres-a $NSP_A | tr '\n' ';')" "$(indexes postgres-b2 $NSP_B | tr '\n' ';')"
waxwing indexes $DUMP --db $SHARD --primary-db $PRIMARY | head -1 || fail=1

spawn; pings 20
wait_synced $A_INDEX; wait_synced $B_INDEX
compare "$F" "$(head_block)"
dump a rig-a >/dev/null
dump b rig-b >/dev/null
waxwing diff work/dumps/rig-a work/dumps/rig-b || fail=1
exit $fail
