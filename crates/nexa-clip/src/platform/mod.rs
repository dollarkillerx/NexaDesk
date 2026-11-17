//! OS-specific clipboard backends.

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
mod windows_image;

/// A picture file the OS can read (JPEG, TIFF, BMP…) as PNG, with its own codecs: a
/// browser sends a large photo as JPEG, and clipboards are given PNG. `None` if it
/// cannot be read, or on a platform without them.
pub fn png_of_picture(file: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    return windows_image::png_of_picture(file);
    #[cfg(target_os = "macos")]
    return macos::png_of_picture(file);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = file;
        None
    }
}
