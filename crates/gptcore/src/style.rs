//! Line styling, decided where the text is written rather than where it is
//! painted.
//!
//! The report writers know what a line *means* — this is a defect, this is
//! a caption, this is the value you must actually read before authorising
//! a write. The screen only knows what colour to make things. Putting the
//! meaning in `gptcore` keeps that judgement next to the text it applies
//! to, keeps it testable on the host, and means the UEFI side never has to
//! guess by matching on substrings, which would silently stop working the
//! first time someone rewords a message.
//!
//! [`Style`] is deliberately semantic, not a palette. `Bad` means "this is
//! damage", not "red" — the mapping to actual colours lives in the
//! application, where the display's limitations are known.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Style {
    /// Body text.
    #[default]
    Normal,
    /// A caption introducing a block.
    Title,
    /// Chrome: column headings, provenance, things you read only if you
    /// are looking for them.
    Dim,
    /// Healthy, verified, succeeded.
    Good,
    /// Needs attention, or a caveat worth reading before continuing.
    Warn,
    /// Damage, refusal, failure.
    Bad,
    /// A value the operator must actually take in: an LBA about to be
    /// overwritten, the change being proposed.
    Key,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub text: String,
    pub style: Style,
}

impl Line {
    /// Every line drawn on a screen or written to the report is built
    /// here, which makes this the place to neutralise control characters.
    ///
    /// Partition names, boot entry descriptions and the variable names in
    /// a snapshot are all text somebody else chose. The framebuffer
    /// console honours `\r` and `\n` and *drops* whatever a newline pushes
    /// past the last row, so a name carrying either can blank the row it
    /// is drawn on, forge a plausible one, or push the list of writes off
    /// the bottom of the page the operator is about to authorise from —
    /// while `page` counts lines and sees nothing missing. Firmware
    /// strings have been filtered like this since `diskinfo` was written;
    /// the strings that come off a disk were not.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        let text: String = text.into();
        let text = match text.chars().any(char::is_control) {
            false => text,
            true => text.chars().map(|c| if c.is_control() { '.' } else { c }).collect(),
        };
        Line { text, style }
    }

    pub fn blank() -> Self {
        Line { text: String::new(), style: Style::Normal }
    }
}

pub fn line(text: impl Into<String>) -> Line {
    Line::new(text, Style::Normal)
}

pub fn title(text: impl Into<String>) -> Line {
    Line::new(text, Style::Title)
}

pub fn dim(text: impl Into<String>) -> Line {
    Line::new(text, Style::Dim)
}

pub fn good(text: impl Into<String>) -> Line {
    Line::new(text, Style::Good)
}

pub fn warn(text: impl Into<String>) -> Line {
    Line::new(text, Style::Warn)
}

pub fn bad(text: impl Into<String>) -> Line {
    Line::new(text, Style::Bad)
}

pub fn key(text: impl Into<String>) -> Line {
    Line::new(text, Style::Key)
}

/// Every line's text, newline-separated. For tests and host tooling.
pub fn plain(lines: &[Line]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&l.text);
    }
    out
}

/// Push several lines of the same style.
pub fn block(out: &mut Vec<Line>, style: Style, texts: &[&str]) {
    for t in texts {
        out.push(Line::new(*t, style));
    }
}

/// Break `text` into lines no wider than `columns`, with `hang` in front of
/// every line after the first.
///
/// Breaks after a `/` wherever it can. The only values here long enough to
/// need this are UEFI device paths, and
/// `PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,..)/HD(1,GPT,..)` reads far better
/// split at its own separators than at whatever column the margin falls on.
/// A single segment too long for a line of its own is broken hard, because
/// the alternative is the truncation this exists to avoid.
pub fn wrap(text: &str, columns: usize, style: Style, hang: &str) -> Vec<Line> {
    let width = columns.max(8);
    let hang_len = hang.chars().count();
    let mut out = Vec::new();
    let mut line = String::new();
    let mut len = 0usize;

    // `split_inclusive` keeps the separator on the segment it ends, so a
    // break lands after a '/' rather than before one.
    for segment in text.split_inclusive('/') {
        if len > 0 && len + segment.chars().count() > width {
            out.push(Line::new(core::mem::take(&mut line), style));
            line.push_str(hang);
            len = hang_len;
        }
        for ch in segment.chars() {
            if len >= width {
                out.push(Line::new(core::mem::take(&mut line), style));
                line.push_str(hang);
                len = hang_len;
            }
            line.push(ch);
            len += 1;
        }
    }
    if len > 0 {
        out.push(Line::new(line, style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::string::ToString;
    use alloc::vec;

    fn texts(lines: &[Line]) -> vec::Vec<String> {
        lines.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn short_text_is_one_line_and_untouched() {
        let out = wrap("  hello", 40, Style::Dim, "    ");
        assert_eq!(texts(&out), vec!["  hello".to_string()]);
    }

    #[test]
    fn a_device_path_breaks_after_its_separators() {
        let path = "  PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,00-00)/HD(1,GPT,ABCD)";
        let out = wrap(path, 30, Style::Dim, "    ");
        assert_eq!(
            texts(&out),
            vec![
                "  PciRoot(0x0)/Pci(0x2,0x0)/".to_string(),
                "    NVMe(0x1,00-00)/".to_string(),
                "    HD(1,GPT,ABCD)".to_string(),
            ]
        );
        // Nothing was dropped on the way: strip the hangs and it is the
        // original string back, leading indent and all.
        assert_eq!(texts(&out).concat().replace("    ", ""), path);
        assert!(out.iter().all(|l| l.text.chars().count() <= 30));
    }

    #[test]
    fn a_segment_wider_than_the_line_is_broken_rather_than_lost() {
        let out = wrap("aaaaaaaaaaaaaaaaaaaa/b", 10, Style::Dim, "  ");
        assert!(out.iter().all(|l| l.text.chars().count() <= 10), "{:?}", texts(&out));
        assert_eq!(texts(&out).concat().replace("  ", ""), "aaaaaaaaaaaaaaaaaaaa/b");
    }

    #[test]
    fn every_style_carries_to_the_continuations() {
        let out = wrap("  a/b/c/d/e/f/g/h/i/j", 8, Style::Warn, "    ");
        assert!(out.len() > 1);
        assert!(out.iter().all(|l| l.style == Style::Warn));
    }

    #[test]
    fn empty_text_produces_nothing_to_draw() {
        assert!(wrap("", 40, Style::Dim, "  ").is_empty());
    }

    /// A partition name, a boot entry's description and a snapshot's
    /// variable names are all somebody else's text. One line of it has to
    /// stay one line: the console honours `\r` and `\n` and silently drops
    /// what a newline pushes off the bottom, which is how a review screen
    /// comes to show fewer writes than it is about to authorise.
    #[test]
    fn control_characters_never_reach_a_line() {
        let l = Line::new("esp\r\n\n\nrootfs\u{7}", Style::Normal);
        assert_eq!(l.text, "esp....rootfs.");
        assert!(!l.text.chars().any(char::is_control));

        // One line in, one line out, whatever the text tried to do.
        let out = wrap("  a\nb\nc", 40, Style::Dim, "  ");
        assert_eq!(out.len(), 1);
        assert!(!out[0].text.chars().any(char::is_control));

        // Ordinary text is passed through untouched.
        assert_eq!(Line::new("  Disk 1 - 931.5 GiB", Style::Dim).text, "  Disk 1 - 931.5 GiB");
    }
}
