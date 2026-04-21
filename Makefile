.PHONY: test conformance bench

NAME       := epsh
TARGET     := $(shell rustc -vV | awk '/^host:/ {print $$2}')

test:
	cargo test
	cargo build
	perl check.pl -p ./target/debug/epsh -s check-epsh.t

conformance:
	cargo build
	perl check.pl -v -p ./target/debug/epsh -s check-epsh.t

bench:
	cargo build
	sh tests/stress/run.sh ./target/debug/epsh dash

release:
	cargo clean -p $(NAME) --release --target $(TARGET)
	RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

setup:
	prek install --prepare-hooks -f

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
