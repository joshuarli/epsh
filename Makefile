.PHONY: test conformance bench

test:
	cargo test -- --test-threads=4
	cargo build
	perl check.pl -p ./target/debug/epsh -s check-epsh.t

conformance:
	cargo build
	perl check.pl -v -p ./target/debug/epsh -s check-epsh.t

bench:
	cargo build
	sh tests/stress/run.sh ./target/debug/epsh dash

setup:
	prek install --install-hooks

pc:
	prek --quiet run --all-files

# Usage: make bump-version [V=x.y.z]
# Without V, increments the patch version.
bump-version:
ifndef V
	$(eval OLD := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml))
	$(eval V := $(shell echo "$(OLD)" | awk -F. '{printf "%d.%d.%d", $$1, $$2, $$3+1}'))
endif
	sed -i '' 's/^version = ".*"/version = "$(V)"/' Cargo.toml
	cargo check --quiet 2>/dev/null
	git add Cargo.toml Cargo.lock
	git commit -m "bump version to $(V)"
	git tag "release/$(V)"
