#!/usr/bin/env bash
# waxwing restore against stock graph-node: graphman creates the deployment
# parked, waxwing loads it with COPY, builds the dump's indexes and hands it
# to B's node. Then B indexes on by itself, and must agree with A.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

psql() { docker compose exec -T postgres-$1 psql -U graph-node -Atc "$2"; }
indexes() {
  local nsp; nsp=$(psql $1 "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
  psql $1 "select indexdef from pg_indexes where schemaname = '$nsp' order by indexname" | sed "s/$nsp\./NSP./g"
}

setup
F1=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
spawn; pings 30
wait_synced $A_INDEX
NSP=$(psql a "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
psql a "drop index $NSP.attr_2_1_ping_event_n" >/dev/null
F2=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
echo "dumps at $F1 and $F2: $(find $DUMP -name 'clamp_*' | wc -l | tr -d ' ') clamp file(s)"

docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
waxwing restore $DUMP --db $B_DB \
  --config config/b.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default

if diff <(indexes a) <(indexes b); then echo "ok        index set"; else
  echo "MISMATCH  index set (< A, > B)"; fail=1
fi
# Lexemes only: graph-node orders a search's fields by a per-process hash,
# so two nodes can disagree on positions for the same row.
lexemes() {
  local nsp; nsp=$(psql $1 "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
  psql $1 "select md5(string_agg(vid || ':' || strip(child_search)::text, ',' order by vid)) from $nsp.child"
}
check "fulltext lexemes of restored children" "$(lexemes a)" "$(lexemes b)"
search() {
  gql "$1/subgraphs/id/$DEPLOYMENT" '{ childSearch(text: "louder") { id } }' | jq -c '[.data.childSearch[].id] | sort'
}
check "fulltext search" "$(search $A_QUERY)" "$(search $B_QUERY)"

spawn; pings 30
wait_synced $A_INDEX; wait_synced $B_INDEX
compare $((F1 - 10)) "$F1" $((F2 - 10)) "$F2" "$(head_block)"

dump a rig-a >/dev/null
dump b rig-b >/dev/null
waxwing diff work/dumps/rig-a work/dumps/rig-b || fail=1
exit $fail
