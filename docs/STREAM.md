# glider-stream

Continuous replication for glider databases, built to Litestream's shape: a
daemon that watches database files and streams committed bytes to object
storage, plus a restore that puts them back.

Still zero dependencies. The S3 client — SigV4 signing, SHA-256, HMAC, HTTP —
is about 600 lines of `std`, verified in tests against the published FIPS and
RFC 4231 vectors, AWS's own worked example for signing-key derivation, and an
integration test that runs against a server which recomputes every signature
independently in Python and rejects mismatches the way S3 does.

## Quickstart

```sh
# one database, one replica, no config file
glider-stream replicate app.gldb /var/backups/app

# to MinIO
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=…
glider-stream replicate app.gldb s3://graphs/app -endpoint http://minio.internal:9000

# put it back
glider-stream restore -o recovered.gldb s3://graphs/app -endpoint http://minio.internal:9000
glider-stream restore -o yesterday.gldb -timestamp -24h /var/backups/app
```

## Configuration

`/etc/glider-stream.yml` by default, `-config PATH` otherwise. `$VAR` and
`${VAR}` are expanded before parsing unless you pass `-no-expand-env`.

```yaml
addr: ":9090"                   # Prometheus metrics endpoint

snapshot:                       # global defaults
  interval: 1h
  retention: 24h

dbs:
  - path: /var/lib/mes/app.gldb
    replicas:
      - url: s3://graphs/mes
        endpoint: http://minio.internal:9000
        region: us-east-1
        access-key-id: $MINIO_ACCESS_KEY
        secret-access-key: $MINIO_SECRET_KEY
        sync-interval: 1s       # ship at least this often
        min-bytes: 4mb          # ship immediately at this much pending
        retention: 72h

  - path: /var/lib/mes/audit.gldb
    replica:                    # singular form, as in Litestream 0.5
      url: /mnt/nas/backups/audit
```

Durations are `30s`, `5m`, `2h`, `7d`. Sizes are `512`, `64kb`, `4mb`, `1gb`.
Tabs in the config are an error rather than a guess.

## Commands

| Command | What it does |
|---|---|
| `replicate` | daemon: every database in the config, to every replica |
| `replicate DB URL` | one database, no config file |
| `replicate -once` | ship what is pending and exit — for cron |
| `replicate -force-snapshot` | snapshot even if one is not due |
| `replicate -enforce-retention` | run retention this pass |
| `restore -o OUT URL` | rebuild a database |
| `restore -timestamp T` | as of a unix time, or `-30m`, `-2h`, `-7d` |
| `restore -generation HEX` | a specific lineage |
| `restore -if-replica-exists` | exit 0 when the replica is empty |
| `databases` | what the config covers |
| `generations` | lineages in a replica, with completeness and last activity |
| `segments` / `snapshots` | what is actually stored |
| `status` | committed vs replicated vs lag, per database |
| `sync` | one pass over the config, then exit |
| `version` | |

## How it stores things

```text
<generation>/segments/<offset:016x>-<unix>.seg     bytes [offset, offset+len)
<generation>/snapshots/<offset:016x>-<unix>.snap   bytes [0, offset)
```

The timestamp is in the key, not in object metadata, because `LastModified`
changes when an object is copied or lifecycle-transitioned and a point-in-time
restore that silently lands on the wrong hour is worse than one that fails.

**A snapshot is a prefix.** Litestream snapshots the SQLite file and ships WAL
frames relative to it. Glider's file *is* a log, so a snapshot is just the
first N bytes as one object — which means restore is concatenation, with no
replay path to get wrong, and segments below a snapshot can be deleted because
the snapshot contains them.

**Retention deletes only what is provably redundant:** segments wholly below
the newest snapshot, older snapshots (never the newest), and whole generations
that have aged out — and only once the live generation has a snapshot of its
own to restore from. Nothing inside the window goes.

What retention cannot do is shrink the data. That needs a `COMPACT` by the
writing process, which mints a new generation; retention then expires the old
one on its next pass. Replication will not do it for you, because doing it
would mean opening the file as a second writer.

## S3, TLS, and the exec escape hatch

The native S3 client speaks plaintext HTTP. That covers MinIO, Ceph RGW,
SeaweedFS, Garage and LocalStack on a private network, which is where most
self-hosted object storage actually lives.

It does not speak TLS, and it never will — a hand-rolled TLS stack would be
strictly worse than a dependency, and a dependency would end the property that
makes this codebase portable to everything rustc targets. For AWS proper, or
anything reached across the public internet, hand the bytes to a tool that
already has a TLS stack:

```yaml
      - url: exec:s3://my-bucket/mes
        exec-put: aws s3 cp {path} s3://my-bucket/mes/{key}
        exec-get: aws s3 cp s3://my-bucket/mes/{key} {path}
        exec-list: aws s3 ls --recursive s3://my-bucket/mes/
        exec-delete: aws s3 rm s3://my-bucket/mes/{key}
```

If the URL is `s3://…` with no plaintext endpoint and `aws` is on PATH, that
configuration is assumed automatically. The same pattern reaches GCS (`gsutil`),
Azure (`az storage blob`), Backblaze, rclone remotes, or anything else with a
CLI — and it is where encryption belongs, too: `age -r … {path}` on the way
out, `age -d` on the way back.

## Metrics

Set `addr` and scrape `/metrics`:

```
glider_segments_total{db="…",replica="…"}
glider_bytes_total
glider_snapshots_total
glider_deletions_total
glider_errors_total
glider_lag_bytes
glider_last_sync_timestamp_seconds
```

`glider_lag_bytes` is the one to alert on: committed bytes not yet replicated.

## Parity with Litestream

| Litestream | glider-stream |
|---|---|
| `replicate` daemon, many DBs, many replicas | yes |
| `-once`, `-force-snapshot`, `-enforce-retention` | yes |
| `restore` with `-o`, `-timestamp`, `-generation`, `-if-replica-exists` | yes |
| `databases`, `status`, `sync`, `version` | yes |
| `ltx` / `wal` listings | `segments`, `snapshots`, `generations` |
| YAML config, env expansion, `/etc` default | yes |
| Generations, snapshots, retention windows | yes |
| Prometheus metrics | yes |
| S3-compatible storage | native over plaintext; TLS via `exec` |
| GCS, Azure, SFTP, NATS | via `exec` only |
| LZ4 compression of segments | **no** — do it in `exec-put` |
| age encryption | **no** — do it in `exec-put` |
| `-txid` restore targets | **no** — glider has byte offsets, not TXIDs |
| `-restore-if-db-not-exists` on replicate | **no** |
| `reset`, `register`/`unregister`/`start`/`stop` | **no** |
| IPC socket, MCP server, read-only VFS | **no** |
| Checkpoint management, `busy-timeout` | **N/A** — no checkpoints to manage |

The gaps are listed because a backup tool that overstates itself is worse than
one that does less. The two worth closing first are compression, which is real
money on a large log, and `-restore-if-db-not-exists`, which is what makes
container startup a one-liner.

## What it still is not

One writer per file. No leader election, no lock file, no fencing. Two
processes writing one glider database corrupt it, and replication does not
change that — it only means you will have a good copy of the moment before
you did it.
