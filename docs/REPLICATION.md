# Replication and durability

## Memory, disk, or both

| You want | Open it as | Notes |
|---|---|---|
| Scratch graph, nothing persisted | `:memory:` | `Graph::memory()`, `glider_open_memory()`; cap it with `--max-memory` |
| Durable, normal case | `file.gldb --sync normal` | survives process death; not power loss |
| Durable through power loss | `--sync always` | fsync per commit |
| A durable copy elsewhere | any file + `wal tail` | see below |

## How it works

A database is pages (the file, and segment files in `<db>-data/`) plus a
write-ahead log (`<db>-wal/`). Every commit appends the transaction's
operations and a commit marker to the log. A checkpoint writes the pages the
log describes, flips the superblock, and deletes the log it covered.

A replica is **base snapshots plus the log shipped after them**:

```
/var/backups/glider/
  96348bfb24205901.../                 <- generation (one per database)
    base/
      0000000000000059/                <- the pages of one checkpoint, covering log up to 0x59
        db  data/00000001.seg  base.json
    segments/
      0000000000000059.seg             <- log bytes from 0x59, whole transactions
      0000000000013f20.seg
    manifest.jsonl                     <- when each segment was shipped
```

Restoring is: copy the newest base, lay the log shipped after it beside it,
open. Opening replays that log, through the same code that ran it the first
time.

The tailer is a separate process and never blocks the writer. Three rules
keep it correct:

**Only ship whole transactions.** The tailer scans the log for the last
commit marker, checking every frame's CRC, and ships up to there; a frame
still being written fails its CRC and ends the scan. What it copied is
checked again before the segment is published.

**The writer keeps the log that has not been shipped.** The tailer leaves a
pin file (`<db>-wal/replica.pin`) saying how far it has shipped. The writer
reads it at most once a second, on commit, and a checkpoint deletes only log
the tailer already has. A pin not refreshed for 10 minutes is ignored, so a
dead tailer cannot make the log grow forever; if the log it needs is gone
when it comes back, it takes a new base and carries on.

**A base is copied from a checkpoint that stays put.** Copying 100 TB takes a
while, and meanwhile the writer keeps writing and checkpointing. Pages are
copy-on-write, so a checkpoint's pages are never overwritten in place — but
once a later checkpoint frees them they can be reused. So the tailer asks, in
the pin, for a *hold*. At its next commit the writer checkpoints, and from
then on keeps every page freed after that checkpoint out of reuse (listed in
the free list, marked held) until the tailer says it is done. The tailer
copies that checkpoint's superblock and pages, verifies the hold lasted the
whole copy, then releases it. With no writer running, the tailer takes the
database's lock instead and copies directly.

`glider serve` looks at the pin every second even when idle. An embedding
application that may sit idle for long periods calls
`Graph::poll_replication()` from time to time, or a base waits for its next
commit.

## Shipping to S3, MinIO, or anything else

Glider does not speak S3: an HTTP client, TLS and SigV4 would be its first
dependencies. The tailer writes to a directory and runs a command for each
base and log segment:

```sh
glider app.gldb wal tail --to /var/spool/glider \
  --interval 10 --min-bytes 1048576 \
  --exec 'aws s3 cp --recursive {path} s3://my-bucket/glider/{gen}/{kind}/{name}'
```

Placeholders: `{path} {name} {gen} {kind} {offset} {len}`, where `{kind}` is
`base` (a directory) or `segments` (a file). A non-zero exit stops the
tailer rather than silently dropping something.

| Flag | Default | Effect |
|---|---|---|
| `--interval S` | 10 | ship at least this often, however little has changed |
| `--min-bytes N` | 1 MiB | ship immediately once this much is pending |
| `--once` | off | ship what exists and exit — for cron (a pin older than 10 minutes lapses, so run it more often than that or expect a fresh base) |
| `--quiet` | off | no progress on stderr |

Lag is `--interval` in the worst case.

`glider-stream`, the daemon with built-in S3 and HTTP backends, replicates
databases from before paged storage; for paged databases use `wal tail
--exec`.

## Restoring

```sh
glider wal verify --from /var/spool/glider
# 96348bfb...  2 bases (newest at log 89), 14 log segments, 1048576 bytes, log complete to 1048665
#     last activity 2026-09-27 00:07:43Z

glider wal restore --from /var/spool/glider --to recovered.gldb
# restored generation 96348bfb... from the base at log 89 (2026-09-27 00:07:43Z) and 14 log segments, through log 1048665
# 1247371 nodes, 8027661 edges
```

Restore refuses to overwrite an existing file, stops at a gap in the log
rather than producing a plausible-looking wrong database, replays the log,
checks every tree, and reports the counts. A restore that does not open is
not a restore.

Point in time, to the segment:

```sh
glider wal restore --from /var/spool/glider --to before.gldb --as-of -30m
glider wal restore --from /var/spool/glider --to before.gldb --as-of 1757692800
```

It takes the newest base taken at or before that time and the log shipped up
to it.

```sh
glider app.gldb wal status --to /var/spool/glider
# committed   log to 1048665
# base        at log 89 (2026-09-27 00:07:43Z), 2 bases
# replicated  log to 1048665 in 14 segments
# lag         0 bytes
```

To restore from object storage, pull the generation down and restore
locally; keep the directory layout, since offsets live in the names.

## Databases from before paged storage

Files written by earlier versions (an append-only log with a snapshot image)
are replicated as they always were: the file itself, in byte ranges, a new
lineage per compaction. `wal restore` recognises such a replica and restores
the file, which `glider <file> migrate` then converts.

## What this does not do

- **No leader election, no consensus.** One writer, enforced by a lock file.
- **No live read replicas.** A restore is a point-in-time copy.
- **Segment granularity for point-in-time restore.** Lower `--interval` to
  tighten it.
- **Full bases only.** A new base copies every page; incremental bases
  (only pages changed since the last) are future work.
- **No encryption.** Encrypt in the exec hook (`age`, `gpg`, SSE-KMS).
