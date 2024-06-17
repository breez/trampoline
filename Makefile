build: 
	cargo build

check: clippy fmt-check test

clippy:
	cargo clippy -- -D warnings -A clippy::uninlined-format-args
	cargo clippy --tests -- -D warnings -A clippy::uninlined-format-args

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

test: build
	cd itest; pytest tests
