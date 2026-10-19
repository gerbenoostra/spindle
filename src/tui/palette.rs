//! The palette: the only production code that builds a `Style` or names
//! a `Color` or a `Modifier`. Everything the dashboard renders carries a `Role`; the
//! table below is the whole mapping from role to style. Hues are the
//! normal ANSI 1-6 plus `DarkGray` for borders and separators only -
//! never ANSI 7 and never a bright variant.
//!
//! Every role's style is built on `Style::reset()`: a span must not
//! inherit colour or weight from the cell it lands on - an uncoloured
//! title span over a dim border would otherwise keep the border's gray,
//! and a `Text` suffix beside a bold focused title would inherit both
//! cyan and bold.
//!
//! The styled-text machinery lives here too: renderers build `Segs` -
//! role-labelled text pieces - and `to_line` turns them into a `Line`,
//! applying the one place cursor styling may exist.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::attention::Attention;

/// What a piece of text is, which is all a span may know. The role table
/// assigns each a style; anything unlisted renders `Text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Values, ages, plain summary parts, footer text, context and
    /// cleanup lines, `?` - everything unlisted.
    Text,
    /// Repo names, unfocused titles, section headers, detail headings.
    Strong,
    /// `!` and `waiting`: blocked on the human.
    Wait,
    /// `✗`: a turn aborted, unseen.
    Error,
    /// `✓` and `done`: a clean turn end, unseen.
    Done,
    /// `●` and `working`: busy, nothing above pending.
    Working,
    /// `checks ok`, upstream `tracked`, forge `open`, verdict `safe`.
    /// Segmented middles and detail values arrive with the upcoming
    /// identity-hue work; nothing names `Ok` yet.
    #[allow(dead_code)] // consumed by the upcoming summary segmentation
    Ok,
    /// `error`/`blocked` words, `checks failed`, `remote gone` - the
    /// word role of `Attention::Error`, consumed by `attention_word_role`
    /// once state words render through it.
    #[allow(dead_code)] // consumed by the upcoming summary segmentation
    Bad,
    /// Branch names, bases, upstreams.
    #[allow(dead_code)] // consumed by the upcoming identity-hue labelling
    Ref,
    /// Conversation ids, commit hashes, PR/MR labels.
    #[allow(dead_code)] // consumed by the upcoming identity-hue labelling
    Id,
    /// The `Needs you` section header.
    NeedsYou,
    /// Unfocused borders, the help border.
    Border,
    /// The focused border and title.
    Focus,
    /// ` · ` between parts, `/` between repo and branch, footer `|`.
    Separator,
    /// Footer and help keys.
    Key,
}

impl Role {
    /// The role's style - the only place a `Role` becomes a `Color`.
    /// Each style starts from `Style::reset()` so a span never inherits
    /// the base cell's colour or modifiers.
    pub fn style(self) -> Style {
        match self {
            Role::Text => Style::reset(),
            Role::Strong => Style::reset().add_modifier(Modifier::BOLD),
            Role::Wait => Style::reset().fg(Color::Yellow),
            Role::Error => Style::reset().fg(Color::Red),
            Role::Done | Role::Ok => Style::reset().fg(Color::Green),
            Role::Working => Style::reset().fg(Color::Blue),
            Role::Bad => Style::reset().fg(Color::Red).add_modifier(Modifier::BOLD),
            Role::Ref | Role::Key => Style::reset().fg(Color::Cyan),
            Role::Id => Style::reset().fg(Color::Magenta),
            Role::NeedsYou => Style::reset()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Role::Border | Role::Separator => Style::reset().fg(Color::DarkGray),
            Role::Focus => Style::reset().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        }
    }
}

/// The role an `Attention`'s leading glyph carries: `!` waits, `✗`
/// errored, `✓` done, `●` working; `?` and the empty slot read plain.
pub fn glyph_role(glyph: &str) -> Role {
    match glyph {
        "!" => Role::Wait,
        "✗" => Role::Error,
        "✓" => Role::Done,
        "●" => Role::Working,
        _ => Role::Text,
    }
}

/// The role a row's leading glyph takes from its attention.
pub fn attention_glyph_role(attention: Attention) -> Role {
    glyph_role(attention.glyph())
}

/// The role a state word takes: its glyph's hue, except `error`, which
/// is bold red.
#[allow(dead_code)] // consumed by the upcoming detail state-word colouring
pub fn attention_word_role(attention: Attention) -> Role {
    match attention {
        Attention::Error => Role::Bad,
        other => attention_glyph_role(other),
    }
}

/// What a segment is, which decides whether the unfocused cursor
/// underlines it: `Content` is row text (label, middle, age, context),
/// `Layout` is the spacing and glyph slot around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Content,
    Layout,
}

/// One styled piece of a line: its text, its role, and what it is.
#[derive(Debug, Clone)]
pub struct Seg {
    pub role: Role,
    pub text: String,
    pub kind: SegKind,
}

