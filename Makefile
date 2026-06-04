.PHONY: fmt fmt-check lint test build pr

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

lint:
	cargo clippy -- -D warnings

test:
	cargo test --verbose

build:
	cargo build --verbose

# Reproduces the full CI gate locally: format check → clippy → tests.
# Run before opening a PR to catch failures without waiting for GitHub Actions.
pr: fmt-check lint test
