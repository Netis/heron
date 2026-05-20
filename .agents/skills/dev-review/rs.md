---
version: 1.0.0
description: "Rust code review for best practices (clippy, ownership, error handling, async). Usage: /dev-review rs"
---

# Code Review for Rust Best Practices

Review Rust code for best practices, fix issues, and ensure quality.

> **Note**: Quality checks (clippy, fmt) are handled by `/dev-review` orchestrator via `just dev fmt` and `cargo clippy`. This skill focuses on language-specific patterns.

## Step 1: Use Semantic Tools for Code Analysis

**Use LSP tools** for efficient semantic analysis:

| Tool | Use Case |
|------|----------|
| `documentSymbol` | Get file structure (structs, enums, impls, functions) |
| `goToDefinition` | Navigate to trait/type definitions |
| `findReferences` | Find all usages of a type or function |
| `hover` | Get type info and trait bounds without reading entire files |

**Fallback to Grep/Glob** for pattern search across codebase.

**Benefits:**

- Reduces token usage vs reading full files
- Symbol-level precision for edits
- Cross-file reference tracking

## Step 2: Best Practices Review

### 2.1 Ownership, Borrowing & Lifetimes

| Check | Issue | Fix |
|-------|-------|-----|
| Unnecessary cloning | `.clone()` when borrow suffices | Use references or `Cow<'_, T>` |
| Excessive `to_string()` | Converting `&str` to `String` unnecessarily | Accept `&str` or `impl AsRef<str>` |
| Lifetime elision | Explicit lifetimes where elision applies | Remove unnecessary lifetime annotations |
| Owned in signatures | `fn foo(s: String)` when read-only | Use `fn foo(s: &str)` |
| Large struct moves | Moving large structs by value | Use `Box<T>` or pass by reference |

**Example fix:**

```rust
// Bad - unnecessary clone
fn process(items: &[Item]) {
    let name = items[0].name.clone();
    println!("{name}");
}

// Good - borrow instead
fn process(items: &[Item]) {
    let name = &items[0].name;
    println!("{name}");
}

// Bad - takes ownership unnecessarily
fn greet(name: String) {
    println!("Hello, {name}");
}

// Good - borrows
fn greet(name: &str) {
    println!("Hello, {name}");
}
```

### 2.2 Error Handling

| Check | Issue | Fix |
|-------|-------|-----|
| Bare `unwrap()` | Panics on None/Err | Use `?`, `unwrap_or`, `unwrap_or_else`, or `expect()` |
| `expect()` without context | Generic panic message | Add descriptive message: `expect("failed to parse config")` |
| Stringly-typed errors | `Err(String)` | Use `thiserror` or custom error enums |
| Swallowed errors | `let _ = fallible_fn();` | Log or propagate the error |
| `unwrap()` in library code | Panics leak to callers | Return `Result` or `Option` |
| Nested `Result` | `Result<Result<T, E1>, E2>` | Flatten with `and_then` or `?` |

**Example fix:**

```rust
// Bad - bare unwrap
let config = std::fs::read_to_string("config.toml").unwrap();
let value: Config = toml::from_str(&config).unwrap();

// Good - propagate with context
let config = std::fs::read_to_string("config.toml")
    .map_err(|e| anyhow::anyhow!("failed to read config.toml: {e}"))?;
let value: Config = toml::from_str(&config)
    .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
```

### 2.3 Unsafe Code

| Check | Issue | Fix |
|-------|-------|-----|
| Unnecessary `unsafe` | Safe alternative exists | Replace with safe code |
| Missing safety comments | `unsafe` block without `// SAFETY:` | Add `// SAFETY:` comment explaining invariants |
| Pointer arithmetic | Raw pointer manipulation | Use slice methods or `std::ptr` helpers |
| Unvalidated FFI input | External data used without checks | Validate at FFI boundary |
| Large `unsafe` blocks | Too much code in `unsafe` | Minimize scope to only the unsafe operation |

**Example fix:**

```rust
// Bad - unnecessary unsafe, no safety comment
unsafe {
    let ptr = data.as_ptr();
    let len = data.len();
    let slice = std::slice::from_raw_parts(ptr, len);
    process(slice);
}

// Good - no unsafe needed
process(&data);
```

### 2.4 Clippy & Common Anti-patterns

| Check | Issue | Fix |
|-------|-------|-----|
| `if let Some(x) = opt { x } else { default }` | Verbose pattern | Use `opt.unwrap_or(default)` |
| `match opt { Some(x) => ..., None => () }` | Verbose | Use `if let Some(x) = opt { ... }` |
| `&Vec<T>` in parameters | Overly specific | Use `&[T]` |
| `&String` in parameters | Overly specific | Use `&str` |
| `&Box<T>` in parameters | Overly specific | Use `&T` |
| `.iter().map().collect()` | Possibly unnecessary allocation | Consider `for` loop or iterator chain |
| `impl Clone` on types with `&mut` | Surprising semantics | Review if Clone is appropriate |
| Manual `Debug` impl | Boilerplate | Use `#[derive(Debug)]` |
| `to_string()` in format args | Redundant | Use `{}` format directly |
| Boolean parameters | `fn foo(enable: bool, verbose: bool)` | Use builder pattern or enums |

