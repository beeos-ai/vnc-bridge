.PHONY: build build-linux build-macos build-all clean

VERSION ?= 0.1.0

build:
	cargo build --release

build-linux:
	docker run --rm -v "$$PWD":/src -w /src rust:1.83-bookworm \
		cargo build --release --target x86_64-unknown-linux-gnu

build-macos:
	cargo build --release --target aarch64-apple-darwin
	cargo build --release --target x86_64-apple-darwin

build-all: build-linux build-macos

clean:
	cargo clean
