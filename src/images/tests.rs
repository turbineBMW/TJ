use std::sync::{Arc, Mutex};

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Config, Term, test::TermSize},
    vte::ansi::Processor,
};
use base64::Engine as _;

use super::{Filter, Geometry, PLACEHOLDER, Store, decode_cell, kitty, sixel};

type Replies = Arc<Mutex<Vec<String>>>;

/// A filter with 10x20 px cells on an 80x24 grid; replies are collected.
fn filter() -> (Filter, Arc<Mutex<Store>>, Replies) {
    let geometry =
        Arc::new(Mutex::new(Geometry { cell_width: 10.0, cell_height: 20.0, scale: 1.0, cols: 80, rows: 24 }));
    let store = Arc::new(Mutex::new(Store::default()));
    let replies = Arc::new(Mutex::new(Vec::new()));
    let sink = replies.clone();
    (Filter::new(geometry, store.clone(), move |r| sink.lock().unwrap().push(r)), store, replies)
}

fn run(filter: &mut Filter, input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    filter.feed(input, &mut out);
    out
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]))
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// What the filter's output does to an 80x24 terminal.
fn screen(bytes: &[u8]) -> Term<VoidListener> {
    let mut term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// The placeholder at a cell: (placement, row, column).
fn cell(term: &Term<VoidListener>, line: i32, col: usize) -> Option<(u16, u16, u16)> {
    let cell = &term.grid()[Point::new(Line(line), Column(col))];
    decode_cell(cell.c, cell.fg, cell.bg)
}

#[test]
fn ordinary_output_passes_through_unchanged_even_split_anywhere() {
    let text: &[u8] = b"plain \x1b[1;31mred\x1b[0m \x1b]0;title\x07 \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \
        \x1bP$qm\x1b\\ \x1b_notkitty\x1b\\ \x1b[2J\x1b[?1049h\x1b7\x1b8 \xe2\x9c\x93 done";
    for split in 0..text.len() {
        let (mut f, _, replies) = filter();
        let mut out = run(&mut f, &text[..split]);
        out.extend(run(&mut f, &text[split..]));
        assert_eq!(out, text, "split at {split}");
        assert!(replies.lock().unwrap().is_empty());
    }
}

#[test]
fn a_kitty_image_becomes_placeholder_cells_and_the_cursor_moves_past_it() {
    let (mut f, store, replies) = filter();
    // 25x30 px with 10x20 cells: 3 columns, 2 rows.
    let command = format!("ab\x1b_Ga=T,f=100,i=7;{}\x1b\\cd", b64(&png(25, 30)));
    let out = run(&mut f, command.as_bytes());
    assert!(!out.windows(3).any(|w| w == b"\x1b_G"), "the APC is gone");
    let term = screen(&out);
    let id = cell(&term, 0, 2).expect("a placeholder at the cursor").0;
    assert_eq!(cell(&term, 0, 2), Some((id, 0, 0)));
    assert_eq!(cell(&term, 0, 4), Some((id, 0, 2)));
    assert_eq!(cell(&term, 1, 3), Some((id, 1, 1)));
    assert_eq!(cell(&term, 2, 2), None, "two rows only");
    // kitty's default: right of the image on its last row, so "cd" follows it there.
    assert_eq!(term.grid()[Point::new(Line(1), Column(5))].c, 'c');
    let store = store.lock().unwrap();
    let p = store.placement(id).unwrap();
    assert_eq!((p.cols, p.rows, p.picture.width), (3, 2, 25));
    assert_eq!(replies.lock().unwrap().as_slice(), ["\x1b_Gi=7;OK\x1b\\"]);
}

#[test]
fn the_apps_colours_survive_an_image() {
    let (mut f, _, _) = filter();
    let command = format!("\x1b[31m\x1b_Ga=T,f=100,q=2;{}\x1b\\x", b64(&png(10, 20)));
    let term = screen(&run(&mut f, command.as_bytes()));
    let after = &term.grid()[Point::new(Line(0), Column(1))];
    assert_eq!(after.c, 'x');
    assert_eq!(after.fg, alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Red));
}

#[test]
fn chunked_uploads_query_place_and_delete() {
    let (mut f, store, replies) = filter();
    let data = b64(&png(10, 20));
    let (a, b) = data.split_at(8);
    let out = run(&mut f, format!("\x1b_Ga=t,f=100,i=3,m=1;{a}\x1b\\\x1b_Gm=0;{b}\x1b\\").as_bytes());
    assert!(out.is_empty(), "transmitting alone draws nothing");
    assert_eq!(replies.lock().unwrap().pop().as_deref(), Some("\x1b_Gi=3;OK\x1b\\"));

    let term = screen(&run(&mut f, b"\x1b_Ga=p,i=3,c=4,r=2,C=1\x1b\\"));
    let id = cell(&term, 0, 0).unwrap().0;
    assert_eq!(cell(&term, 1, 3), Some((id, 1, 3)), "placed over 4x2 cells");
    assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(0)), "C=1 leaves the cursor");
    assert_eq!(store.lock().unwrap().placement(id).unwrap().fit, super::Fit::Stretch);

    run(&mut f, b"\x1b_Ga=d,d=I,i=3\x1b\\");
    assert!(store.lock().unwrap().placement(id).is_none(), "deleted with its data");
    run(&mut f, b"\x1b_Ga=p,i=3\x1b\\");
    assert!(replies.lock().unwrap().pop().unwrap().contains("ENOENT"));

    // The support query apps send first, then DA1: answered in order, and DA1 shows SIXEL.
    replies.lock().unwrap().clear();
    run(&mut f, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c");
    assert_eq!(replies.lock().unwrap().as_slice(), ["\x1b_Gi=31;OK\x1b\\", "\x1b[?62;4;22c"]);
}

