# Development

## Prerequisites

Rust (stable), plus a C/C++ toolchain, **cmake**, and **libclang**. Those last
three are needed because the HTTP client compiles BoringSSL from source rather
than linking a system TLS library.

| Platform | Setup |
|---|---|
| Debian/Ubuntu | `sudo apt-get install build-essential cmake clang libclang-dev` |
| Fedora | `sudo dnf install gcc gcc-c++ cmake clang clang-devel` |
| macOS | `xcode-select --install && brew install cmake` |
| Windows | Visual Studio with the C++ workload, plus cmake, LLVM and NASM (see below) |

Jump to [Releasing](#releasing) if that is what you came for.

```bash
git clone https://github.com/hamaadraza/hls-proxy.git
cd hls-proxy
cargo build
cargo test
cargo run
```

The first build takes several minutes because BoringSSL is compiled from
scratch. Later builds are quick.

## Windows specifics

Two things commonly go wrong.

**libclang.** If the build reports `Unable to find libclang`, point bindgen at
your LLVM install:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

Set it permanently with `[Environment]::SetEnvironmentVariable("LIBCLANG_PATH", "C:\Program Files\LLVM\bin", "User")`.

**NASM.** BoringSSL assembles its x86/x86_64 crypto with NASM, and `boring-sys2`
only disables assembly automatically when cross-compiling — so a native Windows
build fails with `No CMAKE_ASM_NASM_COMPILER could be found` unless NASM is
installed.

[`cmake/boringssl-asm-fallback.cmake`](../cmake/boringssl-asm-fallback.cmake)
handles this automatically:

- **NASM on `PATH`** → nothing changes, BoringSSL builds with assembly.
- **NASM missing** → falls back to the portable C backend so the build succeeds.

The fallback costs raw AES/SHA throughput and nothing else. It does not change
what the proxy does, or how its requests look on the wire.

To get the faster build, install NASM in an elevated shell:

```powershell
choco install nasm
```

Then reopen your terminal and force cmake to reconsider — a stale
`CMakeCache.txt` will otherwise keep the previous decision:

```bash
cargo clean -p boring-sys2
```

Linux and macOS always have a working assembler, so the shim does nothing there.

## Cross-compiling

[`.cargo/config.toml`](../.cargo/config.toml) sets `CMAKE_TOOLCHAIN_FILE`, and
`boring-sys2` skips its own cross-compile cmake setup whenever that variable is
set. **Remove the `[env]` section before building for a non-host target.**

The release workflow avoids the problem entirely by building every target on a
native runner.

## Project layout

```
src/
  main.rs      config, router, CORS, startup
  payload.rs   token schema, base64, URL validation
  proxy.rs     request handlers, classification, streaming
  rewrite.rs   playlist rewriting (pure, unit-tested)
  client.rs    HTTP client pool, one per browser profile
cmake/         BoringSSL assembly shim (Windows only)
docs/          this documentation
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for how these fit together.

## Tests

```bash
cargo test
```

The suite is unit tests, no network access, and runs in milliseconds. It covers
token round-tripping, the SSRF guard, playlist rewriting (relative and absolute
URLs, `EXT-X-KEY`/`EXT-X-MAP`, multiple URIs on one line, `data:` URIs, comment
preservation), response classification, and base-URL resolution.

The rewriting logic in `rewrite.rs` is written as pure functions specifically so
it can be tested without a server or a network. New playlist quirks belong there,
with a test that captures the shape of the playlist that triggered them.

### Testing against a real stream

```bash
cargo run &

curl "http://localhost:8080/encode?url=https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"
# then fetch the returned url, follow a variant, then a segment
```

A quick end-to-end check that everything is wired up:

```bash
TOKEN=$(curl -s "http://localhost:8080/encode?url=https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['payload'])")

curl -s "http://localhost:8080/proxy/$TOKEN" | head -5
```

Every non-comment line should point back at `localhost:8080`.

### Checking the upstream fingerprint

To confirm upstream requests really do look like a browser, proxy a
fingerprinting endpoint and inspect the result:

```bash
curl "http://localhost:8080/encode?url=https://tls.peet.ws/api/all"
# fetch the returned url; ja3/ja4 and the HTTP/2 fingerprint should match Chrome,
# and differ from what plain `curl https://tls.peet.ws/api/all` reports
```

## Before pushing

CI enforces both of these, so run them first:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

## CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs on every push and
pull request:

- `rustfmt` and `clippy -D warnings` on Linux
- the test suite on Linux, Windows and macOS
- a smoke test that starts the binary and calls `/encode`, which catches
  failures a unit test cannot — such as a binary that will not start

Windows CI installs NASM, so the assembly build path is exercised there too.

## Releasing

Pushing a `v*` tag is what triggers a release —
[`.github/workflows/release.yml`](../.github/workflows/release.yml) then builds
every target on a native runner and publishes the archives.

### Cutting a release

**1. Bump the version in [`Cargo.toml`](../Cargo.toml)** so it matches the tag
you are about to push:

```toml
[package]
version = "0.2.0"
```

This step is easy to forget and nothing enforces it. The tag names the archives,
but `Cargo.toml` is what gets compiled into the binary, so skipping it ships a
`v0.2.0` archive containing a binary that still calls itself `0.1.0`.

**2. Commit the bump** (`Cargo.lock` updates too, and the release build uses
`--locked`, so it must be committed):

```bash
cargo check                     # refreshes Cargo.lock
git add Cargo.toml Cargo.lock
git commit -m "Release v0.2.0"
git push
```

**3. Tag and push the tag:**

```bash
git tag v0.2.0
git push origin v0.2.0
```

That is the step that actually starts the release. Watch it under the repository's
**Actions → Release** tab.

### What you get

`.tar.gz` (Linux/macOS) and `.zip` (Windows) archives, each with a `.sha256`
checksum, attached to a GitHub Release with generated notes. Each archive
contains the binary, `README.md`, `.env.example`, `LICENSE` and `docs/`.

### Fixing a bad release

Tags are just pointers, so a mistake is recoverable. Delete both the remote and
local tag, then re-tag:

```bash
git push origin :refs/tags/v0.2.0
git tag -d v0.2.0
```

Delete the GitHub Release too (the workflow uploads with `--clobber`, so a
re-run would otherwise add files to the existing release rather than replace it).

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` |
| `x86_64-apple-darwin` | `macos-15-intel` |
| `aarch64-apple-darwin` | `macos-14` |
| `x86_64-pc-windows-msvc` | `windows-2022` |
| `aarch64-pc-windows-msvc` | `windows-11-arm` (experimental) |

Linux builds use the oldest supported image for the widest glibc compatibility.

`aarch64-pc-windows-msvc` is marked experimental and cannot block a release:
BoringSSL's Windows assembly path is x86-only, so that target deliberately gets
no NASM and builds the portable C backend.

Running the workflow manually builds the archives without publishing, unless it
is run from a tag — useful for rehearsing a release.

## Dependencies worth knowing about

| Crate | Why |
|---|---|
| [`axum`](https://github.com/tokio-rs/axum) | HTTP server and routing |
| [`wreq`](https://github.com/0x676e67/wreq) | HTTP client with browser TLS/HTTP2 emulation; the reason BoringSSL is built |
| [`wreq-util`](https://github.com/0x676e67/wreq-util) | The browser profiles themselves |
| [`tower-http`](https://github.com/tower-rs/tower-http) | CORS and request tracing |

`wreq-util` tracks browser releases closely, so keeping it current is what keeps
the emulated profiles matching real browsers. Dependabot is configured to open
weekly update PRs for both cargo and GitHub Actions.
