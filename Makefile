# bootfixr - build, test and QEMU harness.
#
# The UEFI application and the host-testable core are separate workspaces,
# so they need different cargo invocations. That is what most of this file
# is about.

CARGO      ?= cargo

# Building for x86_64-unknown-uefi needs a rustup toolchain: distro cargo
# packages generally ship no std for that target. Prefer ~/.cargo/bin when
# it is present, which covers both a rustup box and CI.
UEFI_CARGO ?= $(if $(wildcard $(HOME)/.cargo/bin/cargo),$(HOME)/.cargo/bin/cargo,$(CARGO))

TARGET     := x86_64-unknown-uefi
EFI_CRATE  := crates/bootfixr
EFI        := $(EFI_CRATE)/target/$(TARGET)/release/bootfixr.efi
EFI_DEBUG  := $(EFI_CRATE)/target/$(TARGET)/debug/bootfixr.efi

# Built with its own --target-dir so it never clobbers the release build
# above: same crate, same output filename, different feature set (see
# docs/display.md). `make dist` is the only target that compresses it; set
# UPX_BIN if `upx` is not the name on PATH (named UPX_BIN, not UPX, because
# upx itself reads an environment variable called UPX for its own options).
EFI_TINY_DIR := $(EFI_CRATE)/target-tiny
EFI_TINY     := $(EFI_TINY_DIR)/$(TARGET)/release/bootfixr.efi
UPX_BIN      ?= upx

BUILD      ?= build
DIST       := $(BUILD)/dist
IMAGES     ?= $(BUILD)/images

# Corruption applied to the QEMU test disk. Note that OVMF repairs a broken
# main GPT by itself before any application runs, so 'bad-mbr' is the
# mode that actually reaches this tool under QEMU. See docs/testing.md.
CORRUPTION ?= bad-mbr

# Mount point of the ESP for `make install`.
ESP        ?= /boot/efi

.DEFAULT_GOAL := help

# Rasterised on the host and committed, so building the application needs
# neither this font installed nor the tool that bakes it.
FONT       ?= /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf
FONT_DATA  := $(EFI_CRATE)/src/gfx/font_data.rs

.PHONY: help build debug test test-unit test-integration fmt fmt-check \
        clippy check size dist images qemu qemu-repair qemu-shots verify-image \
        font tiny install clean distclean

help: ## Show this help
	@echo "bootfixr targets:"
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "Variables: CORRUPTION=$(CORRUPTION)  SCRIPT=$(SCRIPT)  ESP=$(ESP)  IMAGES=$(IMAGES)  UPX_BIN=$(UPX_BIN)"

# ------------------------------------------------------------------ build

build: ## Build the EFI application (release)
	cd $(EFI_CRATE) && $(UEFI_CARGO) build --release --target $(TARGET)
	@echo "built $(EFI) ($$(stat -c %s $(EFI)) bytes)"

debug: ## Build the EFI application (debug, unstripped)
	cd $(EFI_CRATE) && $(UEFI_CARGO) build --target $(TARGET)

font: ## Re-bake the glyph bitmaps from $(FONT) (writes a specimen too)
	@test -f "$(FONT)" || { echo "no font at $(FONT); set FONT=..." >&2; exit 1; }
	cd tools/mkfont && $(UEFI_CARGO) run --release -- \
	  "$(abspath $(FONT))" "$(abspath $(FONT_DATA))" "$(abspath $(BUILD))/specimen-"
	@echo "look at $(BUILD)/specimen-*.pgm before committing the result."

tiny: ## Build bootfixr-tiny.efi (uncompressed): 12x24 only, for a tight ESP
	cd $(EFI_CRATE) && $(UEFI_CARGO) build --release --target $(TARGET) \
	  --target-dir target-tiny --no-default-features --features tiny
	@echo "built $(EFI_TINY) ($$(stat -c %s $(EFI_TINY)) bytes)"

size: build tiny ## Report binary sizes (bootfixr-tiny.efi before UPX compression)
	@ls -l $(EFI) $(EFI_TINY) \
	  | awk '{printf "%s  %d bytes (%.1f KiB)\n", $$NF, $$5, $$5/1024}'

