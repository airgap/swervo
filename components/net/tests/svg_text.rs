/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use resvg::{tiny_skia, usvg};

const AHEM: &[u8] = include_bytes!("../../../tests/wpt/tests/fonts/Ahem.ttf");
const LATO_LIGA: &[u8] = include_bytes!("../../../tests/wpt/tests/fonts/Lato-Medium-Liga.ttf");

fn render_svg(svg: &str, font: Option<&[u8]>) -> tiny_skia::Pixmap {
    // Never load system fonts: the only available face must be the checked-in fixture.
    let mut options = usvg::Options::default();
    assert_eq!(options.fontdb.faces().count(), 0);
    if let Some(font) = font {
        options.fontdb_mut().load_font_data(font.to_vec());
        assert_eq!(options.fontdb.faces().count(), 1);
    }

    let tree = usvg::Tree::from_str(svg, &options).expect("test SVG should parse");
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    pixmap
}

fn assert_black_rectangles(pixmap: &tiny_skia::Pixmap, rectangles: &[(u32, u32, u32, u32)]) {
    for y in 0..pixmap.height() {
        for x in 0..pixmap.width() {
            let inside = rectangles.iter().any(|&(left, top, right, bottom)| {
                x >= left && x < right && y >= top && y < bottom
            });
            let pixel = pixmap.pixel(x, y).unwrap();
            assert_eq!(
                [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()],
                [0, 0, 0, if inside { 255 } else { 0 }],
                "unexpected pixel at ({x}, {y})"
            );
        }
    }
}

#[test]
fn svg_text_loads_in_memory_font_and_preserves_advances() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="48">
        <text x="8" y="28" font-family="Ahem" font-size="20">X X</text>
    </svg>"#;

    let without_font = render_svg(svg, None);
    assert!(
        without_font.pixels().iter().all(|pixel| pixel.alpha() == 0),
        "an empty font database must not silently use system fonts"
    );

    // Ahem's X is one em square, extending 0.8 em above and 0.2 em below the
    // baseline. Its space also advances one em but has no outline.
    let with_font = render_svg(svg, Some(AHEM));
    assert_black_rectangles(&with_font, &[(8, 12, 28, 32), (48, 12, 68, 32)]);
}

#[test]
fn svg_text_preserves_anchor_and_nested_transforms() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="120"
                     viewBox="0 0 100 60">
        <g transform="translate(5 4) scale(2)">
            <text x="20" y="18" font-family="Ahem" font-size="10"
                  text-anchor="middle">X X</text>
        </g>
    </svg>"#;

    // The centered run starts at x=5. Apply the group's scale/translation,
    // then the viewBox's 2x scale, to both glyph positions and their outlines.
    let pixmap = render_svg(svg, Some(AHEM));
    assert_black_rectangles(&pixmap, &[(30, 48, 70, 88), (110, 48, 150, 88)]);
}

#[test]
fn svg_text_shapes_standard_ligatures() {
    let render_text = |text| {
        render_svg(
            &format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="64">
                    <text x="10" y="48" font-family="Lato Medium Liga" font-size="36">
                        {text}</text>
                </svg>"#
            ),
            Some(LATO_LIGA),
        )
    };

    let shaped = render_text("fi");
    let ligature = render_text("&#xfb01;");
    let unjoined = render_text("f&#x200c;i");

    assert!(
        shaped.pixels().iter().any(|pixel| pixel.alpha() != 0),
        "ligature comparison must contain visible glyphs"
    );
    assert!(
        shaped.data() == ligature.data(),
        "shaping fi must produce the fixture's explicit U+FB01 ligature"
    );
    assert!(
        shaped.data() != unjoined.data(),
        "a zero-width non-joiner must prevent the fi ligature"
    );
}
