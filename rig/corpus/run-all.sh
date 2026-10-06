#!/usr/bin/env bash
# Every deployment in corpus/deployments.txt through corpus.sh, one at a
# time; a line each in work/corpus/summary.txt, the full log beside it.
cd "$(dirname "$0")/.."
mkdir -p work/corpus
grep -vE '^\s*(#|$)' corpus/deployments.txt | while read -r hash network blocks _; do
  [[ "$blocks" =~ ^[0-9]+$ ]] || blocks=""
  grep -q " $hash " work/corpus/summary.txt 2>/dev/null && continue
  log=work/corpus/$hash.log
  ./corpus.sh "$hash" "$network" $blocks > "$log" 2>&1 < /dev/null
  code=$?
  verdict=$(grep -E "^(FAILED|DEPLOY FAILED|MISMATCH|NO DATA)" "$log" | head -1)
  [ $code = 0 ] && verdict="pass: $(grep -E '^compared' "$log"), cut $(grep -E '^cut to' "$log" | sed 's/^cut to //')"
  echo "$(date -u +%FT%TZ) $hash $network exit=$code ${verdict:-$(tail -1 "$log")}" >> work/corpus/summary.txt
done