**Example fix:**

```rust
// Bad
fn process(items: &Vec<String>) {
    for item in items.iter() {
        println!("{}", item.to_string());
    }
}

// Good
fn process(items: &[String]) {
    for item in items {
        println!("{item}");
    }
}
```

### 2.5 Performance Considerations

| Check | Issue | Fix |
|-------|-------|-----|
| Unnecessary `collect()` | Collecting only to iterate again | Chain iterators directly |
| Repeated allocation | `String::new()` in loops | Reuse buffer with `clear()` |
| `Vec` growth | Many small pushes | Pre-allocate with `Vec::with_capacity()` |
| HashMap default hasher | Performance-sensitive lookups | Consider `ahash` or `FxHashMap` |
| String concatenation | `format!` in hot loops | Use `write!` to a buffer |
| Excessive `Arc<Mutex<T>>` | Lock contention | Consider `dashmap`, channels, or actor pattern |
| Large enum variants | One variant much larger | Box the large variant |
| Redundant `to_vec()` / `to_owned()` | Creating owned copy unnecessarily | Use slices or references |

**Example fix:**

```rust
// Bad - unnecessary collect
let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
for name in names {
    println!("{name}");
}

// Good - chain iterators
for name in items.iter().map(|i| &i.name) {
    println!("{name}");
}

// Bad - repeated allocation
for item in items {
    let msg = format!("Processing: {}", item.name);
    log::info!("{msg}");
}

// Good - reuse buffer
use std::fmt::Write;
let mut buf = String::new();
for item in items {
    buf.clear();
    write!(buf, "Processing: {}", item.name).unwrap();
    log::info!("{buf}");
}
```

### 2.6 Trait Usage & Design Patterns

| Check | Issue | Fix |
|-------|-------|-----|
| Trait object vs generics | `Box<dyn Trait>` when generics work | Use `impl Trait` or generics for static dispatch |
| Missing `#[typetag::serde]` | Decoder trait impl not registered | Add `#[typetag::serde]` attribute |
| God struct | Struct with too many fields/methods | Split into smaller focused types |
| Missing `From`/`Into` impls | Manual conversion code | Implement `From<T>` for ergonomic conversions |
| Missing `Default` | Constructor duplicates defaults | Derive or impl `Default` |
| Builder pattern | Many optional constructor args | Use builder pattern |

**Example fix:**

```rust
// Bad - manual conversion scattered in code
let entry = Entry {
    src_ip: packet.src_ip.to_string(),
    dst_ip: packet.dst_ip.to_string(),
    // ... many fields
};

// Good - implement From
impl From<&Packet> for Entry {
    fn from(packet: &Packet) -> Self {
        Self {
            src_ip: packet.src_ip.to_string(),
            dst_ip: packet.dst_ip.to_string(),
            // ...
        }
    }
}

let entry = Entry::from(packet);
```

### 2.7 Testing Patterns

| Check | Issue | Fix |
|-------|-------|-----|
| Missing `#[test]` | New logic without test | Add unit tests in `#[cfg(test)]` module |
| Test isolation | Tests share mutable state | Use unique test data or `serial_test` |
| PCAP test coverage | New parser without pcap test | Add integration test with pcap file |
| `assert!` with no message | Unclear failure | Use `assert!(cond, "descriptive message")` |
| Missing edge cases | Only happy path tested | Add tests for errors, empty input, boundaries |
| `RUST_TEST_THREADS=1` | Tests must run single-threaded | Enforced by justfile; ensure tests are compatible |

**Example fix:**

```rust
// Bad - no message on failure
#[test]
fn test_parse() {
    let result = parse("input");
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 5);
}

// Good - descriptive assertions
#[test]
fn test_parse_valid_input() {
    let result = parse("input").expect("parse should succeed for valid input");
    assert_eq!(result.len(), 5, "expected 5 parsed items from valid input");
}
```

### 2.8 Cargo Feature Flags

| Check | Issue | Fix |
|-------|-------|-----|
| Unconditional dependency | Feature-gated crate always compiled | Gate with `#[cfg(feature = "...")]` |
| Missing feature docs | Feature flag undocumented | Add doc comment in `Cargo.toml` |
| Feature leakage | Internal feature exposed publicly | Use `dep:crate_name` syntax |
| Default features | Too many defaults enabled | Keep defaults minimal, document opt-in |

**Project-specific features:**

```toml
# decoder features in this project
decoder-default = ["decoder-data", "decoder-http", "decoder-json", "decoder-regex", "decoder-xml"]
decoder-all = ["decoder-default", "decoder-http2", "decoder-protobuf", "decoder-wmq"]
```

**Example fix:**

```rust
// Bad - always compiled even without feature
use decoder_protobuf::ProtobufDecoder;

// Good - gated behind feature
#[cfg(feature = "decoder-protobuf")]
use decoder_protobuf::ProtobufDecoder;
```

