// HiDPI (Retina) display handling
// Manages scale factor detection and coordinate scaling

/// Detect the scale factor of the main screen.
/// On macOS, this queries NSScreen.mainScreen.backingScaleFactor.
/// On other platforms, defaults to 1.0.
pub fn detect_scale_factor() -> f64 {
    #[cfg(target_os = "macos")]
    {
        // TODO: Query NSScreen.mainScreen.backingScaleFactor via objc2
        2.0 // Default to 2x for modern Macs
    }

    #[cfg(not(target_os = "macos"))]
    {
        1.0
    }
}

/// Get the main screen dimensions in logical points.
pub fn get_screen_dimensions_points() -> (f64, f64) {
    #[cfg(target_os = "macos")]
    {
        // TODO: Query NSScreen.mainScreen.frame via objc2
        (1440.0, 900.0) // Default MacBook Pro dimensions
    }

    #[cfg(not(target_os = "macos"))]
    {
        (1920.0, 1080.0)
    }
}
