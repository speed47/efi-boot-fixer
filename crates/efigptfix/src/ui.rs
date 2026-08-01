//! Console output and the confirmation gate.
//!
//! All report *formatting* lives in `gptcore::report`, which has no UEFI
//! dependency and is tested on the host. This module only puts the lines on
//! the screen and reads the operator's answer back.

use alloc::string::String;
use gptcore::repair::{Analysis, RepairPlan};
use gptcore::report;
use uefi::boot;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::{print, println, system};

pub fn banner() {
    println!(
        "efigptfix {} - repair a corrupt primary GPT from the backup",
        env!("CARGO_PKG_VERSION")
    );
    println!();
}

fn print_lines(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

/// The full per-disk report: health, verdict, and if applicable the
/// proposed table and the ordered write plan.
pub fn print_report(analysis: &Analysis, plan: Option<&RepairPlan>) {
    print_lines(&report::render(analysis, plan));
}

fn next_key() -> Option<Key> {
    system::with_stdin(|stdin| stdin.read_key().ok().flatten())
}

/// Block until the operator types `word` exactly, or presses Escape.
///
/// NOTE: this requires a keyboard, which a Steam Deck does not have in the
/// firmware environment. Pending the results of the input probe, this is
/// the wrong gate for the target hardware; see `bin/efiprobe.rs`.
pub fn confirm(word: &str) -> bool {
    print!("  Type {word} then Enter to write, or Esc to skip: ");
    let mut typed = String::new();
    loop {
        let Some(key) = next_key() else {
            boot::stall(10_000);
            continue;
        };
        match key {
            Key::Special(ScanCode::ESCAPE) => {
                println!();
                return false;
            }
            Key::Printable(c) => match char::from(c) {
                '\r' | '\n' => {
                    println!();
                    return typed == word;
                }
                '\u{8}' => {
                    if typed.pop().is_some() {
                        print!("\u{8} \u{8}");
                    }
                }
                ch => {
                    typed.push(ch);
                    print!("{ch}");
                }
            },
            _ => {}
        }
    }
}

/// Block until Enter or Escape, discarding anything else.
pub fn wait_for_enter() {
    loop {
        let Some(key) = next_key() else {
            boot::stall(10_000);
            continue;
        };
        match key {
            Key::Special(ScanCode::ESCAPE) => return,
            Key::Printable(c) if matches!(char::from(c), '\r' | '\n') => return,
            _ => {}
        }
    }
}
