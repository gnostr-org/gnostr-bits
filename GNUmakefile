GNOSTR_BITS=$(shell which gnostr-bits)
export GNOSTR_BITS

.PHONY:- help
-:
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?##/ {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
more:## 	more help
	@sed -n 's/^##//p' ${MAKEFILE_LIST} | column -t -s ':' |  sed -e 's/^/	/'
	#$(MAKE) -f Makefile help

rustup-install:
## 	rustup target add x86_64-unknown-linux-musl
	rustup target add x86_64-unknown-linux-musl

test-dl-from-remote:## 	test-dl-from-remote
	rm -rf ~/.gnostr/bits/test-remote
	$(GNOSTR_BITS) download https://bitcoincore.org/bin/bitcoin-core-26.0/bitcoin-26.0.torrent -o ~/.gnostr/bits/test-remote
test-dl-from-local:## 	test-dl-from-local
	rm -rf ~/.gnostr/bits/test-local
	$(GNOSTR_BITS) download ./bitcoin-26.0.local.torrent -o ~/.gnostr/bits/test-local

.PHONY:desktop
desktop:
	@npx kill-port 4240 >/tmp/gnostr-bits.log || true
	@npx kill-port 3030 >/tmp/gnostr-bits.log || true
	@pushd desktop && npm run dev >/tmp/gnostr-bits.log & pushd desktop/src-tauri >/tmp/gnostr-bits.log && cargo run

-include Makefile
-include cargo.mk
