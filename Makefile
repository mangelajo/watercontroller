# Watercontroller dev Makefile.
#
# Idempotent. Each "install" target uses the existence of its installed
# artifact as a sentinel, so re-running `make bootstrap` does no work
# once everything is in place.
#
# Prereqs your distro must already provide:
#   bash, curl, tar, xz, python3, podman (or docker w/ a podman alias).
#   Architecture: x86_64-linux (Espressif's prebuilt QEMU is x86_64 only;
#   on aarch64 you'd build qemu-system-xtensa from source — outside scope).

SHELL       := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

# Layout we install into ---------------------------------------------------
HOME_BIN     := $(HOME)/.cargo/bin
ESPUP        := $(HOME_BIN)/espup
LDPROXY      := $(HOME_BIN)/ldproxy
RUSTUP       := $(HOME_BIN)/rustup
ESP_TC       := $(HOME)/.rustup/toolchains/esp
QEMU_DIR     := $(HOME)/.local/qemu-xtensa
QEMU_BIN     := $(QEMU_DIR)/qemu/bin/qemu-system-xtensa
QEMU_RELEASE := esp-develop-9.0.0-20240606
QEMU_URL     := https://github.com/espressif/qemu/releases/download/$(QEMU_RELEASE)/qemu-xtensa-softmmu-esp_develop_9.0.0_20240606-x86_64-linux-gnu.tar.xz
PW_VENV      := tests/playwright/.venv
PW_SENTINEL  := $(PW_VENV)/.bootstrap-ok
CONTAINER    := docker.io/espressif/idf-rust:esp32_latest

# Always invoke cargo via the rustup-managed shim so the `esp` toolchain
# override (rust-toolchain.toml inside crates/firmware) works.
CARGO := $(HOME_BIN)/cargo

.DEFAULT_GOAL := help

# -- help ------------------------------------------------------------------

.PHONY: help
help: ## list targets
	@printf "Watercontroller dev tasks:\n\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  \033[1m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\nFirst time on a fresh box:\n  make bootstrap\n\n"

# -- one-time installs -----------------------------------------------------

.PHONY: bootstrap
bootstrap: arch-check rustup esp-toolchain qemu-xtensa container playwright ## install everything: rustup, esp toolchain, qemu, container, playwright
	@printf "\n\033[32m✓ bootstrap complete.\033[0m Add to your shell rc if you haven't:\n"
	@printf "    export PATH=\"\$$HOME/.cargo/bin:\$$PATH\"\n"

.PHONY: arch-check
arch-check:
	@arch=$$(uname -m); \
	if [ "$$arch" != "x86_64" ]; then \
		echo "ERROR: Espressif's prebuilt QEMU is x86_64 only; you have $$arch."; \
		echo "       Build qemu-system-xtensa from source (https://github.com/espressif/qemu) and"; \
		echo "       set WC_QEMU_BIN to point at it; the rest of the Makefile is portable."; \
		exit 1; \
	fi

# rustup -------------------------------------------------------------------

$(RUSTUP):
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
		| sh -s -- -y --no-modify-path --default-toolchain stable

.PHONY: rustup
rustup: $(RUSTUP) ## install rustup (skipped if already present)

# espup + Xtensa toolchain -------------------------------------------------

$(ESPUP): $(RUSTUP)
	$(CARGO) install espup --locked

$(LDPROXY): $(RUSTUP)
	$(CARGO) install ldproxy --locked

$(ESP_TC): $(ESPUP)
	$(ESPUP) install --targets esp32

.PHONY: esp-toolchain
esp-toolchain: $(ESPUP) $(LDPROXY) $(ESP_TC) ## install Xtensa Rust + LLVM + GCC via espup

# Espressif QEMU fork (prebuilt) ------------------------------------------

$(QEMU_BIN):
	@mkdir -p $(QEMU_DIR)
	@printf "Downloading qemu-system-xtensa $(QEMU_RELEASE) (~14 MB)...\n"
	@curl -L --fail --silent -o $(QEMU_DIR)/qemu.tar.xz $(QEMU_URL)
	@tar -xf $(QEMU_DIR)/qemu.tar.xz -C $(QEMU_DIR)
	@rm $(QEMU_DIR)/qemu.tar.xz
	@test -x $(QEMU_BIN) || (echo "qemu binary not where expected: $(QEMU_BIN)" && exit 1)
	@printf "\033[32m✓\033[0m qemu-system-xtensa at $(QEMU_BIN)\n"

.PHONY: qemu-xtensa
qemu-xtensa: $(QEMU_BIN) ## download Espressif's prebuilt qemu-system-xtensa

# espressif/idf-rust container --------------------------------------------

.PHONY: container
container: ## pull the idf-rust container (used by scripts/firmware.sh)
	@if podman image inspect $(CONTAINER) >/dev/null 2>&1; then \
		printf "\033[32m✓\033[0m %s already present\n" "$(CONTAINER)"; \
	else \
		podman pull $(CONTAINER); \
	fi

# Playwright Python venv ---------------------------------------------------

$(PW_SENTINEL): tests/playwright/requirements.txt
	python3 -m venv $(PW_VENV)
	$(PW_VENV)/bin/pip install --quiet --upgrade pip
	$(PW_VENV)/bin/pip install --quiet -r tests/playwright/requirements.txt
	$(PW_VENV)/bin/playwright install chromium
	@touch $@

.PHONY: playwright
playwright: $(PW_SENTINEL) ## set up Playwright venv + headless chromium

