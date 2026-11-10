//! Every test here uses a privately named pasteboard, released on drop. The
//! general pasteboard is out of reach by construction: `MacClipboardBackend::
//! general` does not exist when compiling tests.

use std::sync::atomic::{AtomicUsize, Ordering};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData};

use super::*;

/// A minimal valid PNG: a 1x1 opaque red pixel.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

struct PrivateBoard(Retained<NSPasteboard>);

impl PrivateBoard {
    fn new() -> Self {
        Self(NSPasteboard::pasteboardWithUniqueName())
    }

    /// Simulates another application writing one item.
    fn foreign_write(&self, entries: &[(&str, &[u8])]) {
        let item = NSPasteboardItem::new();
        for (name, bytes) in entries {
            assert!(item.setData_forType(&NSData::with_bytes(bytes), &ns(name)));
        }
        self.0.clearContents();
        let writing: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(item);
        assert!(
            self.0
                .writeObjects(&NSArray::from_retained_slice(&[writing]))
        );
    }

    fn raw(&self, name: &str) -> Option<Vec<u8>> {
        self.0.dataForType(&ns(name)).map(|d| d.to_vec())
    }

    fn string(&self, name: &str) -> Option<String> {
        self.0.stringForType(&ns(name)).map(|s| s.to_string())
    }
}

impl Drop for PrivateBoard {
    fn drop(&mut self) {
        // SAFETY: `releaseGlobally` takes no arguments; the handle stays valid.
        let () = unsafe { objc2::msg_send![&*self.0, releaseGlobally] };
    }
}

fn no_resolver() -> PromiseResolver {
    Arc::new(|_, _| panic!("no promise expected"))
}

fn backend(board: &PrivateBoard) -> MacClipboardBackend {
    MacClipboardBackend::new(board.0.clone(), no_resolver())
}

fn generation(n: u64) -> Gen {
    (n, DeviceId([n as u8; 16]))
}

#[test]
fn inline_text_round_trips() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let text = "hello\nwörld".as_bytes();
    clip.write_promise(
        generation(1),
        &[FormatInfo::of(ClipKind::Text, text)],
        &[(ClipKind::Text, Bytes::from_static(text))],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Text).unwrap(), text);
    assert_eq!(board.raw(TEXT_TYPE).unwrap(), text);
}

#[test]
fn inline_html_round_trips() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let html = b"<b>bold</b>";
    clip.write_promise(
        generation(1),
        &[FormatInfo::of(ClipKind::Html, html)],
        &[(ClipKind::Html, Bytes::from_static(html))],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Html).unwrap(), html);
    assert_eq!(board.raw(HTML_TYPE).unwrap(), html);
}

#[test]
fn inline_rtf_round_trips() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let rtf = br"{\rtf1\ansi plain}";
    clip.write_promise(
        generation(1),
        &[FormatInfo::of(ClipKind::Rtf, rtf)],
        &[(ClipKind::Rtf, Bytes::from_static(rtf))],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Rtf).unwrap(), rtf);
    assert_eq!(board.raw(RTF_TYPE).unwrap(), rtf);
}

#[test]
fn inline_png_round_trips_and_also_offers_tiff() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    clip.write_promise(
        generation(1),
        &[FormatInfo::of(ClipKind::Png, PNG_1X1)],
        &[(ClipKind::Png, Bytes::from_static(PNG_1X1))],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Png).unwrap(), PNG_1X1);
    assert_eq!(board.raw(PNG_TYPE).unwrap(), PNG_1X1);
    let tiff = board.raw(TIFF_TYPE).expect("tiff offered alongside png");
    assert!(tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*"));
}

#[test]
fn every_format_can_be_written_together() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let text = b"text";
    let html = b"<i>html</i>";
    let rtf = b"{\\rtf1 rtf}";
    clip.write_promise(
        generation(3),
        &[
            FormatInfo::of(ClipKind::Text, text),
            FormatInfo::of(ClipKind::Html, html),
            FormatInfo::of(ClipKind::Rtf, rtf),
            FormatInfo::of(ClipKind::Png, PNG_1X1),
        ],
        &[
            (ClipKind::Text, Bytes::from_static(text)),
            (ClipKind::Html, Bytes::from_static(html)),
            (ClipKind::Rtf, Bytes::from_static(rtf)),
            (ClipKind::Png, Bytes::from_static(PNG_1X1)),
        ],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Text).unwrap(), text);
    assert_eq!(clip.read(ClipKind::Html).unwrap(), html);
    assert_eq!(clip.read(ClipKind::Rtf).unwrap(), rtf);
    assert_eq!(clip.read(ClipKind::Png).unwrap(), PNG_1X1);
}

