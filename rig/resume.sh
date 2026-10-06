#!/usr/bin/env bash
# Interrupt waxwing restore mid-load, run it again, and check B ends up as A.
# Pads A with ROWS synthetic rows so the load takes long enough to kill.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
ROWS=${ROWS:-3000000}

psql() { docker compose exec -T postgres-$1 psql -U graph-node -Atc "$2"; }
nsp() { psql $1 "select name from deployment_schemas where subgraph = '$DEPLOYMENT'"; }
# Everything a restore could get wrong, as one line.
summary() {
  local nsp; nsp=$(nsp $1)
  psql $1 "select (select count(*) || ' ' || sum(hashtext(id::text || n::text || block\$)) from $nsp.ping_event),
                  (select count(*) || ' ' || sum(hashtext(id::text || block_range::text || label)) from $nsp.child),
                  (select md5(string_agg(strip(child_search)::text, ',' order by vid)) from $nsp.child),
                  (select count(*) from pg_indexes where schemaname = '$nsp'),
                  (select block_number from subgraphs.head h join deployment_schemas s on s.id = h.id where s.name = '$nsp')"
}
restore_b() {
  waxwing restore $DUMP --db $B_DB \
    --config config/b.toml --graphman "docker compose exec -T graph-node-b graphman" \
    --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default
}

setup
NSP=$(nsp a)
B=$(head_block)
psql a "insert into $NSP.ping_event (block\$, id, child, n, block)
  select $B, int8send(i), (select id from $NSP.child order by vid limit 1), i, $B from generate_series(1000, 999 + $ROWS) i" >/dev/null
psql a "insert into $NSP.child (block_range, id, pings, created_at, label, child_search)
  select int4range($B, null), int8send(i), i, $B, 'padded child ' || i, to_tsvector('english', 'padded child ' || i)
    from generate_series(1000, 999 + $ROWS) i" >/dev/null
dump >/dev/null
docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null

# Killed once the first table is in and the second is loading.
restore_b > work/resume-first.log 2>&1 &
pid=$!
until grep -q "loading ping_event" work/resume-first.log 2>/dev/null; do
  kill -0 $pid 2>/dev/null || { cat work/resume-first.log; echo "FAIL: restore ended before it could be interrupted" >&2; exit 1; }
  sleep 0.2
done
sleep 2
pkill -TERM -f "waxwing restore" || true
wait $pid || true
echo "interrupted with $(psql b "select count(*) from $(nsp b).ping_event") of $((ROWS + 60)) pings loaded"

restore_b | grep -E "resuming|after vid|restored in"
check "tables, fulltext, indexes and head" "$(summary a)" "$(summary b)"

spawn; pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
compare "$(head_block)"
exit $fail