# -- day-to-day commands ---------------------------------------------------

.PHONY: test
test: ## cargo test -p watercontroller-core
	$(CARGO) test --lib -p watercontroller-core

.PHONY: host
host: ## cargo run -p watercontroller-host  (SPA on http://127.0.0.1:8765)
	$(CARGO) run -p watercontroller-host

.PHONY: firmware
firmware: ## debug firmware build (inside the idf-rust container)
	./scripts/firmware.sh build

.PHONY: firmware-release
firmware-release: ## release firmware build (smaller, fits OTA partition)
	./scripts/firmware.sh build --release

.PHONY: firmware-shell
firmware-shell: ## drop into a shell inside the firmware build container
	./scripts/firmware.sh shell

.PHONY: qemu
qemu: $(QEMU_BIN) ## release+features=qemu firmware, boot in QEMU (HTTP on :18080, telnet on :18023)
	./scripts/qemu.sh

.PHONY: qemu-stop
qemu-stop: ## kill any running qemu-system-xtensa instance
	@pids=$$(pgrep -f qemu-system-xtensa || true); \
	if [ -n "$$pids" ]; then kill -9 $$pids && echo "killed: $$pids"; else echo "no qemu running"; fi

.PHONY: ui-tests
ui-tests: $(PW_SENTINEL) ## Playwright tests in headless chromium against the host build
	$(PW_VENV)/bin/pytest tests/playwright -v

# -- OTA over the network -------------------------------------------------
# Iterating on a flashed device: 0.5 s upload + ~3 s reboot, vs. ~10 s for
# the full serial flash + boot cycle.

APP_BIN := target/firmware/app.bin

.PHONY: app-image
app-image: ## build app-only OTA image (target/firmware/app.bin)
	@./scripts/firmware.sh build --release > /dev/null
	@podman run --rm --userns=keep-id:uid=1000,gid=1000 \
	    -v $$(pwd):/project:Z \
	    -w /project/crates/firmware \
	    docker.io/espressif/idf-rust:esp32_latest \
	    espflash save-image \
	        --chip esp32 --flash-size 4mb \
	        /project/target/firmware/xtensa-esp32-espidf/release/watercontroller-firmware \
	        /project/$(APP_BIN) > /dev/null
	@printf "\033[32m✓\033[0m %s ($(shell du -h $(APP_BIN) 2>/dev/null | cut -f1))\n" "$(APP_BIN)"

.PHONY: ota
ota: app-image ## OTA-flash a running device. Usage: make ota IP=<addr> [TOKEN=<bearer>]
	@if [ -z "$(IP)" ]; then \
	    echo "Usage: make ota IP=<device-ip> [TOKEN=<admin-token>]"; \
	    echo "       TOKEN only needed if Config.admin_token is non-empty."; \
	    exit 1; \
	fi
	@./scripts/ota-flash.sh "$(IP)" "$(APP_BIN)" "$(TOKEN)"

.PHONY: ota-status
ota-status: ## quick status snapshot. Usage: make ota-status IP=<addr>
	@if [ -z "$(IP)" ]; then echo "Usage: make ota-status IP=<device-ip>"; exit 1; fi
	@curl -s --max-time 5 http://$(IP)/api/status | python3 -m json.tool

# Build + serial-flash + run the full playwright suite against the real
# device, all inside a `jmp shell` lease. The lease auto-releases when the
# inner shell exits.
#
# Override SELECTOR to target a specific exporter (default: any board
# labelled target=esp32). Override IP to skip the flash and run tests
# against an already-running device:
#   make device-test
#   make device-test SELECTOR='-n my-esp32'
#   make device-test IP=192.168.1.182    # skip flash, reuse device
SELECTOR ?= -l target=esp32

.PHONY: device-test
device-test: app-image $(PW_SENTINEL) ## build firmware, flash via jumpstarter, run playwright suite against the real device
	@command -v jmp >/dev/null 2>&1 || { echo "jmp (jumpstarter CLI) not on PATH"; exit 1; }
	@jmp shell $(SELECTOR) -- env IP=$(IP) BOOT_TIMEOUT_S=$(BOOT_TIMEOUT_S) ./scripts/device-test.sh

# -- maintenance -----------------------------------------------------------

.PHONY: clean
clean: ## cargo clean (firmware target dir included)
	$(CARGO) clean
	rm -rf target/firmware

.PHONY: distclean
distclean: clean ## also nuke the cargo / espressif side caches
	rm -rf $(HOME)/.cache/watercontroller-cargo $(HOME)/.cache/watercontroller-espressif

# -- diagnostics -----------------------------------------------------------

.PHONY: doctor
doctor: ## report what's installed vs. missing
	@printf "%-12s %s\n" "rustup:"    "$$(test -x $(RUSTUP)  && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "espup:"     "$$(test -x $(ESPUP)   && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "ldproxy:"   "$$(test -x $(LDPROXY) && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "esp tc:"    "$$(test -d $(ESP_TC)  && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "qemu:"      "$$(test -x $(QEMU_BIN) && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "podman:"    "$$(command -v podman >/dev/null 2>&1 && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "container:" "$$(podman image inspect $(CONTAINER) >/dev/null 2>&1 && echo ✓ || echo ✗ missing)"
	@printf "%-12s %s\n" "playwright:" "$$(test -f $(PW_SENTINEL) && echo ✓ || echo ✗ missing)"
