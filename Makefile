PREFIX ?= $(HOME)/.local/bin

.PHONY: build install reinstall uninstall verify clean

build:
	cargo build --release

# Atomic install. Sequence matters:
#   1. cp to a sibling tmp path (avoids text-file-busy on Linux + Gatekeeper
#      cache invalidation on macOS when an older ygg is currently running)
#   2. chmod
#   3. mv (atomic rename) — the next invocation gets a fresh inode
#   4. codesign --force on macOS so the new path has a clean signature
#   5. verify by running --version against the installed path; SIGKILL on
#      first launch is the canonical "you cp'd over a running binary"
#      symptom and we want install to fail loudly when it happens.
install: build
	@mkdir -p $(PREFIX)
	cp target/release/ygg $(PREFIX)/ygg.next
	chmod +x $(PREFIX)/ygg.next
	mv -f $(PREFIX)/ygg.next $(PREFIX)/ygg
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		codesign --force -s - $(PREFIX)/ygg >/dev/null 2>&1 || true; \
	fi
	@$(MAKE) -s verify PREFIX=$(PREFIX)
	@echo "ygg installed to $(PREFIX)/ygg"
	@if [ "$(PREFIX)/ygg" != "$$(which ygg 2>/dev/null)" ]; then \
		echo "NOTE: a different ygg is in your PATH at $$(which ygg 2>/dev/null)"; \
		echo "      to use the new build: export PATH=$(PREFIX):\$$PATH"; \
		echo "      or update it with: sudo cp $(PREFIX)/ygg $$(which ygg)"; \
	fi
	@echo "run 'ygg init' to finish setup"

# Force a re-sign + verify against the currently-installed binary. Useful
# after a manual `cp` that bypassed `make install` and now hangs (the
# canonical "ygg status fails after I copied the binary" recovery path).
reinstall:
	@if [ ! -x $(PREFIX)/ygg ]; then \
		echo "no binary at $(PREFIX)/ygg; run 'make install' instead" >&2; \
		exit 1; \
	fi
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		codesign --remove-signature $(PREFIX)/ygg >/dev/null 2>&1 || true; \
		codesign --force -s - $(PREFIX)/ygg; \
	fi
	@$(MAKE) -s verify PREFIX=$(PREFIX)
	@echo "ygg re-signed and verified at $(PREFIX)/ygg"

# Run the installed binary's --version with a hard 5s timeout. SIGKILL or
# hang is the canonical macOS-Gatekeeper-cache-invalidation symptom; treat
# either as install failure. Skips the timeout binary check on systems
# without `perl` (rare) by falling back to a raw invocation.
verify:
	@if [ ! -x $(PREFIX)/ygg ]; then \
		echo "FAIL: $(PREFIX)/ygg not found or not executable" >&2; \
		exit 1; \
	fi
	@if command -v perl >/dev/null 2>&1; then \
		out=$$(perl -e ' \
			eval { \
				local $$SIG{ALRM} = sub { die "timeout\n" }; \
				alarm 5; \
				my $$pid = open(my $$fh, "-|", "$(PREFIX)/ygg", "--version") or die $$!; \
				my $$line = <$$fh>; \
				alarm 0; \
				close $$fh; \
				print $$line if defined $$line; \
			}; \
			if ($$@) { print STDERR "verify: $$@"; exit 1 } \
		' 2>&1) ; rc=$$?; \
	else \
		out=$$($(PREFIX)/ygg --version 2>&1); rc=$$?; \
	fi; \
	if [ $$rc -ne 0 ] || ! echo "$$out" | grep -q "^ygg "; then \
		echo "FAIL: $(PREFIX)/ygg --version did not return cleanly within 5s." >&2; \
		echo "      symptom: SIGKILL or hang. on macOS this means the codesign cache for" >&2; \
		echo "      the previous binary at this path was invalidated by your cp/mv. run" >&2; \
		echo "      'make reinstall' to fix it without rebuilding." >&2; \
		exit 1; \
	fi; \
	echo "verify: $$out"

uninstall:
	rm -f $(PREFIX)/ygg

clean:
	cargo clean
