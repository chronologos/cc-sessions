# cc-sessions justfile

version       := `grep '^version' Cargo.toml | head -1 | cut -d'"' -f2`
apple_targets := "aarch64-apple-darwin x86_64-apple-darwin"
linux_targets := "aarch64-unknown-linux-musl x86_64-unknown-linux-musl"

# Default recipe - show available commands
default:
    @just --list

# Build release binary
build:
    cargo build --release

# Run tests
test:
    cargo test

# Build and install to ~/.local/bin (with macOS signing)
install: build
    cp target/release/cc-sessions ~/.local/bin/
    @if [ "$(uname)" = "Darwin" ]; then \
        codesign -s - -f ~/.local/bin/cc-sessions; \
    fi

# One-time: install cross-compilation prerequisites
cross-setup:
    rustup target add {{apple_targets}} {{linux_targets}}
    @command -v cargo-zigbuild >/dev/null || cargo install --locked cargo-zigbuild
    @command -v zig >/dev/null || { echo "zig not found -- run: brew install zig"; exit 1; }

# Cross-compile release binaries for mac+linux, arm+x86
build-all:
    @for t in {{apple_targets}}; do \
        echo "==> $t"; \
        cargo build --release --target $t || exit 1; \
    done
    @for t in {{linux_targets}}; do \
        echo "==> $t"; \
        cargo zigbuild --release --target $t || exit 1; \
    done

# Package per-target tarballs into dist/
dist: build-all
    rm -rf dist
    mkdir -p dist
    @for t in {{apple_targets}} {{linux_targets}}; do \
        out="dist/cc-sessions-{{version}}-$t.tar.gz"; \
        tar --no-xattrs -czf "$out" -C "target/$t/release" cc-sessions; \
    done
    @ls -lh dist/

# Run with arguments (e.g., just run -- --list)
run *ARGS:
    cargo run --release -- {{ARGS}}

# Check code without building
check:
    cargo check

# Format code
fmt:
    cargo fmt

# Lint with clippy
lint:
    cargo clippy -- -D warnings

# Clean build artifacts
clean:
    cargo clean

# Watch for changes and run tests
watch-test:
    cargo watch -x test

# Watch for changes and check
watch:
    cargo watch -x check
