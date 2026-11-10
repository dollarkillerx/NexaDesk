//! Reads the system cursor the way the host does (§6.9). Reading only: nothing here
//! moves, hides or changes the cursor.
#![cfg(target_os = "macos")]

use nexa_media::CursorSource;
use nexa_media::macos::SystemCursorSource;

#[test]
fn the_system_cursor_reads_as_straight_rgba_off_the_main_thread() {
    let mut source = SystemCursorSource::new();
    let first = std::thread::spawn(move || {
        let a = source.sample();
        let b = source.sample();
        (a, b)
    })
    .join()
    .unwrap();
    let (a, b) = first;
    // Without a window server there may be no cursor to read; that is not an error.
    let Some(image) = a.image else {
        println!("no cursor image available");
        return;
    };
    println!("cursor {}x{} px at {}x, hot spot ({}, {})", image.width, image.height, image.scale, image.hot_x, image.hot_y);
    assert!(image.width > 0 && image.width <= 512 && image.height > 0 && image.height <= 512);
    assert_eq!(image.rgba.len(), (image.width * image.height * 4) as usize);
    assert!(image.scale >= 1.0, "scale {}", image.scale);
    assert!(image.hot_x >= 0.0 && image.hot_x <= f64::from(image.width));
    assert!(image.rgba.chunks(4).any(|p| p[3] != 0), "something is opaque");
    // An unchanged cursor comes back as the same allocation, so the tracker skips it.
    if let Some(again) = b.image
        && *again == *image
    {
        assert!(std::sync::Arc::ptr_eq(&again, &image));
    }
    assert!(a.position.is_some());
}
