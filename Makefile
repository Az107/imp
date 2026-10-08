# Packaging and development entry points (§9).
#
# `make dist` builds one tarball per target with a matching `.sha256`, which is
# the layout `install.sh` and a Forgejo/GitHub release expect. `make release`
# builds the host's binary only.

CARGO ?= cargo +1.89.0
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST := dist

# The targets a release publishes. macOS builds only on a macOS host; the Linux
# musl targets need a musl C toolchain (see AGENTS.md, "Packaging").
TARGETS ?= aarch64-unknown-linux-musl x86_64-unknown-linux-musl

.PHONY: all build test lint fmt release dist check clean install

all: lint test

build:
	$(CARGO) build --workspace --locked

test:
	$(CARGO) test --workspace --locked

lint:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

fmt:
	$(CARGO) fmt --all

check:
	$(CARGO) check --workspace --all-targets --locked

## release: build the host's stripped release binary.
release:
	$(CARGO) build --release --locked

## dist: one tarball and checksum per target under $(DIST).
dist:
	@mkdir -p $(DIST)
	@for target in $(TARGETS); do \
		echo "== $$target"; \
		$(CARGO) build --release --locked --target $$target || exit 1; \
		asset=imp-$(VERSION)-$$target.tar.gz; \
		tar -czf $(DIST)/$$asset -C target/$$target/release imp; \
		(cd $(DIST) && sha256sum $$asset > $$asset.sha256); \
	done
	@ls -lh $(DIST)

## install: cargo-install the CLI from this checkout.
install:
	$(CARGO) install --path crates/imp-cli --locked

clean:
	$(CARGO) clean
	rm -rf $(DIST)
