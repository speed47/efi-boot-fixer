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
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Line { text: text.into(), style }
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
