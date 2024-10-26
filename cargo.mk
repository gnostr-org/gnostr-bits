build-release:cargo-build-release## 	build-release
cargo-br: cargo-b-release
cargo-b-release:cargo-build-release## 	cargo-build-release
cargo-build-release:## 	cargo-build-release
	@cargo b -r
install:cargo-install## 	install
cargo-i:cargo-install
cargo-install:## 	cargo-install
	@cargo install --path crates/bits
	@cargo install --path crates/modal
cargo-sort:## 	cargo-sort
	@[ -x cargo-sort ] || cargo install cargo-sort
	cargo-sort
cargo-deny-check-bans:## 	cargo-deny-check-bans
	@[ -x cargo-deny ] || cargo install cargo-deny
	cargo deny check bans
