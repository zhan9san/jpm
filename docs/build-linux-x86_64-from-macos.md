# Build Linux x86_64 on macOS

Use Zig only.

```bash
brew install zig
cargo install cargo-zigbuild --locked
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Binary output:

```text
target/x86_64-unknown-linux-musl/release/jpm
```