dist: build tiny ## Stage both binaries (bootfixr-tiny.efi UPX-compressed) and a checksum
	@mkdir -p $(DIST)
	cp $(EFI) $(DIST)/bootfixr.efi
	cp $(EFI_TINY) $(DIST)/bootfixr-tiny.efi
	@command -v $(UPX_BIN) >/dev/null 2>&1 || { \
	  echo "no '$(UPX_BIN)' in PATH; install upx-ucl (crazy-max/ghaction-upx in CI) or set UPX_BIN=/path/to/upx" >&2; exit 1; }
	$(UPX_BIN) --lzma --best $(DIST)/bootfixr-tiny.efi
	cd $(DIST) && sha256sum bootfixr.efi bootfixr-tiny.efi > SHA256SUMS
	@cat $(DIST)/SHA256SUMS

# ------------------------------------------------------------------- test

test: ## Run all host tests (requires gdisk)
	$(CARGO) test

test-unit: ## Run gptcore unit tests only
	$(CARGO) test --lib

test-integration: ## Run the sgdisk-backed image tests only (requires gdisk)
	$(CARGO) test --test repair_images

fmt: ## Format every workspace
	$(CARGO) fmt --all
	cd $(EFI_CRATE) && $(UEFI_CARGO) fmt --all
	cd tools/mkfont && $(UEFI_CARGO) fmt --all

fmt-check: ## Check formatting without writing
	$(CARGO) fmt --all -- --check
	cd $(EFI_CRATE) && $(UEFI_CARGO) fmt --all -- --check
	cd tools/mkfont && $(UEFI_CARGO) fmt --all -- --check

clippy: ## Lint both workspaces, warnings are errors
	$(CARGO) clippy --all-targets -- -D warnings
	cd $(EFI_CRATE) && $(UEFI_CARGO) clippy --target $(TARGET) -- -D warnings
	cd $(EFI_CRATE) && $(UEFI_CARGO) clippy --target $(TARGET) \
	  --no-default-features --features tiny -- -D warnings

check: fmt-check clippy test build ## Everything CI runs

# ------------------------------------------------------------------- qemu

images: build ## Build the QEMU boot and test disk images
	./tools/mkimages.sh $(IMAGES) $(EFI) $(CORRUPTION)

# SCRIPT names a menu walk in tools/run-qemu.sh: none, menu, overview, check,
# repair, repair-boot, repair-cancel, backup, restore, prevent, and the
# boot* walks over the NVRAM screens. ONE_DISK=1 (with check-one) leaves the
# test disk off the machine, which is how the skipped picker gets exercised.
# USB=1 attaches a removable stick, which is what the backup-usb* and
# restore-usb walks need: without it there is only one place a backup can go
# and the application does not ask.
SCRIPT ?= menu

qemu: images ## Boot under OVMF and drive the menus (SCRIPT=...)
	./tools/run-qemu.sh $(IMAGES) $(SCRIPT)

qemu-repair: images ## Boot under OVMF and repair the test disk
	./tools/run-qemu.sh $(IMAGES) repair

# The graphical backend writes nothing to the serial console, so looking at
# it means photographing the framebuffer. QRES is the shape OVMF is asked
# for: 800x1280 is the Steam Deck's panel, and a portrait framebuffer is
# what makes the backend rotate, so it is the default here.
QRES ?= 800x1280

qemu-shots: images ## Boot with a $(QRES) framebuffer and screendump the menus
	SHOTS=$(BUILD)/shots RES=$(QRES) ./tools/run-qemu.sh $(IMAGES) $(SCRIPT)
	@echo "screenshots in $(BUILD)/shots (PPM; any viewer or ImageMagick)"

verify-image: ## Ask sgdisk what it thinks of the test disk
	@sgdisk -v $(IMAGES)/test.img 2>&1 \
	  | grep -E "No problems|Caution|Warning|ERROR|Main header|Backup header" || true
	@sgdisk -p $(IMAGES)/test.img 2>/dev/null | sed -n '/^Number/,$$p'

# ----------------------------------------------------------------- deploy

install: build ## Copy the binary to $(ESP)/EFI (does NOT add a boot entry)
	@test -d "$(ESP)" || { echo "no ESP mounted at $(ESP); set ESP=..." >&2; exit 1; }
	install -D -m 0644 $(EFI) "$(ESP)/EFI/bootfixr.efi"
	@echo "installed to $(ESP)/EFI/bootfixr.efi"
	@echo "invoke it from the firmware's 'boot from file' menu."

# ------------------------------------------------------------------ clean

clean: ## Remove build output and QEMU images
	$(CARGO) clean
	cd $(EFI_CRATE) && $(UEFI_CARGO) clean
	rm -rf $(EFI_TINY_DIR)
	rm -rf $(BUILD)

distclean: clean ## Also remove Cargo.lock files
	rm -f Cargo.lock $(EFI_CRATE)/Cargo.lock
