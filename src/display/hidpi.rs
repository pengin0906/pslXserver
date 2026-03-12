// HiDPI (Retina) display handling
// Manages scale factor detection and coordinate scaling

#[cfg(target_os = "macos")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut std::ffi::c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut std::ffi::c_void);
}

/// Detect the scale factor of the main screen.
pub fn detect_scale_factor() -> f64 {
    #[cfg(target_os = "macos")]
    {
        // TODO: Query NSScreen.mainScreen.backingScaleFactor via objc2
        2.0 // Default to 2x for modern Macs
    }

    #[cfg(target_os = "ios")]
    {
        // iOS Retina displays are typically 2x or 3x
        2.0
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        1.0
    }
}

/// Get the main screen dimensions in logical points.
/// Since our IOSurface uses contentsScale=1.0 (point-based rendering),
/// all X11 coordinates should be in points, not physical pixels.
pub fn get_screen_dimensions_pixels() -> (u16, u16) {
    #[cfg(target_os = "macos")]
    {
        unsafe {
            let display = CGMainDisplayID();
            // Use logical (point) dimensions, not physical pixels.
            // This matches our point-based IOSurface rendering.
            let w = CGDisplayPixelsWide(display) as u16;
            let h = CGDisplayPixelsHigh(display) as u16;
            (w, h)
        }
    }

    #[cfg(target_os = "ios")]
    {
        // Return logical screen size (points); will be set properly at runtime
        // Default to iPhone 15 Pro dimensions
        (393, 852)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        (1920, 1080)
    }
}
