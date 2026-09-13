# Shipping glider on five platforms

The engine is `std`-only Rust with no C dependencies, no `mmap`, no `fork`, no
platform APIs. So the port is not a port: it is a packaging exercise. Every
target below type-checks from the same source tree today:

```
aarch64-linux-android      ok      aarch64-apple-ios        ok
armv7-linux-androideabi    ok      x86_64-pc-windows-gnu    ok
wasm32-unknown-unknown     ok      x86_64-unknown-linux-musl  builds + tests
```

What actually differs per platform is the artifact you ship and the lifecycle
rules you obey.

| Platform | Artifact | Entry point |
|---|---|---|
| Linux / macOS / Windows | `glider` binary, or `libglider.{so,dylib,dll}` | CLI, or C ABI |
| iOS / iPadOS | `libglider.a` in an `.xcframework` | C ABI from Swift |
| Android | `libglider.so` per ABI in `jniLibs/` | C ABI via JNI |
| Browser / RN | `wasm32-unknown-unknown` | in-memory only |

`scripts/build-all.sh` produces all of them. `include/glider.h` is the contract.

## The C ABI

Thirteen functions, SQLite-shaped: open, query, free, close. `src/ffi.rs` is
the whole layer — it catches panics at the boundary and converts them to
errors, because unwinding across an FFI edge is undefined behaviour and a
query parser bug should not crash someone's app.

```c
glider_db *db = glider_open("/path/to/graph.gldb", GLIDER_SYNC_NORMAL);
char *json = glider_query(db, "MATCH (n:Person) RETURN n.name LIMIT 10");
// {"columns":["n.name"],"rows":[["Ada"]],"touched":1}
glider_free(json);
glider_checkpoint(db);
glider_close(db);
```

A handle is not thread-safe. One per thread, or wrap it — the graph is mutated
in place, so sharing it unsynchronised is a data race, not merely a stall.

## iOS

Build a static library and wrap it in an XCFramework; Apple's toolchain wants
the `.a`, and static linking avoids the dynamic-framework signing dance.

```sh
./scripts/build-all.sh ios     # requires a Mac with Xcode
```

Then in Swift, with a bridging header that includes `glider.h`:

```swift
final class Glider {
    private let db: OpaquePointer

    init(filename: String) throws {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory,
                                           in: .userDomainMask)[0]
        let url = dir.appendingPathComponent(filename)
        guard let handle = glider_open(url.path, GLIDER_SYNC_NORMAL) else {
            throw GliderError.open(String(cString: glider_last_error()))
        }
        db = OpaquePointer(handle)
    }

    func query(_ q: String) throws -> Data {
        guard let out = glider_query(db, q) else {
            throw GliderError.query(String(cString: glider_last_error()))
        }
        defer { glider_free(out) }
        return Data(String(cString: out).utf8)
    }

    func checkpoint() { glider_checkpoint(db) }
    deinit { glider_close(db) }
}
```

Three things will bite you, in order of how long they take to debug:

**Data protection.** By default iOS encrypts files with
`NSFileProtectionCompleteUntilFirstUserAuthentication`, but if your app or MDM
profile raises it to `...Complete`, every read and write fails with `EPERM`
while the device is locked — including from a background task. Set it
explicitly on the database file, and decide deliberately:

```swift
try FileManager.default.setAttributes(
    [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication],
    ofItemAtPath: url.path)
```

**Suspension.** A backgrounded app gets SIGKILLed with no further callback.
Under `GLIDER_SYNC_NORMAL` the last commits are in the OS page cache, which
survives that — but not a reboot or a power loss. Call `glider_checkpoint` from
`sceneDidEnterBackground`. Wrap `glider_compact` in
`beginBackgroundTask`/`endBackgroundTask` so you are not suspended mid-rewrite.
(Even if you are, compaction is crash-safe: temp file, fsync, atomic rename.)

**Location.** Put the file in Application Support, not Documents, unless you
want users deleting it in the Files app. Exclude it from iCloud backup
(`isExcludedFromBackupKey`) if it is a rebuildable cache — Apple rejects apps
that back up large derived data.

## Android

```sh
cargo install cargo-ndk
export ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.3.11579264
./scripts/build-all.sh android
```

That emits `arm64-v8a`, `armeabi-v7a` and `x86_64` (emulator) `.so` files into
`dist/jniLibs`. Point Gradle at it:

```kotlin
android {
    sourceSets["main"].jniLibs.srcDirs("../glider/dist/jniLibs")
    ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
}
```

Kotlin cannot call a C function directly. Two ways across:

- **JNA** — add `net.java.dev.jna:jna:5.14.0@aar`, declare the interface in
  Kotlin, done in an afternoon. Per-call overhead is a few microseconds, fine
  when a call runs a whole query but wasteful in a tight loop.
- **A JNI shim** — a second crate (`bindings/android`) depending on the `jni`
  crate and on `glider`, exporting `Java_com_you_Glider_query`. Keeps the core
  dependency-free while giving you zero-marshalling calls. This is what I would
  ship.

Use `context.filesDir` for the path. Checkpoint in `onStop()`, not
`onDestroy()` — `onDestroy` is not guaranteed to run.

The one current gotcha: **Android 15 uses 16 KB memory pages** on new devices,
and a `.so` linked with 4 KB alignment will not load at all. The build script
passes `-Wl,-z,max-page-size=16384` already; if you build by hand, don't drop it.

## Windows

`cargo build --release --target x86_64-pc-windows-msvc` and you have
`glider.exe` plus `glider.dll`. Nothing in the engine is Unix-specific. Two
details already handled in `store.rs`:

- Rust opens files with `FILE_SHARE_DELETE`, so compaction's rename-over-the-
  open-file works — but the old handle is stale afterwards, so the store
  reopens the file immediately after the rename.
- A virus scanner or the search indexer can hold a transient handle and make
  the rename fail with `ACCESS_DENIED` for a few milliseconds. The rename
  retries with backoff instead of failing the compaction.

## Browser / React Native

`wasm32-unknown-unknown` compiles, with no filesystem — use `Graph::memory()`
and `import_jsonl`/`export_jsonl` to move state in and out, persisting the dump
wherever the host keeps data. `wasm32-wasip1` gets you real file I/O under a
WASI runtime. Neither is wired up with bindings yet; the FFI layer is the model
to copy.

## The constraint that actually matters on mobile

The whole graph lives in RAM while open. That is the design — it is what makes
the algorithms fast — but on a phone it is a budget, not an afterthought.
Roughly: a node costs ~120 bytes plus its properties, an edge ~80. A million
nodes and four million edges lands near 450 MB, which Android will kill you
for. Practical ceilings:

| Device budget | Comfortable graph |
|---|---|
| Android, low-end (~100 MB) | ~150k nodes / 600k edges |
| iOS, foreground (~500 MB) | ~1M nodes / 4M edges |
| Desktop | tens of millions of edges |

If you need more on-device, the honest answer is that glider's storage model
has to change — a paged B-tree with an LRU buffer pool instead of
replay-into-memory. That is a real rewrite of `store.rs` and `graph.rs`, not a
flag. Before doing it, check whether you can shard by subgraph and open one at
a time, which is usually true for per-user or per-repo graphs.

Interning helps already: labels, edge types and property keys are stored once
as `u32` ids, so a million nodes with the same schema pay for the schema once.