#[test]
fn bad_kitty_data_gets_an_error_and_draws_nothing() {
    let (mut f, _, replies) = filter();
    let out = run(&mut f, b"\x1b_Ga=T,f=100,i=9;bm90IGEgcG5n\x1b\\");
    assert!(out.is_empty());
    assert!(replies.lock().unwrap()[0].starts_with("\x1b_Gi=9;EBADPNG"));
}

#[test]
fn control_data_is_parsed() {
    let (c, payload) = kitty::parse(b"a=T,f=24,s=2,v=3,i=5,c=10,r=4,C=1,q=1,o=z;abc");
    assert_eq!((c.action, c.format, c.width, c.height, c.id), (b'T', 24, 2, 3, Some(5)));
    assert_eq!((c.cols, c.rows, c.cursor_stays, c.quiet, c.compressed), (Some(10), Some(4), true, 1, true));
    assert_eq!(payload, b"abc");
}

#[test]
fn raw_rgb_and_zlib_data_decode() {
    let rgb = [255u8, 0, 0, 0, 255, 0];
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(&rgb, 6);
    let (c, _) = kitty::parse(b"f=24,s=2,v=1,o=z");
    let image = kitty::decode(&c, b64(&packed).as_bytes()).unwrap();
    assert_eq!(image.get_pixel(1, 0).0, [0, 255, 0, 255]);
}

#[test]
fn a_sixel_image_decodes_and_sits_below_the_cursor_after() {
    // Two colours: red then (after a carriage return) blue in the bottom half, 4 px wide.
    // `F` paints the top three pixels of a band, `w` the bottom three.
    let data = b"0;1q\"1;1;4;6#1;2;100;0;0#2;2;0;0;100#1!4F$#2!4w";
    let image = sixel::decode(data).unwrap();
    assert_eq!(image.dimensions(), (4, 6));
    assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    assert_eq!(image.get_pixel(3, 5).0, [0, 0, 255, 255]);

    let (mut f, _, _) = filter();
    let mut sequence = b"ab\x1bP".to_vec();
    sequence.extend_from_slice(data);
    sequence.extend_from_slice(b"\x1b\\");
    let term = screen(&run(&mut f, &sequence));
    assert!(cell(&term, 0, 2).is_some());
    assert_eq!(term.grid().cursor.point, Point::new(Line(1), Column(2)), "the line below, at the image's column");
}

#[test]
fn an_iterm2_image_takes_its_size_in_cells() {
    let (mut f, store, _) = filter();
    let sequence =
        format!("\x1b]1337;File=name=eA==;inline=1;width=6;height=2;preserveAspectRatio=0:{}\x07", b64(&png(40, 40)));
    let term = screen(&run(&mut f, sequence.as_bytes()));
    let id = cell(&term, 0, 0).unwrap().0;
    let store = store.lock().unwrap();
    let p = store.placement(id).unwrap();
    assert_eq!((p.cols, p.rows, p.fit), (6, 2, super::Fit::Stretch));
    assert_eq!(term.grid().cursor.point.line, Line(2));
    // Not inline: nothing shown, and the sequence is consumed (iTerm2 would download it).
    let (mut f, _, _) = filter();
    assert!(run(&mut f, format!("\x1b]1337;File=inline=0:{}\x07", b64(&png(4, 4))).as_bytes()).is_empty());
}

#[test]
fn the_cell_size_is_answered_and_other_csi_passes() {
    let (mut f, _, replies) = filter();
    assert_eq!(run(&mut f, b"\x1b[16t\x1b[14t\x1b[5n"), b"\x1b[14t\x1b[5n");
    assert_eq!(replies.lock().unwrap().as_slice(), ["\x1b[6;20;10t"]);
}

#[test]
fn a_placeholder_needs_its_marker() {
    use alacritty_terminal::vte::ansi::{Color, Rgb};
    let fg = Color::Spec(Rgb { r: 0, g: 5, b: 1 });
    assert_eq!(decode_cell(PLACEHOLDER, fg, Color::Spec(Rgb { r: 0xA5, g: 0, b: 3 })), Some((5, 1, 3)));
    assert_eq!(decode_cell(PLACEHOLDER, fg, Color::Spec(Rgb { r: 1, g: 0, b: 3 })), None);
    assert_eq!(decode_cell('x', fg, Color::Spec(Rgb { r: 0xA5, g: 0, b: 3 })), None);
    let _ = Term::<VoidListener>::new(Config::default(), &TermSize::new(2, 2), VoidListener).grid().columns();
}
