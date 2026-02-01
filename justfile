# cc-sessions justfile

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
    xattr -cr ~/.local/bin/cc-sessions
    codesign -s - ~/.local/bin/cc-sessions

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
