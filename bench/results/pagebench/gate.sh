#!/usr/bin/env bash
# P1 gate: the storage engine alone at 100 GB with a 1 GiB page cache.
set -uo pipefail
cd "$(dirname "$0")"
P=../../glider/target/release/pagebench
DB=gate100.db
OUT=gate.jsonl
evict() { python3 -c '
import os,sys,glob
for f in [sys.argv[1]] + glob.glob(sys.argv[1]+"-data/*.seg"):
    fd=os.open(f,os.O_RDONLY); os.fsync(fd); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED); os.close(fd)
' "$DB"; }
rm -rf $DB $DB-wal $DB-data $DB.lock
: > $OUT
echo "== build" >&2
$P build --db $DB --gb ${GB:-100} --cache-mb 1024 | tee -a $OUT
evict
echo "== lookups cold" >&2
$P lookups --db $DB --cache-mb 1024 --n 20000 | sed 's/"cmd":"lookups"/"cmd":"lookups_cold"/' | tee -a $OUT
echo "== lookups warm" >&2
$P lookups --db $DB --cache-mb 1024 --n 300000 | sed 's/"cmd":"lookups"/"cmd":"lookups_warm"/' | tee -a $OUT
evict
echo "== scan cold" >&2
$P scan --db $DB --cache-mb 1024 | tee -a $OUT
echo "== update" >&2
$P update --db $DB --cache-mb 1024 --txns 3000 --per 50 | tee -a $OUT
echo "== lookups after updates" >&2
$P lookups --db $DB --cache-mb 1024 --n 100000 | sed 's/"cmd":"lookups"/"cmd":"lookups_after_update"/' | tee -a $OUT
echo done >&2
