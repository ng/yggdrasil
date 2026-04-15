PREFIX ?= $(HOME)/.local/bin

.PHONY: build install uninstall clean

build:
	cargo build --release

install: build
	mkdir -p $(PREFIX)
	cp target/release/ygg $(PREFIX)/ygg
	chmod +x $(PREFIX)/ygg
	@echo "ygg installed to $(PREFIX)/ygg"
	@echo "ensure $(PREFIX) is on your PATH"
	$(PREFIX)/ygg init

uninstall:
	rm -f $(PREFIX)/ygg

clean:
	cargo clean