/// A line under construction: an ordered run of `Seg`s. `push` adds
/// `Content`, `push_layout` adds `Layout` - only `render_row` calls the
/// latter, so kind is never inferred from text or position.
#[derive(Debug, Default)]
pub struct Segs(Vec<Seg>);

impl Segs {
    /// A `Content` segment.
    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.0.push(Seg {
            role,
            text: text.into(),
            kind: SegKind::Content,
        });
    }

    /// A `Layout` segment - row glyph slot, inter-field padding, the gap
    /// before the age.
    pub fn push_layout(&mut self, role: Role, text: impl Into<String>) {
        self.0.push(Seg {
            role,
            text: text.into(),
            kind: SegKind::Layout,
        });
    }

    /// The concatenated text: width math and filters measure this.
    /// `render_row` still measures the cell strings directly; the width
    /// contract moves onto segments with the segmented middles work.
    #[allow(dead_code)] // consumed by the upcoming segmented middles
    pub fn text(&self) -> String {
        self.0.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Where the line sits relative to the focused list's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Not the cursor item.
    None,
    /// The cursor item of the focused list: one reverse-video bar.
    Focused,
    /// The cursor item of an unfocused list: underlined text, kept hues.
    Unfocused,
}

/// `segs` as a `Line` of `width` cells - the sole site of cursor
/// styling. `Focused` strips every span's own style, reverses the whole
/// line and pads it to `width`, because `Paragraph` styles only the
/// cells a line covers. `Unfocused` underlines `Content` segments over
/// their roles; `Layout` segments keep theirs. `None` is roles alone.
/// `Focused` and `None` ignore the kind.
pub fn to_line(segs: &Segs, cursor: Cursor, width: usize) -> Line<'static> {
    match cursor {
        Cursor::Focused => {
            let mut spans: Vec<Span<'static>> =
                segs.0.iter().map(|s| Span::raw(s.text.clone())).collect();
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            if used < width {
                spans.push(Span::raw(" ".repeat(width - used)));
            }
            let mut line = Line::from(spans);
            line.style = Style::default().add_modifier(Modifier::REVERSED);
            line
        }
        Cursor::Unfocused => Line::from(
            segs.0
                .iter()
                .map(|s| {
                    let style = match s.kind {
                        SegKind::Content => s.role.style().add_modifier(Modifier::UNDERLINED),
                        SegKind::Layout => s.role.style(),
                    };
                    Span::styled(s.text.clone(), style)
                })
                .collect::<Vec<_>>(),
        ),
        Cursor::None => Line::from(
            segs.0
                .iter()
                .map(|s| Span::styled(s.text.clone(), s.role.style()))
                .collect::<Vec<_>>(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every role, for the exhaustive pins.
    const ROLES: [Role; 15] = [
        Role::Text,
        Role::Strong,
        Role::Wait,
        Role::Error,
        Role::Done,
        Role::Working,
        Role::Ok,
        Role::Bad,
        Role::Ref,
        Role::Id,
        Role::NeedsYou,
        Role::Border,
        Role::Focus,
        Role::Separator,
        Role::Key,
    ];

    #[test]
    fn every_role_pins_its_style() {
        let expect = |fg: Option<Color>, modifier: Modifier| {
            let mut style = Style::reset();
            if let Some(fg) = fg {
                style = style.fg(fg);
            }
            style.add_modifier(modifier)
        };
        let cases: [(Role, Style); 15] = [
            (Role::Text, expect(None, Modifier::empty())),
            (Role::Strong, expect(None, Modifier::BOLD)),
            (Role::Wait, expect(Some(Color::Yellow), Modifier::empty())),
            (Role::Error, expect(Some(Color::Red), Modifier::empty())),
            (Role::Done, expect(Some(Color::Green), Modifier::empty())),
            (Role::Working, expect(Some(Color::Blue), Modifier::empty())),
            (Role::Ok, expect(Some(Color::Green), Modifier::empty())),
            (Role::Bad, expect(Some(Color::Red), Modifier::BOLD)),
            (Role::Ref, expect(Some(Color::Cyan), Modifier::empty())),
            (Role::Id, expect(Some(Color::Magenta), Modifier::empty())),
            (Role::NeedsYou, expect(Some(Color::Yellow), Modifier::BOLD)),
            (
                Role::Border,
                expect(Some(Color::DarkGray), Modifier::empty()),
            ),
            (Role::Focus, expect(Some(Color::Cyan), Modifier::BOLD)),
            (
                Role::Separator,
                expect(Some(Color::DarkGray), Modifier::empty()),
            ),
            (Role::Key, expect(Some(Color::Cyan), Modifier::empty())),
        ];
        for (role, style) in cases {
            assert_eq!(role.style(), style, "{role:?}");
        }
        assert_eq!(cases.len(), ROLES.len(), "every role is pinned");
    }

    #[test]
    fn no_role_uses_ansi7_or_a_bright_hue() {
        // The ANSI 7 pair `White`/`Gray` and every bright (9-14) variant
        // are forbidden; `DarkGray` (ANSI 8) stays for borders and
        // separators. `Reset` is not a colour - it is the explicit
        // default-foreground reset each role builds on.
        for role in ROLES {
            let style = role.style();
            let forbidden = [
                Color::White,
                Color::Gray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
            ];
            assert!(
                !forbidden.contains(&style.fg.unwrap_or(Color::Reset)),
                "{role:?}"
            );
        }
    }

    #[test]
    fn both_attention_maps_are_exhaustive() {
        for (attention, glyph, word) in [
            (Attention::Waiting, Role::Wait, Role::Wait),
            (Attention::Error, Role::Error, Role::Bad),
            (Attention::CompletedUnseen, Role::Done, Role::Done),
            (Attention::Working, Role::Working, Role::Working),
            (Attention::Unknown, Role::Text, Role::Text),
            (Attention::None, Role::Text, Role::Text),
        ] {
            assert_eq!(attention_glyph_role(attention), glyph, "{attention:?}");
            assert_eq!(attention_word_role(attention), word, "{attention:?}");
        }
        // The glyph map itself: `?` and the empty slot stay plain.
        assert_eq!(glyph_role("!"), Role::Wait);
        assert_eq!(glyph_role("✗"), Role::Error);
        assert_eq!(glyph_role("✓"), Role::Done);
        assert_eq!(glyph_role("●"), Role::Working);
        assert_eq!(glyph_role("?"), Role::Text);
        assert_eq!(glyph_role(""), Role::Text);
    }

    #[test]
    fn segs_concatenate_and_keep_their_kind() {
        let mut segs = Segs::default();
        segs.push(Role::Ref, "ab");
        segs.push_layout(Role::Text, "  ");
        segs.push(Role::Text, "cd");
        assert_eq!(segs.text(), "ab  cd");
        let kinds: Vec<SegKind> = segs.0.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [SegKind::Content, SegKind::Layout, SegKind::Content]);
    }

    #[test]
    fn to_line_styles_each_cursor_state() {
        let segs = || {
            let mut segs = Segs::default();
            segs.push_layout(Role::Wait, "! ");
            segs.push(Role::Ref, "feat/x");
            segs.push_layout(Role::Text, "  ");
            segs.push(Role::Text, "2m");
            segs
        };

        // None: roles only, the kind changes nothing.
        let line = to_line(&segs(), Cursor::None, 20);
        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(line.spans[1].style.fg, Some(Color::Cyan));
        for span in &line.spans {
            assert!(!span.style.add_modifier.contains(Modifier::UNDERLINED));
        }
        assert!(!line.style.add_modifier.contains(Modifier::REVERSED));

        // Unfocused: Content underlined over its role, Layout keeps its
        // role without the underline.
        let line = to_line(&segs(), Cursor::Unfocused, 20);
        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(line.spans[1].style.fg, Some(Color::Cyan));
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            !line.spans[2]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            line.spans[3]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(!line.style.add_modifier.contains(Modifier::REVERSED));

        // Focused: every span neutral, the whole line reversed, padded to
        // the full cell width.
        let line = to_line(&segs(), Cursor::Focused, 20);
        assert!(line.style.add_modifier.contains(Modifier::REVERSED));
        for span in &line.spans {
            assert_eq!(span.style.fg, None, "{span:?}");
            assert_eq!(span.style.bg, None, "{span:?}");
        }
        assert_eq!(line.width(), 20);
        assert_eq!(
            line.spans.last().expect("the padding").content,
            " ".repeat(8)
        );
    }

    #[test]
    fn to_line_kind_matters_only_when_unfocused() {
        // Two runs identical but for one segment's kind: `None` and
        // `Focused` produce identical lines; only `Unfocused` tells them
        // apart.
        let pair = |kind| {
            let mut segs = Segs::default();
            segs.push(Role::Ref, "a");
            match kind {
                SegKind::Content => segs.push(Role::Text, "b"),
                SegKind::Layout => segs.push_layout(Role::Text, "b"),
            }
            segs
        };
        let (content, layout) = (pair(SegKind::Content), pair(SegKind::Layout));
        for cursor in [Cursor::None, Cursor::Focused] {
            let a = to_line(&content, cursor, 4);
            let b = to_line(&layout, cursor, 4);
            assert_eq!(
                a.spans
                    .iter()
                    .map(|s| (s.content.as_ref(), s.style))
                    .collect::<Vec<_>>(),
                b.spans
                    .iter()
                    .map(|s| (s.content.as_ref(), s.style))
                    .collect::<Vec<_>>(),
                "{cursor:?}"
            );
        }
        let a = to_line(&content, Cursor::Unfocused, 4);
        let b = to_line(&layout, Cursor::Unfocused, 4);
        assert!(a.spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!b.spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
    }
}
