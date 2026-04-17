PREFIX ?= $(HOME)/.local/bin

.PHONY: build install uninstall clean

build:
	cargo build --release

install: build
	mkdir -p $(PREFIX)
	cp target/release/ygg $(PREFIX)/ygg
	chmod +x $(PREFIX)/ygg
	@# Re-sign ad-hoc after copy: on macOS, a freshly-landed binary at a
	@# new path is re-evaluated by Gatekeeper on first launch and can be
	@# SIGKILL'd ("zsh: killed") without any dialog. Replacing the
	@# signature after the copy produces a fresh code-directory hash
	@# that Gatekeeper accepts for the new path. No-op on Linux.
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		codesign --force -s - $(PREFIX)/ygg >/dev/null 2>&1 || true; \
	fi
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