#[test]
fn foreign_content_is_listed_with_canonical_hashes() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    board.foreign_write(&[
        (TEXT_TYPE, b"one\r\ntwo"),
        (HTML_TYPE, b"<p>x</p>"),
        (RTF_TYPE, b"{\\rtf1 x}"),
        (PNG_TYPE, PNG_1X1),
    ]);
    let formats = clip.read_formats().unwrap();
    assert_eq!(
        formats,
        vec![
            FormatInfo::of(ClipKind::Text, b"one\ntwo"),
            FormatInfo::of(ClipKind::Html, b"<p>x</p>"),
            FormatInfo::of(ClipKind::Rtf, b"{\\rtf1 x}"),
            FormatInfo::of(ClipKind::Png, PNG_1X1),
        ]
    );
    assert_eq!(clip.read(ClipKind::Text).unwrap(), b"one\ntwo");
}

#[test]
fn tiff_only_clipboard_is_read_as_png() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let tiff = png_to_tiff(PNG_1X1).unwrap();
    board.foreign_write(&[(TIFF_TYPE, &tiff)]);
    let png = clip.read(ClipKind::Png).unwrap();
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_eq!(clip.read_formats().unwrap()[0].kind, ClipKind::Png);
}

#[test]
fn missing_format_reports_not_available() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    board.foreign_write(&[(TEXT_TYPE, b"x")]);
    assert_eq!(
        clip.read(ClipKind::Rtf),
        Err(ClipError::NotAvailable(ClipKind::Rtf))
    );
}

#[test]
fn empty_clipboard_has_no_formats() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    assert_eq!(clip.read_formats().unwrap(), vec![]);
}

#[test]
fn promised_data_is_fulfilled_on_request() {
    let board = PrivateBoard::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let resolver: PromiseResolver = Arc::new(move |generation_seen, kind| {
        assert_eq!(generation_seen, generation(7));
        assert_eq!(kind, ClipKind::Text);
        counter.fetch_add(1, Ordering::SeqCst);
        Some(b"from the peer".to_vec())
    });
    let mut clip = MacClipboardBackend::new(board.0.clone(), resolver);
    clip.write_promise(
        generation(7),
        &[FormatInfo::of(ClipKind::Text, b"from the peer")],
        &[],
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing rendered early");
    assert_eq!(
        clip.read(ClipKind::Text),
        Err(ClipError::Deferred(ClipKind::Text))
    );

    // Another application asks for the data.
    assert_eq!(board.raw(TEXT_TYPE).unwrap(), b"from the peer");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Now that it is rendered, a read returns it instead of a promise.
    assert_eq!(clip.read(ClipKind::Text).unwrap(), b"from the peer");
}

#[test]
fn a_failed_promise_renders_empty() {
    let board = PrivateBoard::new();
    let resolver: PromiseResolver = Arc::new(|_, _| None);
    let mut clip = MacClipboardBackend::new(board.0.clone(), resolver);
    clip.write_promise(generation(1), &[FormatInfo::of(ClipKind::Html, b"x")], &[])
        .unwrap();
    assert_eq!(board.raw(HTML_TYPE).unwrap_or_default(), b"");
}

#[test]
fn promised_png_serves_both_png_and_tiff_from_one_fetch() {
    let board = PrivateBoard::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let resolver: PromiseResolver = Arc::new(move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(PNG_1X1.to_vec())
    });
    let mut clip = MacClipboardBackend::new(board.0.clone(), resolver);
    clip.write_promise(
        generation(2),
        &[FormatInfo::of(ClipKind::Png, PNG_1X1)],
        &[],
    )
    .unwrap();
    assert_eq!(board.raw(PNG_TYPE).unwrap(), PNG_1X1);
    let tiff = board.raw(TIFF_TYPE).unwrap();
    assert!(tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_promise_is_listed_without_being_rendered() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board); // panics if the resolver runs
    let info = FormatInfo::of(ClipKind::Rtf, b"secret");
    clip.write_promise(generation(9), &[info], &[]).unwrap();
    assert_eq!(clip.read_formats().unwrap(), vec![info]);
    let snapshot = clip.snapshot();
    assert_eq!(snapshot.formats, vec![info]);
    assert!(snapshot.owner_marker);
    assert!(snapshot.data.is_empty());
}

#[test]
fn inline_and_promised_formats_mix() {
    let board = PrivateBoard::new();
    let resolver: PromiseResolver = Arc::new(|_, _| Some(b"<b>later</b>".to_vec()));
    let mut clip = MacClipboardBackend::new(board.0.clone(), resolver);
    clip.write_promise(
        generation(2),
        &[
            FormatInfo::of(ClipKind::Text, b"now"),
            FormatInfo::of(ClipKind::Html, b"<b>later</b>"),
        ],
        &[(ClipKind::Text, Bytes::from_static(b"now"))],
    )
    .unwrap();
    assert_eq!(clip.read(ClipKind::Text).unwrap(), b"now");
    assert_eq!(
        clip.read(ClipKind::Html),
        Err(ClipError::Deferred(ClipKind::Html))
    );
    assert_eq!(board.raw(HTML_TYPE).unwrap(), b"<b>later</b>");
}

