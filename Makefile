BINARY = pedalboard-bridge
REMOTE = laenzi@cm5-dev.home
REMOTE_DIR = /tmp/bridge-rust
DEPLOY_DIR = /udata/pedalboard-bridge

.PHONY: build lint deploy restart clean

build:
	cargo build --release

lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings

deploy:
	rsync -av --exclude target . $(REMOTE):$(REMOTE_DIR)/
	ssh $(REMOTE) 'source ~/.cargo/env && cd $(REMOTE_DIR) && cargo build --release'
	ssh $(REMOTE) "sudo systemctl stop $(BINARY) && sudo cp $(REMOTE_DIR)/target/release/$(BINARY) $(DEPLOY_DIR)/$(BINARY)-rust && sudo systemctl start $(BINARY)"

restart:
	ssh $(REMOTE) "sudo systemctl restart $(BINARY)"

clean:
	cargo clean
