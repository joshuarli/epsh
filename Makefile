.PHONY: test conformance bench

test:
	cargo test
	cargo build
	@perl check.pl -p ./target/debug/epsh -s check-epsh.t; \
	if [ $$? -le 6 ]; then echo "mksh conformance: ok (6 known failures)"; else exit 1; fi

conformance:
	cargo build
	perl check.pl -v -p ./target/debug/epsh -s check-epsh.t

bench:
	cargo build
	sh tests/stress/run.sh ./target/debug/epsh dash