### 2.9 Documentation

| Check | Issue | Fix |
|-------|-------|-----|
| Missing `///` on public items | Public API undocumented | Add doc comments |
| Missing `//!` module docs | Module purpose unclear | Add module-level docs |
| Outdated docs | Code changed, docs stale | Update docs to match code |
| Missing examples | Complex API without examples | Add `# Examples` section |
| `// TODO` without issue | Untracked work | Link to issue or resolve |

**Example fix:**

```rust
// Bad
pub fn decode(buf: &[u8]) -> Result<Message> { ... }

// Good
/// Decode a raw byte buffer into a structured `Message`.
///
/// # Errors
///
/// Returns an error if the buffer is too short or contains
/// an invalid protocol header.
pub fn decode(buf: &[u8]) -> Result<Message> { ... }
```

### 2.10 Concurrency Patterns (tokio & async/await)

| Check | Issue | Fix |
|-------|-------|-----|
| Blocking in async | `std::fs` or CPU work in async | Use `tokio::fs` or `spawn_blocking` |
| `Mutex` in async | `std::sync::Mutex` held across `.await` | Use `tokio::sync::Mutex` |
| Unbounded channels | `kanal::unbounded()` without backpressure | Use bounded channels with capacity |
| Missing `select!` timeout | Awaiting forever | Add timeout branch |
| `spawn` without `JoinHandle` | Detached task, lost errors | Store handle and `.await` it |
| Lock contention | Single `Mutex` bottleneck | Shard data or use lock-free structures |

**Example fix:**

```rust
// Bad - blocking in async context
async fn read_config() -> Result<Config> {
    let data = std::fs::read_to_string("config.toml")?;  // blocks runtime
    Ok(toml::from_str(&data)?)
}

// Good - use async fs
async fn read_config() -> Result<Config> {
    let data = tokio::fs::read_to_string("config.toml").await?;
    Ok(toml::from_str(&data)?)
}

// Good - spawn_blocking for CPU-heavy work
async fn parse_pcap(path: &Path) -> Result<Vec<Packet>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        parse_pcap_file(&path)
    }).await?
}
```

### 2.11 Security

| Check | Issue | Fix |
|-------|-------|-----|
| Hardcoded secrets | API keys, tokens in code | Use env vars or config files |
| Unchecked index | `buf[i]` without bounds check | Use `.get(i)` or validate length |
| Integer overflow | Arithmetic without checked ops | Use `checked_add`, `saturating_add` |
| Unvalidated input | External data used directly | Validate at ingestion boundary |
| Path traversal | User-controlled path components | Canonicalize and validate paths |

### 2.12 Duplicate Code Detection

| Check | Issue | Fix |
|-------|-------|-----|
| Copy-pasted blocks | 10+ similar lines | Extract to shared function |
| Similar impls | Same logic across types | Use generics or trait default methods |
| Repeated match arms | Same pattern in multiple matches | Extract to helper function |
| Boilerplate impls | Repeated `impl` blocks | Use derive macros or `impl_trait!` |

**Example fix:**

```rust
// Bad - repeated parsing logic
fn parse_tcp(buf: &[u8]) -> Result<TcpHeader> {
    if buf.len() < 20 { return Err(Error::TooShort); }
    // ... parse fields
}

fn parse_udp(buf: &[u8]) -> Result<UdpHeader> {
    if buf.len() < 8 { return Err(Error::TooShort); }
    // ... parse fields
}

// Good - shared validation
fn validate_min_len(buf: &[u8], min: usize) -> Result<()> {
    if buf.len() < min {
        return Err(Error::TooShort { expected: min, actual: buf.len() });
    }
    Ok(())
}
```

## Step 3: Apply Fixes

1. Make fixes directly to the code
2. Run `cargo +nightly fmt --all` to format
3. Run `cargo clippy --workspace` to verify no warnings
4. Run `just test probe all` to ensure unit tests pass
5. Run `just test integration all` to ensure integration tests pass

## Step 4: Report & Commit

Summarize what was fixed:

- List files modified
- Categories of fixes applied
- Any issues needing manual attention

If changes were made and tests pass, invoke `/dev-commit`.

## Review Checklist

```text
[ ] Use semantic tools to analyze code
[ ] No unnecessary cloning (use borrows where possible)
[ ] Error handling: no bare unwrap() in library/application code
[ ] Unsafe code: minimal, justified, with SAFETY comments
[ ] No clippy warnings (run cargo clippy)
[ ] Performance: no unnecessary allocations in hot paths
[ ] Traits: proper use of generics vs trait objects
[ ] Feature flags: properly gated conditional compilation
[ ] Tests: unit tests for new logic, integration tests with pcap files
[ ] Documentation: public items have doc comments
[ ] Concurrency: no blocking in async, proper channel usage
[ ] No security issues (bounds checks, no hardcoded secrets)
[ ] No duplicate code (extract shared logic)
[ ] All tests passing (single-threaded)
```
