/// X11 coordinate: origin at top-left, Y increases downward.
/// All values are in physical (backing store) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct X11Point {
    pub x: i16,
    pub y: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct X11Rect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// macOS coordinate: origin at bottom-left of primary screen, Y increases upward.
/// Values are in "points" (logical pixels).
#[derive(Debug, Clone, Copy, Default)]
pub struct MacOSPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacOSRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Per-screen coordinate context, capturing screen geometry and scale factor.
/// This is the foundation of all coordinate transforms in the system.
///
/// Key insight: NSView.isFlipped = true makes the view use top-left origin,
/// matching X11 convention and eliminating coordinate bugs within the view.
/// However, window→screen conversion still requires Y-flip because macOS
/// screen coordinates have bottom-left origin.
pub struct CoordContext {
    /// Height of the primary screen in points (used for Y-flip in screen coords).
    pub primary_screen_height_points: f64,
    /// Retina scale factor (1.0 for non-retina, 2.0 for standard retina).
    pub scale_factor: f64,
}

impl CoordContext {
    pub fn new(screen_height_points: f64, scale_factor: f64) -> Self {
        Self {
            primary_screen_height_points: screen_height_points,
            scale_factor,
        }
    }

    /// Convert X11 physical pixels to macOS logical points.
    #[inline]
    pub fn px_to_points(&self, px: f64) -> f64 {
        px / self.scale_factor
    }

    /// Convert macOS logical points to X11 physical pixels.
    #[inline]
    pub fn points_to_px(&self, points: f64) -> f64 {
        points * self.scale_factor
    }

    /// Convert X11 window-local coordinates to macOS screen coordinates.
    ///
    /// This is the critical path for IME candidate window placement.
    ///
    /// The NSView has isFlipped=true, so view-local coordinates already use
    /// top-left origin. But macOS screen coordinates use bottom-left origin.
    ///
    /// Steps:
    /// 1. Convert X11 physical pixels to logical points (divide by scale_factor)
    /// 2. Add window frame origin (the window's position in screen coords)
    /// 3. For Y: the window frame origin.y is the BOTTOM edge in macOS coords,
    ///    so we need: screen_y = frame.y + frame.height - logical_y
    pub fn x11_to_macos_screen(
        &self,
        point: X11Point,
        window_frame: MacOSRect,
    ) -> MacOSPoint {
        let logical_x = self.px_to_points(point.x as f64);
        let logical_y = self.px_to_points(point.y as f64);

        MacOSPoint {
            x: window_frame.x + logical_x,
            // window_frame.y is the bottom edge in macOS screen coords.
            // window_frame.y + window_frame.height is the top edge.
            // X11's point.y is distance from top, so:
            y: window_frame.y + window_frame.height - logical_y,
        }
    }

    /// Convert macOS screen coordinates to X11 root window coordinates.
    pub fn macos_screen_to_x11(&self, point: MacOSPoint) -> X11Point {
        X11Point {
            x: self.points_to_px(point.x) as i16,
            // macOS Y increases upward, X11 Y increases downward
            y: self.points_to_px(self.primary_screen_height_points - point.y) as i16,
        }
    }

    /// Convert X11 caret position to macOS screen rect for IME placement.
    ///
    /// This is called from firstRectForCharacterRange: which macOS uses
    /// to determine where to place the IME candidate window.
    ///
    /// Returns a rect in macOS screen coordinates (bottom-left origin).
    pub fn x11_caret_to_macos_screen_rect(
        &self,
        caret: X11Point,
        line_height_px: u16,
        window_frame: MacOSRect,
    ) -> MacOSRect {
        let logical_x = self.px_to_points(caret.x as f64);
        let logical_y = self.px_to_points(caret.y as f64);
        let logical_height = self.px_to_points(line_height_px as f64);

        // The caret rect bottom in macOS screen coords:
        // window bottom + window height - caret_y - line_height
        let screen_x = window_frame.x + logical_x;
        let screen_y = window_frame.y + window_frame.height - logical_y - logical_height;

        MacOSRect {
            x: screen_x,
            y: screen_y,
            width: 1.0, // Caret width (1 point)
            height: logical_height,
        }
    }

    /// Compute X11 root window dimensions from screen geometry.
    pub fn screen_dimensions_px(&self) -> (u16, u16) {
        let w = self.points_to_px(self.primary_screen_height_points); // placeholder
        // In practice, we'd use the actual screen width too
        (w as u16, self.points_to_px(self.primary_screen_height_points) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_px_to_points_retina() {
        let ctx = CoordContext::new(900.0, 2.0);
        assert_eq!(ctx.px_to_points(100.0), 50.0);
        assert_eq!(ctx.points_to_px(50.0), 100.0);
    }

    #[test]
    fn test_px_to_points_non_retina() {
        let ctx = CoordContext::new(1080.0, 1.0);
        assert_eq!(ctx.px_to_points(100.0), 100.0);
    }

    #[test]
    fn test_x11_to_macos_screen() {
        // 2x Retina, screen height 900pt
        let ctx = CoordContext::new(900.0, 2.0);

        // Window at macOS position (100, 200) with size 400x300 (in points)
        let window_frame = MacOSRect {
            x: 100.0,
            y: 200.0,
            width: 400.0,
            height: 300.0,
        };

        // X11 point (0, 0) in physical pixels = top-left of window
        let result = ctx.x11_to_macos_screen(X11Point { x: 0, y: 0 }, window_frame);
        assert_eq!(result.x, 100.0); // left edge of window
        assert_eq!(result.y, 500.0); // top edge = bottom + height = 200 + 300

        // X11 point (200, 100) in physical pixels = (100pt, 50pt) from top-left
        let result = ctx.x11_to_macos_screen(X11Point { x: 200, y: 100 }, window_frame);
        assert_eq!(result.x, 200.0); // 100 + 100pt
        assert_eq!(result.y, 450.0); // 200 + 300 - 50pt
    }

    #[test]
    fn test_caret_to_macos_screen_rect() {
        let ctx = CoordContext::new(900.0, 2.0);

        let window_frame = MacOSRect {
            x: 100.0,
            y: 200.0,
            width: 400.0,
            height: 300.0,
        };

        // Caret at X11 (40, 60) with line height 32px
        let rect = ctx.x11_caret_to_macos_screen_rect(
            X11Point { x: 40, y: 60 },
            32,
            window_frame,
        );

        // logical position: (20pt, 30pt from top)
        // line height: 16pt
        assert_eq!(rect.x, 120.0); // 100 + 20
        assert_eq!(rect.y, 454.0); // 200 + 300 - 30 - 16 = 454
        assert_eq!(rect.width, 1.0);
        assert_eq!(rect.height, 16.0);
    }
}
