build: 
	cargo build

check: build
	cd itest; pytest tests -n $(shell nproc)