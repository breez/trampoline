CLIPPY_OPTS := -D warnings -A clippy::uninlined-format-args

# (method=thread to support xdist)
PYTEST_OPTS := -v -p no:logging $(PYTEST_OPTS)

# If we run the tests in parallel we can speed testing up by a lot, however we
# then don't exit on the first error, since that'd kill the other tester
# processes and result in loads in loads of output. So we only tell py.test to
# abort early if we aren't running in parallel.
ifneq ($(PYTEST_PAR),)
PYTEST_OPTS += -n=$(PYTEST_PAR)
else
PYTEST_OPTS += -x
endif

# Set the individual test timeout
ifneq ($(SLOW_MACHINE),)
PYTEST_OPTS += --timeout 180
else
PYTEST_OPTS += --timeout 30
endif


build: 
	cargo build

check: clippy fmt-check test

clean: clean-test clean-rust

clean-test:
	rm -rf itest-env
	rm -rf .pytest_cache
	rm -rf itest/build
	rm -rf itest/tests/__pycache__
	rm -rf itest/trampoline.egg-info

clean-rust:
	rm -rf target

clippy:
	cargo clippy -- $(CLIPPY_OPTS)
	cargo clippy --tests -- $(CLIPPY_OPTS)

fmt: fmt-python fmt-rust

fmt-check: fmt-check-python fmt-check-rust

fmt-check-python: itest-env
	itest-env/bin/black ./itest --check

fmt-check-rust:
	cargo fmt -- --check

fmt-python: itest-env
	itest-env/bin/black ./itest

fmt-rust:
	cargo fmt

itest: build itest-env
	. itest-env/bin/activate; itest-env/bin/pytest itest/tests $(PYTEST_OPTS)

itest-env:
	virtualenv itest-env --python=$(which python3) --download --always-copy --clear
	itest-env/bin/python3 -m pip install -U pip
	itest-env/bin/pip install ./itest

release:
	cargo build --release

test: utest itest

utest:
	cargo test
