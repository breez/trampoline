build: 
	cargo build

check: clippy fmt-check test

clean: clean-test clean-rust

clean-test:
	rm -rf test-env

clean-rust:
	rm -rf target

clippy:
	cargo clippy -- -D warnings -A clippy::uninlined-format-args
	cargo clippy --tests -- -D warnings -A clippy::uninlined-format-args

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

test: build test-env
	test-env/bin/pytest itest/tests

test-env:
	virtualenv test-env --python=$(which python3) --download --always-copy --clear
	test-env/bin/python3 -m pip install -U pip
	test-env/bin/pip install ./itest