#[test]
fn owner_marker_identifies_our_writes() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    let g = generation(11);
    clip.write_promise(
        g,
        &[FormatInfo::of(ClipKind::Text, b"ours")],
        &[(ClipKind::Text, Bytes::from_static(b"ours"))],
    )
    .unwrap();
    assert_eq!(clip.owner_on_board(), Some(g));
    let snapshot = clip
        .poll()
        .unwrap()
        .expect("our own write counts as a change");
    assert!(snapshot.owner_marker);
    assert_eq!(snapshot.formats.len(), 1);
}

#[test]
fn a_third_party_write_has_no_owner_marker() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    clip.write_promise(
        generation(11),
        &[FormatInfo::of(ClipKind::Text, b"ours")],
        &[(ClipKind::Text, Bytes::from_static(b"ours"))],
    )
    .unwrap();
    clip.poll().unwrap();
    board.foreign_write(&[(TEXT_TYPE, b"someone else")]);
    let snapshot = clip.poll().unwrap().expect("change detected");
    assert!(!snapshot.owner_marker);
    assert_eq!(
        snapshot.formats,
        vec![FormatInfo::of(ClipKind::Text, b"someone else")]
    );
    assert_eq!(snapshot.data[0].1.as_ref(), b"someone else");
}

#[test]
fn owner_marker_round_trips_a_full_generation() {
    let g: Gen = (u64::MAX, DeviceId([0xAB; 16]));
    assert_eq!(parse_owner(&owner_value(g)), Some(g));
    assert_eq!(parse_owner("garbage"), None);
    assert_eq!(parse_owner("1:abc"), None);
}

#[test]
fn a_clipboard_marked_by_another_instance_is_still_an_owner_echo() {
    let board = PrivateBoard::new();
    let mut other_instance = backend(&board);
    other_instance
        .write_promise(
            generation(5),
            &[FormatInfo::of(ClipKind::Text, b"x")],
            &[(ClipKind::Text, Bytes::from_static(b"x"))],
        )
        .unwrap();
    let clip = backend(&board);
    let snapshot = clip.snapshot();
    assert!(snapshot.owner_marker, "marker alone identifies an echo");
    assert_eq!(
        snapshot.formats,
        vec![],
        "formats are only known to the writer"
    );
}

#[test]
fn poll_reports_nothing_without_a_change() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    assert_eq!(clip.poll().unwrap(), None);
    board.foreign_write(&[(TEXT_TYPE, b"x")]);
    assert!(clip.poll().unwrap().is_some());
    assert_eq!(clip.poll().unwrap(), None);
}

#[test]
fn concealed_type_sets_the_sensitive_flag() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    board.foreign_write(&[(TEXT_TYPE, b"hunter2"), (CONCEALED_TYPE, b"")]);
    let snapshot = clip.poll().unwrap().unwrap();
    assert!(snapshot.sensitive);
}

#[test]
fn ordinary_content_is_not_sensitive() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    board.foreign_write(&[(TEXT_TYPE, b"public")]);
    assert!(!clip.poll().unwrap().unwrap().sensitive);
}

#[test]
fn a_write_replaces_earlier_content() {
    let board = PrivateBoard::new();
    let mut clip = backend(&board);
    board.foreign_write(&[(HTML_TYPE, b"<old/>")]);
    clip.write_promise(
        generation(1),
        &[FormatInfo::of(ClipKind::Text, b"new")],
        &[(ClipKind::Text, Bytes::from_static(b"new"))],
    )
    .unwrap();
    assert_eq!(board.raw(HTML_TYPE), None);
    assert_eq!(board.string(TEXT_TYPE).as_deref(), Some("new"));
}

#[test]
fn format_table_matches_the_design() {
    assert_eq!(types_of(ClipKind::Text), &["public.utf8-plain-text"]);
    assert_eq!(types_of(ClipKind::Html), &["public.html"]);
    assert_eq!(types_of(ClipKind::Rtf), &["public.rtf"]);
    assert_eq!(types_of(ClipKind::Png), &["public.png", "public.tiff"]);
    assert_eq!(OWNER_TYPE, "app.nexadesk.owner");
    assert_eq!(CONCEALED_TYPE, "org.nspasteboard.ConcealedType");
}

#[test]
fn poll_interval_matches_the_design() {
    assert_eq!(POLL_INTERVAL, Duration::from_millis(100));
}
