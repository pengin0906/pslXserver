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
/// On macOS Retina displays, backingScaleFactor is always 2.0 regardless
/// of the "Looks like" display scaling setting. This is what CALayer uses.
pub fn detect_scale_factor() -> f64 {
    #[cfg(target_os = "macos")]
    {
        2.0 // Retina Macs always use 2.0 backingScaleFactor
    }

    #[cfg(target_os = "ios")]
    {
        // Query UIScreen.mainScreen.scale for actual device scale factor
        unsafe {
            use objc2::msg_send;
            let screen: *mut objc2::runtime::AnyObject =
                msg_send![objc2::class!(UIScreen), mainScreen];
            if !screen.is_null() {
                let scale: f64 = msg_send![screen, scale];
                if scale > 0.0 {
                    return scale;
                }
            }
        }
        2.0 // fallback
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        1.0
    }
}

/// Get the main screen dimensions in logical points.
/// Font scaling (2x bitmap) handles readability. contentsScale=1.0 for correct clicks.
pub fn get_screen_dimensions_pixels() -> (u16, u16) {
    #[cfg(target_os = "macos")]
    {
        unsafe {
            let display = CGMainDisplayID();
            let w = CGDisplayPixelsWide(display) as u16;
            let h = CGDisplayPixelsHigh(display) as u16;
            (w, h)
        }
    }

    #[cfg(target_os = "ios")]
    {
        // Query UIScreen.mainScreen.bounds × scale for actual pixel dimensions.
        // HiDPI: X11 screen dimensions are in physical pixels (like macOS).
        use objc2::msg_send;
        use objc2::encode::{Encode, Encoding};

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGRect { origin: [f64; 2], size: [f64; 2] }
        unsafe impl Encode for CGRect {
            const ENCODING: Encoding = Encoding::Struct("CGRect", &[
                Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
                Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
            ]);
        }

        unsafe {
            let screen: *mut objc2::runtime::AnyObject =
                msg_send![objc2::class!(UIScreen), mainScreen];
            if !screen.is_null() {
                let bounds: CGRect = msg_send![screen, bounds];
                let scale = detect_scale_factor();
                // Multiply by scale factor for HiDPI pixel dimensions (like macOS)
                let w = (bounds.size[0] * scale) as u16;
                let h = (bounds.size[1] * scale) as u16;
                if w > 0 && h > 0 {
                    return (w, h);
                }
            }
        }
        // Fallback: iPad Pro 13" dimensions in pixels (2x)
        (2732, 2048)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        (1920, 1080)
    }
}
