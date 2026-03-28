# Quark Dispatch Points

Add `target_os = "quark"` to these cfg_select! blocks in `library/std/src/sys/`.
Follow the motor pattern exactly — add quark as a peer branch.

## New files to copy

Copy all `quark.rs` files from this directory into the matching locations
in the forked `library/std/src/sys/` tree. Also copy `pal/quark/` directory.

## Cargo.toml

In `library/std/Cargo.toml`, add alongside the moto-rt dependency:

```toml
[target.'cfg(target_os = "quark")'.dependencies]
quark-rt = { version = "0.1", features = ['rustc-dep-of-std'], public = true }
```

## Dispatch modifications (add a quark branch to each cfg_select!)

### pal/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use self::quark::*;
}
```

### stdio/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### alloc/mod.rs
```rust
target_os = "quark" => {
    mod quark;
}
```

### fs/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    use quark as imp;
}
```

### fd/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### net/connection/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### process/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    use quark as imp;
}
```

### pipe/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::{Pipe, pipe};
}
```

### args/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### env/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### thread/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### time/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### io/error/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### io/is_terminal/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::*;
}
```

### random/mod.rs
```rust
target_os = "quark" => {
    mod quark;
    pub use quark::fill_bytes;
}
```

## Grouping modifications (add quark to existing any() groups)

### sync/mutex/mod.rs — add to futex group
Add `target_os = "quark"` inside the `any(...)` that includes `target_os = "motor"`.

### sync/condvar/mod.rs — add to futex group
Same as mutex.

### sync/rwlock/mod.rs — add to futex group
Same as mutex.

### sync/once/mod.rs — add to futex group
Same as mutex.

### sync/thread_parking/mod.rs — add to futex group
Same as mutex.

### os_str/mod.rs — add to UTF-8 group
Add `target_os = "quark"` inside the `any(...)` that includes `target_os = "motor"` (UTF-8 branch).

### path/unix.rs — add quark to unix-style paths
Add `target_os = "quark"` to the cfg that determines unix-style path handling.

### personality/mod.rs — add to aborting stub group
Add `target_os = "quark"` inside the `any(...)` that includes `target_os = "motor"`.

### thread_local/mod.rs — use racy/static key
Add `target_os = "quark"` to use `racy` (static key) thread-local storage,
since Quark doesn't have OS-backed TLS yet.

### exit.rs — add quark branch
```rust
target_os = "quark" => {
    quark_rt::rt::exit(code)
}
```

### io/mod.rs — add default buf size
Add `target_os = "quark"` to the group that uses the default 8 KiB buffer.
