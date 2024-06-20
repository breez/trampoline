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

itest: build itest-env
	itest-env/bin/pytest itest/tests

itest-env:
	virtualenv itest-env --python=$(which python3) --download --always-copy --clear
	itest-env/bin/python3 -m pip install -U pip
	itest-env/bin/pip install ./itest

release:
	cargo build --release

test: utest itest

utest:
	cargo test
