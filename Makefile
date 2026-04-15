PREFIX ?= $(HOME)/.local/bin

.PHONY: build install uninstall clean

build:
	cargo build --release

install: build
	mkdir -p $(PREFIX)
	cp target/release/ygg $(PREFIX)/ygg
	chmod +x $(PREFIX)/ygg
	@echo "ygg installed to $(PREFIX)/ygg"
	@if [ "$(PREFIX)/ygg" != "$$(which ygg 2>/dev/null)" ]; then \
		echo "NOTE: a different ygg is in your PATH at $$(which ygg 2>/dev/null)"; \
		echo "      to use the new build: export PATH=$(PREFIX):\$$PATH"; \
		echo "      or update it with: sudo cp $(PREFIX)/ygg $$(which ygg)"; \
	fi
	@echo "run 'ygg init' to finish setup"

uninstall:
	rm -f $(PREFIX)/ygg

clean:
	cargo clean
