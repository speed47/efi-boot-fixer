# efigptfix - build, test and QEMU harness.
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
EFI_CRATE  := crates/efigptfix
EFI        := $(EFI_CRATE)/target/$(TARGET)/release/efigptfix.efi
EFI_DEBUG  := $(EFI_CRATE)/target/$(TARGET)/debug/efigptfix.efi

BUILD      ?= build
DIST       := $(BUILD)/dist
IMAGES     ?= $(BUILD)/images

# Corruption applied to the QEMU test disk. Note that OVMF repairs a broken
# primary GPT by itself before any application runs, so 'bad-mbr' is the
# mode that actually reaches this tool under QEMU. See the README.
CORRUPTION ?= bad-mbr

# Mount point of the ESP for `make install`.
ESP        ?= /boot/efi

.DEFAULT_GOAL := help

.PHONY: help build debug test test-unit test-integration fmt fmt-check \
        clippy check size dist images qemu qemu-confirm verify-image \
        install clean distclean

help: ## Show this help
	@echo "efigptfix targets:"
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "Variables: CORRUPTION=$(CORRUPTION)  ESP=$(ESP)  IMAGES=$(IMAGES)"

# ------------------------------------------------------------------ build

build: ## Build the EFI application (release)
	cd $(EFI_CRATE) && $(UEFI_CARGO) build --release --target $(TARGET)
	@echo "built $(EFI) ($$(stat -c %s $(EFI)) bytes)"

debug: ## Build the EFI application (debug, unstripped)
	cd $(EFI_CRATE) && $(UEFI_CARGO) build --target $(TARGET)

size: build ## Report the binary size
	@ls -l $(EFI) | awk '{printf "%s  %d bytes (%.1f KiB)\n", $$NF, $$5, $$5/1024}'

dist: build ## Stage the binary and a checksum in build/dist
	@mkdir -p $(DIST)
	cp $(EFI) $(DIST)/efigptfix.efi
	cd $(DIST) && sha256sum efigptfix.efi > SHA256SUMS
	@cat $(DIST)/SHA256SUMS

# ------------------------------------------------------------------- test

test: ## Run all host tests (requires gdisk)
	$(CARGO) test

test-unit: ## Run gptcore unit tests only
	$(CARGO) test --lib

test-integration: ## Run the sgdisk-backed image tests only (requires gdisk)
	$(CARGO) test --test repair_images

fmt: ## Format both workspaces
	$(CARGO) fmt --all
	cd $(EFI_CRATE) && $(UEFI_CARGO) fmt --all

fmt-check: ## Check formatting without writing
	$(CARGO) fmt --all -- --check
	cd $(EFI_CRATE) && $(UEFI_CARGO) fmt --all -- --check

clippy: ## Lint both workspaces, warnings are errors
	$(CARGO) clippy --all-targets -- -D warnings
	cd $(EFI_CRATE) && $(UEFI_CARGO) clippy --target $(TARGET) -- -D warnings

check: fmt-check clippy test build ## Everything CI runs

# ------------------------------------------------------------------- qemu

images: build ## Build the QEMU boot and test disk images
	./tools/mkimages.sh $(IMAGES) $(EFI) $(CORRUPTION)

qemu: images ## Boot under OVMF, declining the repair
	./tools/run-qemu.sh $(IMAGES) no

qemu-confirm: images ## Boot under OVMF and type the confirmation
	./tools/run-qemu.sh $(IMAGES) yes

verify-image: ## Ask sgdisk what it thinks of the test disk
	@sgdisk -v $(IMAGES)/test.img 2>&1 \
	  | grep -E "No problems|Caution|Warning|ERROR|Main header|Backup header" || true
	@sgdisk -p $(IMAGES)/test.img 2>/dev/null | sed -n '/^Number/,$$p'

# ----------------------------------------------------------------- deploy

install: build ## Copy the binary to $(ESP)/EFI (does NOT add a boot entry)
	@test -d "$(ESP)" || { echo "no ESP mounted at $(ESP); set ESP=..." >&2; exit 1; }
	install -D -m 0644 $(EFI) "$(ESP)/EFI/efigptfix.efi"
	@echo "installed to $(ESP)/EFI/efigptfix.efi"
	@echo "invoke it from the firmware's 'boot from file' menu."

# ------------------------------------------------------------------ clean

clean: ## Remove build output and QEMU images
	$(CARGO) clean
	cd $(EFI_CRATE) && $(UEFI_CARGO) clean
	rm -rf $(BUILD)

distclean: clean ## Also remove Cargo.lock files
	rm -f Cargo.lock $(EFI_CRATE)/Cargo.lock
