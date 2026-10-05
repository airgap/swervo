/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Collapsing the spacing of adjacent fullwidth punctuation, the `trim-adjacent` part of
//! `text-spacing-trim: normal` (<https://drafts.csswg.org/css-text-4/#text-spacing-trim-property>).
//!
//! Fullwidth punctuation such as `「` or `、` has half an em of blank space built into its glyph.
//! When two of them meet, as in `」、` or `（「`, one of them is set half-width with the font's
//! `halt` feature. This follows Chrome's implementation (`HanKerning` in Blink), including how it
//! decides from the font's glyphs whether `、。，．：；` behave as closing or as middle
//! punctuation (left-aligned in Japanese and Simplified Chinese fonts, centered in Traditional
//! Chinese ones) and whether curly quotes are fullwidth.

use icu_properties::{EastAsianWidth, GeneralCategory, maps};
use read_fonts::FontRead;
use read_fonts::tables::gpos::Gpos;
use read_fonts::types::Tag;

use crate::{Font, FontTableMethods, GPOS};

pub(crate) const HALT: Tag = Tag::new(b"halt");

#[derive(Clone, Copy, Debug, PartialEq)]
enum CharType {
    Other,
    Open,
    Close,
    Middle,
}

/// What a font's glyphs say about how its punctuation behaves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HanKerningData {
    /// The type of `、。，．`.
    dot: CharType,
    colon: CharType,
    semicolon: CharType,
    /// Whether `“”‘’` are fullwidth, and therefore opening and closing punctuation.
    quotes_are_fullwidth: bool,
}

impl HanKerningData {
    /// Returns `None` if the font cannot set punctuation half-width.
    pub(crate) fn for_font(font: &Font) -> Option<Self> {
        let gpos = font.table_for_tag(GPOS)?;
        let has_halt = Gpos::read(read_fonts::FontData::new(gpos.buffer()))
            .and_then(|gpos| gpos.feature_list())
            .is_ok_and(|feature_list| {
                feature_list
                    .feature_records()
                    .iter()
                    .any(|record| record.feature_tag() == HALT)
            });
        if !has_halt {
            return None;
        }

        let em = font.descriptor.pt_size.to_f32_px();
        let type_from_bounds = |characters: &[char]| {
            let types = characters.iter().map(|character| {
                let Some(glyph) = font.glyph_index(*character) else {
                    return CharType::Other;
                };
                let bounds = font.typographic_bounds(glyph);
                let half_em = em / 2.;
                if bounds.max_x() <= half_em {
                    CharType::Close
                } else if bounds.min_x() >= half_em {
                    CharType::Open
                } else if bounds.width() <= half_em && bounds.min_x() >= half_em / 2. {
                    CharType::Middle
                } else {
                    CharType::Other
                }
            });
            let mut types = types.peekable();
            let first = *types.peek().expect("At least one character");
            match types.all(|char_type| char_type == first) {
                true => first,
                false => CharType::Other,
            }
        };
        let is_fullwidth = |character| {
            font.glyph_index(character)
                .is_some_and(|glyph| (font.glyph_h_advance(glyph) as f32 - em).abs() < em / 16.)
        };

        Some(Self {
            dot: type_from_bounds(&['\u{3001}', '\u{3002}', '\u{FF0C}', '\u{FF0E}']),
            colon: type_from_bounds(&['\u{FF1A}']),
            semicolon: type_from_bounds(&['\u{FF1B}']),
            quotes_are_fullwidth: is_fullwidth('\u{201C}') && is_fullwidth('\u{201D}'),
        })
    }

    fn char_type(&self, character: char) -> CharType {
        match character {
            '\u{3001}' | '\u{3002}' | '\u{FF0C}' | '\u{FF0E}' => self.dot,
            '\u{FF1A}' => self.colon,
            '\u{FF1B}' => self.semicolon,
            '\u{00B7}' | '\u{3000}' | '\u{30FB}' => CharType::Middle,
            '\u{2018}' | '\u{201C}' if self.quotes_are_fullwidth => CharType::Open,
            '\u{2019}' | '\u{201D}' if self.quotes_are_fullwidth => CharType::Close,
            _ => {
                let width = maps::east_asian_width().get(character);
                if width != EastAsianWidth::Wide && width != EastAsianWidth::Fullwidth {
                    return CharType::Other;
                }
                match maps::general_category().get(character) {
                    GeneralCategory::OpenPunctuation => CharType::Open,
                    GeneralCategory::ClosePunctuation => CharType::Close,
                    _ => CharType::Other,
                }
            },
        }
    }

    /// The byte offsets, relative to `range.start`, of the characters in `text[range]` to set
    /// half-width. The characters just outside `range` count as neighbours.
    pub(crate) fn trimmed_punctuation(&self, text: &str, range: std::ops::Range<usize>) -> Vec<usize> {
        let mut previous = text[..range.start]
            .chars()
            .next_back()
            .map_or(CharType::Other, |character| self.char_type(character));
        let mut characters = text[range.clone()].char_indices().peekable();
        let mut following_text = text[range.end..].chars();
        let mut trimmed = Vec::new();
        while let Some((offset, character)) = characters.next() {
            let char_type = self.char_type(character);
            let next = characters
                .peek()
                .map(|(_, character)| *character)
                .or_else(|| following_text.next())
                .map_or(CharType::Other, |character| self.char_type(character));
            let trim = match char_type {
                // An opening bracket after other punctuation, as in `（「` or `、「`.
                CharType::Open => previous != CharType::Other,
                // A closing bracket or dot before another one or a middle dot, as in `」）`.
                CharType::Close => matches!(next, CharType::Close | CharType::Middle),
                CharType::Middle | CharType::Other => false,
            };
            if trim {
                trimmed.push(offset);
            }
            previous = char_type;
        }
        trimmed
    }
}
