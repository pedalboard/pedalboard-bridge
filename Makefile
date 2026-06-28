BINARY = pedalboard-bridge
REMOTE = laenzi@cm5-dev.home
REMOTE_DIR = ~/projects/pedalboard-bridge
DEPLOY_DIR = /udata/pedalboard-bridge
GIT_HASH = $(shell git describe --always --dirty=+dev)
LDFLAGS = -ldflags "-X main.version=0.1.0-$(GIT_HASH)"

.PHONY: build deploy restart clean

build:
	go build $(LDFLAGS) -o $(BINARY) .

deploy:
	git push
	ssh $(REMOTE) 'cd $(REMOTE_DIR) && git pull && /usr/local/go/bin/go build -ldflags "-X main.version=0.1.0-$(GIT_HASH)" -o $(BINARY) .'
	ssh $(REMOTE) "sudo systemctl stop $(BINARY) && sudo cp $(REMOTE_DIR)/$(BINARY) $(DEPLOY_DIR)/ && sudo systemctl start $(BINARY)"

restart:
	ssh $(REMOTE) "sudo systemctl restart $(BINARY)"

clean:
	rm -f $(BINARY)
