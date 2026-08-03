# Building

Requires a Rust toolchain with the `x86_64-unknown-uefi` target:

```sh
rustup target add x86_64-unknown-uefi
make build          # -> crates/bootfixr/target/x86_64-unknown-uefi/release/bootfixr.efi
make                # list every target
```

Both workspaces declare `rust-version = "1.85"`. CI's `test`/`build` jobs
build against whatever `stable` currently is, which drifts forward every six
weeks or so — so code that only compiles on a *recent* stable is a trap: it
passes CI and then fails to build for anyone still on 1.85. When a newer
stdlib API would be clippy's preferred spelling of something 1.85 can
already express, keep the older spelling and add a narrow
`#[allow(unknown_lints, clippy::the_new_lint_name)]` at the call site rather
than raising `rust-version`.

`make dist` stages both binaries — `bootfixr.efi` and `efiprobe.efi`, see
[input.md](input.md) for what the second one is for — plus a `SHA256SUMS` in
`build/dist`, and `make install ESP=/boot/efi` copies `bootfixr.efi` onto a
mounted ESP without touching NVRAM. `make check` runs the same `fmt-check`,
`clippy` and test suite as CI's `test` job; the `build` job does a bit more
than `make build` alone, see the CI table below.

Note that the UEFI application is a separate workspace from `gptcore`, so it
needs its own cargo invocation with an explicit `--target`; the Makefile
handles that. It also prefers `~/.cargo/bin/cargo` when present, because
distro cargo packages generally ship no std for the UEFI target.

The glyph bitmaps in `crates/bootfixr/src/gfx/font_data.rs` are generated and
committed, so a normal build needs neither a font installed nor the tool that
bakes one. `make font` re-bakes them from DejaVu Sans Mono and writes a
specimen image next to the result; see [display.md](display.md).

## CI

`.github/workflows/ci.yml` does four things:

| Job | Trigger | What it does |
| --- | --- | --- |
| `test` | every push and PR | `fmt-check`, `clippy -D warnings`, full test suite (installs `gdisk`) |
| `build` | every push and PR | builds both `.efi` binaries, asserts each really is a PE32+ x86_64 EFI application, checks that both are stamped with the commit they were built from (`git describe`), uploads them as artifacts |
| `continuous` | push to the default branch | deletes and recreates the `continuous` **prerelease** with the binary attached |
| `release` | tag `v*` | creates a **draft** release with the binary attached |

The `continuous` release is deliberately deleted and recreated rather than
edited, so its tag always points at the current default branch. Both the
release and its tag are removed with `|| true`, since the first run has
neither and a cancelled run can leave a tag without a release.

The QEMU harness is not in CI: driving the menus over a serial console depends
on boot timing, which is exactly the kind of thing that turns into a flaky
job. Run `make qemu SCRIPT=repair` locally instead — see
[testing.md](testing.md).
