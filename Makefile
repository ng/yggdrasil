PREFIX ?= /usr/local/bin

.PHONY: build install uninstall clean

build:
	cargo build --release

install: build
	sudo cp target/release/ygg $(PREFIX)/ygg
	sudo chmod +x $(PREFIX)/ygg
	@echo "ygg installed to $(PREFIX)/ygg"
	ygg init

uninstall:
	sudo rm -f $(PREFIX)/ygg

clean:
	cargo clean
