# Replication and durability

## Memory, disk, or both

| You want | Open it as | Notes |
|---|---|---|
| Scratch graph, nothing persisted | `:memory:` | `Graph::memory()`, `glider_open_memory()` |
| Durable, normal case | `file.gldb --sync normal` | survives process death; not power loss |
| Durable through power loss | `--sync always` | fsync per commit |
| Memory speed *and* a durable copy elsewhere | file on `/dev/shm` + `wal tail` | see below |

That last row is the interesting one. Because replication reads the file rather
than the process, you can put the database on a tmpfs — memory, as far as the
kernel is concerned — and still have every committed byte shipped off-box
within a second. You get in-memory write performance with off-host durability,
and you lose only the unshipped tail if the machine dies.

```sh
glider /dev/shm/live.gldb --sync off -c "..."          # writes never touch a disk
glider /dev/shm/live.gldb wal tail --to /var/backups/glider --interval 1
```

## Why there is no separate WAL file

SQLite keeps a main database of fixed-size pages that get rewritten in place,
so it needs a *separate* write-ahead log, and Litestream exists to tail that
log before a checkpoint overwrites it.

Glider has no pages and rewrites nothing. The database file **is** the
write-ahead log: every mutation is a CRC-framed record appended at the end, and
a transaction ends with a commit marker. Which means replication is not a
protocol, it's byte ranges. A replica is your database, in pieces:

```
/var/backups/glider/
  c46f16170b58b63f.../            <- generation
    segments/
      0000000000000000.seg        <- bytes [0, 172), header included
      00000000000000ac.seg        <- bytes [172, 209)
      00000000000000d1.seg
    manifest.jsonl
```

`cat segments/*.seg > restored.gldb` is a genuine restore. That is not a
coincidence to be papered over, it's the property that makes the whole thing
auditable: there is no format to trust beyond the one you already have.

Two rules make it safe.

**Only ship to a transaction boundary.** The writer may be mid-transaction when
the tailer looks. `store::scan_committed_end` walks frames, verifies CRCs, and
returns the offset after the last commit marker; bytes past that are not
shipped. A torn tail stops the scan instead of being replicated.

**A compaction starts a new generation.** `COMPACT` rewrites the file from
scratch, so every existing offset becomes meaningless — this is exactly the
hazard Litestream's generations exist for. Glider puts a 16-byte generation id
in the file header and mints a fresh one on every compaction. The tailer sees
it change and starts a new lineage rather than appending onto a log that no
longer exists. Old generations stay restorable until you delete them.

## Streaming to S3, MinIO, or anything else

Glider does not speak S3. An HTTP client, TLS and SigV4 would be the first
dependencies in the codebase, and they would be dependencies on a moving target.
Instead the tailer writes a segment and runs a command:

```sh
glider app.gldb wal tail --to /var/spool/glider \
  --interval 10 --min-bytes 1048576 \
  --exec 'aws s3 cp {path} s3://my-bucket/glider/{gen}/{name}'
```

Placeholders: `{path} {name} {gen} {offset} {len}`. A non-zero exit fails the
tailer loudly rather than silently dropping a segment.

For MinIO, `mc` is the natural fit:

```sh
--exec 'mc cp {path} minio/graphs/{gen}/{name}'
```

Anything that moves a file works the same way — `rclone copyto`, `restic
backup`, `scp`, a script that also writes a row to a tracking table. The
segments are immutable once written, which is what makes this safe: a retry is
always idempotent, and object storage lifecycle rules can expire whole
generations without coordination.

Flags worth knowing:

| Flag | Default | Effect |
|---|---|---|
| `--interval S` | 10 | ship at least this often, however little has changed |
| `--min-bytes N` | 1 MiB | ship immediately once this much is pending |
| `--once` | off | ship what exists and exit — for cron |
| `--quiet` | off | no progress on stderr |

Lag is `--interval` in the worst case. Set it to what you can afford to lose.

## Restoring

```sh
glider wal verify --from /var/spool/glider
# c46f1617...  4 segments, 283 bytes, complete to 283
#     2026-09-12 20:57:13Z .. 2026-09-12 20:57:15Z

glider wal restore --from /var/spool/glider --to recovered.gldb
# restored 283 bytes from 4 segments of generation c46f1617...
# 6 nodes, 1 edges
```

Restore refuses to overwrite an existing file, refuses to start from a missing
first segment, stops at any gap rather than producing a plausible-looking
corrupt file, and opens the result before declaring success. A restore that
does not open is not a restore.

Point in time, to the segment:

```sh
glider wal restore --from /var/spool/glider --to before.gldb --as-of -30m
glider wal restore --from /var/spool/glider --to before.gldb --as-of 1757692800
```

Note that `--as-of` picks within one generation, defaulting to the most
recently active one. To recover to a moment *before* a compaction, name the
older generation explicitly — `wal verify` prints the time range of each.

Checking on things:

```sh
glider app.gldb wal status --to /var/spool/glider
# committed   283 bytes
# replicated  283 bytes in 4 segments
# lag         0 bytes
```

## Restoring from object storage

Pull the generation down, then restore locally:

```sh
aws s3 sync s3://my-bucket/glider/ ./replica/
glider wal restore --from ./replica --to recovered.gldb
```

The layout on the remote is the same as on disk because the exec hook preserves
`{gen}/{name}`. If you flatten it, keep the `.seg` names — the offsets live in
the filenames, and that is what makes gaps detectable.

## Upgrading an existing database

Generation ids landed in format v2. A v1 file still opens and reads normally,
but cannot be replicated, because there is no way to tell one lineage from
another. Run a compaction once:

```sh
glider old.gldb compact     # rewrites as v2 with a fresh generation
```

## What this does not do

- **No leader election, no consensus.** One writer. If two processes open the
  same file for writing, they will corrupt it — that is true of SQLite without
  its locking too, and glider has no lock file yet.
- **No live read replicas.** A restored file is a point-in-time copy, not a
  follower that stays current. You could poll-restore, but there is no
  streaming reader.
- **Segment granularity, not transaction granularity.** PITR lands on a segment
  boundary. Lower `--interval` to tighten it.
- **No encryption.** Segments are raw database bytes. If the destination is not
  trusted, encrypt in the exec hook (`age`, `gpg`, SSE-KMS on the bucket).
