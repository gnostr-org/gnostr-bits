all:

@PHONY: webui-deps
webui-deps:## 	webui-deps
	cd desktop && npm install
	cd crates/librqbit/webui && npm install

@PHONY: webui-dev
webui-dev: webui-deps## 	webui-dev
	cd crates/librqbit/webui && \
	npm run dev

@PHONY: webui-build
webui-build: webui-deps## 	webui-build
	cd crates/librqbit/webui && \
	npm run build

@PHONY: devserver
devserver:## 	devserver
	echo -n '' > /tmp/gnostr-bits-log
	cargo run --release -- \
		--log-file /tmp/gnostr-bits-log \
		--log-file-rust-log=debug,librqbit=trace \
		server start /tmp/scratch/

@PHONY: clean
clean:## 	clean
	rm -rf target

@PHONY: sign-debug
sign-debug:## 	sign-debug
	bash -c "[ '$(shell uname -s)' == 'Darwin' ] && codesign -f --entitlements resources/debugging.entitlements -s - target/debug/gnostr-bits"

@PHONY: sign-release
sign-release:## 	sign-release
	bash -c "[ '$(shell uname -s)' == 'Darwin' ] && codesign -f --entitlements resources/debugging.entitlements -s - target/release/gnostr-bits"

@PHONY: build-release
build-release:## 	build-release
	cargo build --release

@PHONY: install
install: build-release## 	install
	$(MAKE) build-release
	$(MAKE) sign-release
	install target/release/gnostr-bits "$(HOME)/bin/"

@PHONY: release-macos-universal
release-macos-universal:## 	release-macos-universal
	cargo build --target aarch64-apple-darwin --profile release-github
	cargo build --target x86_64-apple-darwin --profile release-github
	lipo \
		./target/aarch64-apple-darwin/release-github/gnostr-bits \
		./target/x86_64-apple-darwin/release-github/gnostr-bits \
		-create \
		-output ./target/x86_64-apple-darwin/release-github/gnostr-bits-osx-universal

@PHONY: release-windows
release-windows:## 	release-windowss
	# prereqs:
	# brew install mingw-w64
	cargo build --target x86_64-pc-windows-gnu --profile release-github

@PHONY: release-linux-current-target
release-linux-current-target:## 	release-linux-current-target
	CC_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-gcc \
	CXX_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-g++ \
	AR_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-ar \
	CARGO_TARGET_$(TARGET_SNAKE_UPPER_CASE)_LINKER=$(CROSS_COMPILE_PREFIX)-gcc \
	cargo build  --profile release-github --target=$(TARGET) --features=openssl-vendored

@PHONY: release-linux
release-linux: release-linux-x86_64 release-linux-aarch64 release-linux-armv6 release-linux-armv7## 	release-linux

@PHONY: release-linux-x86_64
release-linux-x86_64:## 	release-linux-x86-64
	TARGET=x86_64-unknown-linux-gnu \
	TARGET_SNAKE_CASE=x86_64_unknown_linux_gnu \
	TARGET_SNAKE_UPPER_CASE=X86_64_UNKNOWN_LINUX_GNU \
	CROSS_COMPILE_PREFIX=x86_64-unknown-linux-gnu \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-aarch64
release-linux-aarch64:## 	release-linux-aarch64
	TARGET=aarch64-unknown-linux-gnu \
	TARGET_SNAKE_CASE=aarch64_unknown_linux_gnu \
	TARGET_SNAKE_UPPER_CASE=AARCH64_UNKNOWN_LINUX_GNU \
	CROSS_COMPILE_PREFIX=aarch64-unknown-linux-gnu \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-armv6
release-linux-armv6:## 	release-linux-armv6
	TARGET=arm-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=arm_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARM_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=arm-linux-gnueabihf \
	LDFLAGS=-latomic \
	$(MAKE) release-linux-current-target

# armv7-unknown-linux-gnueabihf
@PHONY: release-linux-armv7
release-linux-armv7:## 	release-linux-armv7
	TARGET=armv7-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-gnueabihf \
	$(MAKE) release-linux-current-target


@PHONY: release-all
release-all: release-windows release-linux release-macos-universal## 	release-all
	rm -rf /tmp/gnotr-bits-release
	mkdir -p /tmp/gnostr-bitss-release
	cp ./target/x86_64-pc-windows-gnu/release-github/gnostr-bits.exe /tmp/gnostr-bits-release
	cp ./target/x86_64-apple-darwin/release-github/gnostr-bits-osx-universal /tmp/gnostr-bits-release
	cp ./target/x86_64-unknown-linux-gnu/release-github/gnostr-bits /tmp/gnostr-bits-release/gnostr-bits-linux-x86_64
	echo "The release was built in /tmp/gnostr-bits-release"
