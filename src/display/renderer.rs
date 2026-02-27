// Core Graphics renderer for X11 drawing commands
// Maps RenderCommand variants to CGContext operations

use crate::display::RenderCommand;

/// Apply a render command to a pixel buffer (software rendering fallback).
/// This works on all platforms for testing.
/// On macOS, we'll use CGContext instead.
pub fn render_to_buffer(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    command: &RenderCommand,
) {
    match command {
        RenderCommand::FillRectangle { x, y, width: w, height: h, color } => {
            let x = *x as i32;
            let y = *y as i32;
            let w = *w as i32;
            let h = *h as i32;

            let r = ((*color >> 16) & 0xFF) as u8;
            let g = ((*color >> 8) & 0xFF) as u8;
            let b = (*color & 0xFF) as u8;
            let a = 0xFF_u8;

            for py in y.max(0)..((y + h).min(height as i32)) {
                for px in x.max(0)..((x + w).min(width as i32)) {
                    let offset = (py as u32 * stride + px as u32 * 4) as usize;
                    if offset + 3 < buffer.len() {
                        // BGRA format (macOS native)
                        buffer[offset] = b;
                        buffer[offset + 1] = g;
                        buffer[offset + 2] = r;
                        buffer[offset + 3] = a;
                    }
                }
            }
        }
        RenderCommand::ClearArea { x, y, width: w, height: h, bg_color } => {
            // Same as FillRectangle with background color
            let fill = RenderCommand::FillRectangle {
                x: *x, y: *y, width: *w, height: *h, color: *bg_color,
            };
            render_to_buffer(buffer, width, height, stride, &fill);
        }
        RenderCommand::DrawLine { x1, y1, x2, y2, color, .. } => {
            // Bresenham's line algorithm
            let mut x = *x1 as i32;
            let mut y = *y1 as i32;
            let x2 = *x2 as i32;
            let y2 = *y2 as i32;

            let dx = (x2 - x).abs();
            let dy = -(y2 - y).abs();
            let sx = if x < x2 { 1 } else { -1 };
            let sy = if y < y2 { 1 } else { -1 };
            let mut err = dx + dy;

            let r = ((*color >> 16) & 0xFF) as u8;
            let g = ((*color >> 8) & 0xFF) as u8;
            let b = (*color & 0xFF) as u8;

            loop {
                if x >= 0 && x < width as i32 && y >= 0 && y < height as i32 {
                    let offset = (y as u32 * stride + x as u32 * 4) as usize;
                    if offset + 3 < buffer.len() {
                        buffer[offset] = b;
                        buffer[offset + 1] = g;
                        buffer[offset + 2] = r;
                        buffer[offset + 3] = 0xFF;
                    }
                }

                if x == x2 && y == y2 { break; }
                let e2 = 2 * err;
                if e2 >= dy { err += dy; x += sx; }
                if e2 <= dx { err += dx; y += sy; }
            }
        }
        _ => {
            // Other commands will be implemented in Phase 3
            log::trace!("Unimplemented render command: {:?}", command);
        }
    }
}
