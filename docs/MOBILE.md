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

A handful of functions, SQLite-shaped: open, query, free, close. `src/ffi.rs` is
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
`sceneDidEnterBackground`; a checkpoint writes only the pages changed since
the last one. Even if you are suspended mid-checkpoint nothing is lost: the
superblock that names the new pages is written last, so a crash leaves the
previous checkpoint plus the write-ahead log, which the next open replays.

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
`glider.exe` plus `glider.dll`. Nothing in the engine is Unix-specific: pages
are read and written with positional I/O (`seek_read`/`seek_write` on
Windows), and the database is never renamed over while open. (Converting a
pre-paged file with `glider <db> migrate` does rename, and retries with
backoff when a virus scanner holds a transient handle.)

## Browser / React Native

`wasm32-unknown-unknown` compiles, with no filesystem: graphs are `:memory:`
page stores, and `import_jsonl`/`export_jsonl` move state in and out, to be
persisted wherever the host keeps data. `Graph::from_bytes` loads the pages of
a database file (as of its last checkpoint) into memory. `wasm32-wasip1` gets
you real file I/O under a WASI runtime. The TypeScript bindings in `ts/`
wrap the wasm build.

## Memory on a phone

Storage is paged, as in SQLite: the database lives in the file and RAM holds
a page cache of a size you choose, however large the graph grows. Opening
reads a superblock, so it takes milliseconds at any size.

```c
glider_db *db = glider_open_ex(path, GLIDER_SYNC_NORMAL, 8 << 20); /* 8 MB cache */
```

A smaller cache means more reads from flash for the same query; point
lookups and short traversals stay in the low milliseconds even when almost
nothing is cached. Algorithms (`CALL pagerank(...)` and the rest) work within
the working memory and spill beyond it to temp files next to the database.

Writes go to a write-ahead log and are folded into the pages by a checkpoint:
automatically once the log passes 256 MiB (`glider_set_auto_compact` sets the
threshold), on `glider_checkpoint`, and on close. Checkpointing inside a
commit on the UI thread can stall it, so on mobile set a lower threshold and
checkpoint from a background task when the app backgrounds.

An in-memory graph can be capped, so a cache that grows too large fails
cleanly instead of getting the app killed:

```c
glider_db *scratch = glider_open_memory_ex(64 << 20); /* at most 64 MB */
```

Past the cap, the write that would exceed it fails with an error and is
rolled back; the graph stays usable, as SQLite does with `SQLITE_FULL`.

Interning helps already: labels, edge types and property keys are stored once
as `u32` ids, so a million nodes with the same schema pay for the schema once.
