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
            let x0 = (*x as i32).max(0) as u32;
            let y0 = (*y as i32).max(0) as u32;
            let x1 = ((*x as i32 + *w as i32) as u32).min(width);
            let y1 = ((*y as i32 + *h as i32) as u32).min(height);

            if x0 >= x1 || y0 >= y1 { return; }

            // BGRA pixel as u32 for fast fill
            let pixel_u32: u32 = (*color & 0x00FFFFFF) | 0xFF000000;
            let row_bytes = (x1 - x0) as usize * 4;

            // Fill first row
            let first_start = (y0 * stride + x0 * 4) as usize;
            let first_end = first_start + row_bytes;
            if first_end > buffer.len() { return; }
            {
                // Write as u32 for speed (4x fewer stores)
                let row = &mut buffer[first_start..first_end];
                // Safety: buffer is u8, aligned to 4 on IOSurface; write u32 via bytes
                for chunk in row.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&pixel_u32.to_ne_bytes());
                }
            }

            // Copy first row to remaining rows (memcpy is much faster than per-pixel fill)
            for py in (y0 + 1)..y1 {
                let dst_start = (py * stride + x0 * 4) as usize;
                let dst_end = dst_start + row_bytes;
                if dst_end > buffer.len() { break; }
                buffer.copy_within(first_start..first_end, dst_start);
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

            // BGRA pixel
            let pixel: [u8; 4] = [
                (*color & 0xFF) as u8,
                ((*color >> 8) & 0xFF) as u8,
                ((*color >> 16) & 0xFF) as u8,
                0xFF,
            ];

            let w = width as i32;
            let h = height as i32;

            loop {
                if x >= 0 && x < w && y >= 0 && y < h {
                    let offset = (y as usize) * (stride as usize) + (x as usize) * 4;
                    // Safety: bounds already checked above
                    buffer[offset..offset + 4].copy_from_slice(&pixel);
                }

                if x == x2 && y == y2 { break; }
                let e2 = 2 * err;
                if e2 >= dy { err += dy; x += sx; }
                if e2 <= dx { err += dx; y += sy; }
            }
        }
        RenderCommand::PutImage { x, y, width: w, height: h, depth, format, data } => {
            let dst_x = (*x as i32).max(0) as u32;
            let dst_y = (*y as i32).max(0) as u32;
            let src_w = *w as u32;
            let src_h = *h as u32;

            if *format == 2 && *depth == 24 {
                // ZPixmap format, 24-bit depth (stored as 32-bit BGRX in X11)
                let src_stride = src_w * 4;
                for row in 0..src_h {
                    let dy = dst_y + row;
                    if dy >= height { break; }
                    let src_off = (row * src_stride) as usize;
                    let dst_off = (dy * stride + dst_x * 4) as usize;
                    let copy_w = src_w.min(width.saturating_sub(dst_x)) as usize;
                    let src_end = src_off + copy_w * 4;
                    let dst_end = dst_off + copy_w * 4;
                    if src_end <= data.len() && dst_end <= buffer.len() {
                        // X11 sends BGRX, IOSurface expects BGRA — just copy and set alpha
                        let src_row = &data[src_off..src_end];
                        let dst_row = &mut buffer[dst_off..dst_end];
                        dst_row.copy_from_slice(src_row);
                        // Set alpha to 0xFF for every pixel
                        for chunk in dst_row.chunks_exact_mut(4) {
                            chunk[3] = 0xFF;
                        }
                    }
                }
            } else if *format == 2 && *depth == 32 {
                // ZPixmap 32-bit (BGRA)
                let src_stride = src_w * 4;
                for row in 0..src_h {
                    let dy = dst_y + row;
                    if dy >= height { break; }
                    let src_off = (row * src_stride) as usize;
                    let dst_off = (dy * stride + dst_x * 4) as usize;
                    let copy_w = src_w.min(width.saturating_sub(dst_x)) as usize;
                    let src_end = src_off + copy_w * 4;
                    let dst_end = dst_off + copy_w * 4;
                    if src_end <= data.len() && dst_end <= buffer.len() {
                        buffer[dst_off..dst_end].copy_from_slice(&data[src_off..src_end]);
                    }
                }
            } else {
                log::debug!("PutImage: unsupported format={} depth={}", format, depth);
            }
        }
        RenderCommand::DrawRectangle { x, y, width: w, height: h, color, .. } => {
            let pixel: [u8; 4] = [
                (*color & 0xFF) as u8,
                ((*color >> 8) & 0xFF) as u8,
                ((*color >> 16) & 0xFF) as u8,
                0xFF,
            ];
            let x0 = *x as i32;
            let y0 = *y as i32;
            let x1 = x0 + *w as i32;
            let y1 = y0 + *h as i32;
            // Top and bottom edges
            for px in x0..=x1 {
                set_pixel(buffer, width, height, stride, px, y0, &pixel);
                set_pixel(buffer, width, height, stride, px, y1, &pixel);
            }
            // Left and right edges
            for py in y0..=y1 {
                set_pixel(buffer, width, height, stride, x0, py, &pixel);
                set_pixel(buffer, width, height, stride, x1, py, &pixel);
            }
        }
        RenderCommand::FillArc { x, y, width: w, height: h, angle1, angle2, color } => {
            fill_arc(buffer, width, height, stride,
                     *x, *y, *w, *h, *angle1, *angle2, *color);
        }
        RenderCommand::DrawArc { x, y, width: w, height: h, angle1, angle2, color, .. } => {
            draw_arc(buffer, width, height, stride,
                     *x, *y, *w, *h, *angle1, *angle2, *color);
        }
        RenderCommand::DrawText { x, y, text, font_id: _, color, bg_color } => {
            log::debug!("DrawText at ({},{}) len={} fg=0x{:06X} bg={:?}", x, y, text.len(), color, bg_color);
            draw_text_bitmap(buffer, width, height, stride,
                              *x, *y, text, *color, *bg_color);
        }
        RenderCommand::CopyArea { src_x, src_y, dst_x, dst_y, width: w, height: h } => {
            copy_area(buffer, width, height, stride,
                      *src_x, *src_y, *dst_x, *dst_y, *w, *h);
        }
        RenderCommand::FillPolygon { points, color } => {
            fill_polygon(buffer, width, height, stride, points, *color);
        }
        _ => {
            log::trace!("Unimplemented render command: {:?}", command);
        }
    }
}

/// Copy a rectangular region within the same buffer (for scrolling).
/// Handles overlapping regions correctly by using a temporary buffer.
fn copy_area(
    buffer: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    stride: u32,
    src_x: i16, src_y: i16,
    dst_x: i16, dst_y: i16,
    width: u16, height: u16,
) {
    let w = width as u32;
    let h = height as u32;

    // Clip to buffer bounds
    let sx0 = (src_x as i32).max(0) as u32;
    let sy0 = (src_y as i32).max(0) as u32;
    let dx0 = (dst_x as i32).max(0) as u32;
    let dy0 = (dst_y as i32).max(0) as u32;

    let actual_w = w.min(buf_w.saturating_sub(sx0)).min(buf_w.saturating_sub(dx0));
    let actual_h = h.min(buf_h.saturating_sub(sy0)).min(buf_h.saturating_sub(dy0));

    if actual_w == 0 || actual_h == 0 { return; }

    let row_bytes = actual_w as usize * 4;

    // Copy to temp buffer first to handle overlapping regions
    let mut tmp = vec![0u8; row_bytes * actual_h as usize];
    for row in 0..actual_h {
        let src_off = ((sy0 + row) as usize) * (stride as usize) + (sx0 as usize) * 4;
        let tmp_off = (row as usize) * row_bytes;
        if src_off + row_bytes <= buffer.len() {
            tmp[tmp_off..tmp_off + row_bytes].copy_from_slice(&buffer[src_off..src_off + row_bytes]);
        }
    }

    // Copy from temp to destination
    for row in 0..actual_h {
        let dst_off = ((dy0 + row) as usize) * (stride as usize) + (dx0 as usize) * 4;
        let tmp_off = (row as usize) * row_bytes;
        if dst_off + row_bytes <= buffer.len() {
            buffer[dst_off..dst_off + row_bytes].copy_from_slice(&tmp[tmp_off..tmp_off + row_bytes]);
        }
    }
}

/// Set a single pixel in BGRA buffer (bounds-checked).
#[inline]
fn set_pixel(buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32, px: i32, py: i32, pixel: &[u8; 4]) {
    if px >= 0 && (px as u32) < buf_w && py >= 0 && (py as u32) < buf_h {
        let offset = (py as usize) * (stride as usize) + (px as usize) * 4;
        if offset + 4 <= buffer.len() {
            buffer[offset..offset + 4].copy_from_slice(pixel);
        }
    }
}

/// Check if angle (in 1/64 degree) is within arc span.
/// X11 angles: angle1 is start, angle2 is extent (can be negative).
/// 0 = 3 o'clock, counter-clockwise positive.
fn angle_in_arc(angle_deg64: f64, start: i16, extent: i16) -> bool {
    if extent == 0 { return false; }
    // Full circle
    if extent >= 360 * 64 || extent <= -360 * 64 { return true; }

    let start_f = start as f64;
    let extent_f = extent as f64;

    // Normalize angle to [0, 360*64)
    let normalize = |a: f64| -> f64 {
        let mut v = a % (360.0 * 64.0);
        if v < 0.0 { v += 360.0 * 64.0; }
        v
    };

    let a = normalize(angle_deg64);

    if extent_f > 0.0 {
        // Counter-clockwise: from start to start + extent
        let s = normalize(start_f);
        let e = normalize(start_f + extent_f);
        if s <= e {
            a >= s && a <= e
        } else {
            a >= s || a <= e
        }
    } else {
        // Clockwise: from start to start + extent (extent is negative)
        let s = normalize(start_f);
        let e = normalize(start_f + extent_f);
        if e <= s {
            a >= e && a <= s
        } else {
            a >= e || a <= s
        }
    }
}

/// Fill an arc (ellipse sector or full ellipse).
fn fill_arc(
    buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
    ax: i16, ay: i16, aw: u16, ah: u16,
    angle1: i16, angle2: i16, color: u32,
) {
    let pixel: [u8; 4] = [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ];

    let cx = ax as f64 + aw as f64 / 2.0;
    let cy = ay as f64 + ah as f64 / 2.0;
    let rx = aw as f64 / 2.0;
    let ry = ah as f64 / 2.0;
    if rx < 0.5 || ry < 0.5 { return; }

    let full_circle = angle2 >= 360 * 64 || angle2 <= -360 * 64;

    // Scan bounding box
    let y0 = (ay as i32).max(0);
    let y1 = ((ay as i32 + ah as i32).min(buf_h as i32)).max(0);
    let x0 = (ax as i32).max(0);
    let x1 = ((ax as i32 + aw as i32).min(buf_w as i32)).max(0);

    for py in y0..y1 {
        let dy = py as f64 - cy;
        // Solve ellipse: (dx/rx)^2 + (dy/ry)^2 <= 1
        let ry_term = (dy / ry) * (dy / ry);
        if ry_term > 1.0 { continue; }
        let dx_max = rx * (1.0 - ry_term).sqrt();
        let left = ((cx - dx_max).ceil() as i32).max(x0);
        let right = ((cx + dx_max).floor() as i32 + 1).min(x1);

        for px in left..right {
            if full_circle {
                set_pixel(buffer, buf_w, buf_h, stride, px, py, &pixel);
            } else {
                let dx = px as f64 - cx;
                let dy2 = py as f64 - cy;
                // atan2 with X11 convention: 0 at 3 o'clock, CCW positive
                let angle = (-dy2).atan2(dx); // negate Y because screen Y is flipped
                let angle_deg64 = angle.to_degrees() * 64.0;
                if angle_in_arc(angle_deg64, angle1, angle2) {
                    set_pixel(buffer, buf_w, buf_h, stride, px, py, &pixel);
                }
            }
        }
    }
}

/// Draw an arc outline (ellipse arc).
fn draw_arc(
    buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
    ax: i16, ay: i16, aw: u16, ah: u16,
    angle1: i16, angle2: i16, color: u32,
) {
    let pixel: [u8; 4] = [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ];

    let cx = ax as f64 + aw as f64 / 2.0;
    let cy = ay as f64 + ah as f64 / 2.0;
    let rx = aw as f64 / 2.0;
    let ry = ah as f64 / 2.0;
    if rx < 0.5 || ry < 0.5 { return; }

    // Number of steps proportional to circumference
    let circumference = std::f64::consts::PI * (3.0 * (rx + ry) - ((3.0 * rx + ry) * (rx + 3.0 * ry)).sqrt());
    let steps = (circumference * 2.0).max(360.0) as usize;

    let start_rad = (angle1 as f64 / 64.0).to_radians();
    let extent_rad = (angle2 as f64 / 64.0).to_radians();

    for i in 0..=steps {
        let t = start_rad + extent_rad * (i as f64 / steps as f64);
        let px = (cx + rx * t.cos()) as i32;
        // X11: positive angle is CCW, but screen Y goes down, so negate sin
        let py = (cy - ry * t.sin()) as i32;
        set_pixel(buffer, buf_w, buf_h, stride, px, py, &pixel);
    }
}

/// Fill a polygon using scanline algorithm.
fn fill_polygon(
    buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
    points: &[(i16, i16)], color: u32,
) {
    if points.len() < 3 { return; }

    let pixel: [u8; 4] = [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ];

    // Find bounding box
    let min_y = points.iter().map(|p| p.1).min().unwrap().max(0) as i32;
    let max_y = points.iter().map(|p| p.1).max().unwrap().min(buf_h as i16 - 1) as i32;

    let n = points.len();

    // Scanline fill
    for y in min_y..=max_y {
        // Find intersections with polygon edges
        let mut intersections: Vec<i32> = Vec::with_capacity(16);
        let scan_y = y as f64 + 0.5; // sample at pixel center

        for i in 0..n {
            let j = (i + 1) % n;
            let (y0, y1) = (points[i].1 as f64, points[j].1 as f64);
            let (x0, x1) = (points[i].0 as f64, points[j].0 as f64);

            // Check if this edge crosses the scanline
            if (y0 <= scan_y && y1 > scan_y) || (y1 <= scan_y && y0 > scan_y) {
                let t = (scan_y - y0) / (y1 - y0);
                let x = x0 + t * (x1 - x0);
                intersections.push(x as i32);
            }
        }

        intersections.sort_unstable();

        // Fill between pairs of intersections
        let mut i = 0;
        while i + 1 < intersections.len() {
            let x_start = intersections[i].max(0) as u32;
            let x_end = (intersections[i + 1] as u32).min(buf_w);
            for px in x_start..x_end {
                let offset = (y as usize) * (stride as usize) + (px as usize) * 4;
                if offset + 4 <= buffer.len() {
                    buffer[offset..offset + 4].copy_from_slice(&pixel);
                }
            }
            i += 2;
        }
    }
}

// --- Bitmap font text rendering ---

/// Draw text using a built-in 6x13 bitmap font.
/// x, y: X11 coordinates where y is the text baseline.
fn draw_text_bitmap(
    buffer: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    stride: u32,
    x: i16,
    y: i16,       // X11 baseline Y
    text: &[u8],
    color: u32,
    bg_color: Option<u32>,
) {
    const GLYPH_W: u32 = 6;
    const GLYPH_H: u32 = 13;
    const ASCENT: u32 = 10;

    let pixel: [u8; 4] = [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ];

    let top_y = y as i32 - ASCENT as i32;

    for (i, &ch) in text.iter().enumerate() {
        let cx = x as i32 + (i as i32 * GLYPH_W as i32);

        // Fill background per character cell if ImageText
        if let Some(bg) = bg_color {
            let fill = RenderCommand::FillRectangle {
                x: cx as i16, y: top_y as i16,
                width: GLYPH_W as u16, height: GLYPH_H as u16,
                color: bg,
            };
            render_to_buffer(buffer, buf_w, buf_h, stride, &fill);
        }

        // Get glyph bitmap
        let glyph = get_glyph_bitmap(ch);
        for row in 0..GLYPH_H {
            let bits = glyph[row as usize];
            if bits == 0 { continue; }
            let py = top_y + row as i32;
            if py < 0 || py >= buf_h as i32 { continue; }
            for col in 0..GLYPH_W {
                if bits & (0x80 >> col) != 0 {
                    let px = cx + col as i32;
                    if px >= 0 && (px as u32) < buf_w {
                        let off = (py as usize) * (stride as usize) + (px as usize) * 4;
                        if off + 4 <= buffer.len() {
                            buffer[off..off + 4].copy_from_slice(&pixel);
                        }
                    }
                }
            }
        }
    }
}

/// 6x13 bitmap font data. Each glyph is 13 bytes (rows), 6 MSB bits per row.
fn get_glyph_bitmap(ch: u8) -> [u8; 13] {
    match ch {
        // Space
        b' ' => [0x00; 13],
        b'!' => [0x00,0x00,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x20,0x00,0x00,0x00],
        b'"' => [0x00,0x00,0x50,0x50,0x50,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        b'#' => [0x00,0x00,0x50,0x50,0xF8,0x50,0xF8,0x50,0x50,0x00,0x00,0x00,0x00],
        b'$' => [0x00,0x20,0x70,0xA8,0xA0,0x70,0x28,0xA8,0x70,0x20,0x00,0x00,0x00],
        b'%' => [0x00,0x00,0x48,0xA8,0x50,0x20,0x50,0xA8,0x90,0x00,0x00,0x00,0x00],
        b'&' => [0x00,0x00,0x40,0xA0,0xA0,0x40,0xA8,0x90,0x68,0x00,0x00,0x00,0x00],
        b'\'' => [0x00,0x00,0x20,0x20,0x20,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        b'(' => [0x00,0x00,0x10,0x20,0x40,0x40,0x40,0x20,0x10,0x00,0x00,0x00,0x00],
        b')' => [0x00,0x00,0x40,0x20,0x10,0x10,0x10,0x20,0x40,0x00,0x00,0x00,0x00],
        b'*' => [0x00,0x00,0x00,0x20,0xA8,0x70,0xA8,0x20,0x00,0x00,0x00,0x00,0x00],
        b'+' => [0x00,0x00,0x00,0x20,0x20,0xF8,0x20,0x20,0x00,0x00,0x00,0x00,0x00],
        b',' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x20,0x20,0x40,0x00,0x00],
        b'-' => [0x00,0x00,0x00,0x00,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        b'.' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x20,0x00,0x00,0x00,0x00],
        b'/' => [0x00,0x00,0x08,0x08,0x10,0x20,0x40,0x80,0x80,0x00,0x00,0x00,0x00],
        b'0' => [0x00,0x00,0x70,0x88,0x98,0xA8,0xC8,0x88,0x70,0x00,0x00,0x00,0x00],
        b'1' => [0x00,0x00,0x20,0x60,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,0x00],
        b'2' => [0x00,0x00,0x70,0x88,0x08,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,0x00],
        b'3' => [0x00,0x00,0x70,0x88,0x08,0x30,0x08,0x88,0x70,0x00,0x00,0x00,0x00],
        b'4' => [0x00,0x00,0x10,0x30,0x50,0x90,0xF8,0x10,0x10,0x00,0x00,0x00,0x00],
        b'5' => [0x00,0x00,0xF8,0x80,0xF0,0x08,0x08,0x88,0x70,0x00,0x00,0x00,0x00],
        b'6' => [0x00,0x00,0x30,0x40,0x80,0xF0,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'7' => [0x00,0x00,0xF8,0x08,0x10,0x20,0x40,0x40,0x40,0x00,0x00,0x00,0x00],
        b'8' => [0x00,0x00,0x70,0x88,0x88,0x70,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'9' => [0x00,0x00,0x70,0x88,0x88,0x78,0x08,0x10,0x60,0x00,0x00,0x00,0x00],
        b':' => [0x00,0x00,0x00,0x00,0x20,0x00,0x00,0x20,0x00,0x00,0x00,0x00,0x00],
        b';' => [0x00,0x00,0x00,0x00,0x20,0x00,0x00,0x20,0x20,0x40,0x00,0x00,0x00],
        b'<' => [0x00,0x00,0x08,0x10,0x20,0x40,0x20,0x10,0x08,0x00,0x00,0x00,0x00],
        b'=' => [0x00,0x00,0x00,0x00,0xF8,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,0x00],
        b'>' => [0x00,0x00,0x80,0x40,0x20,0x10,0x20,0x40,0x80,0x00,0x00,0x00,0x00],
        b'?' => [0x00,0x00,0x70,0x88,0x08,0x10,0x20,0x00,0x20,0x00,0x00,0x00,0x00],
        b'@' => [0x00,0x00,0x70,0x88,0xB8,0xA8,0xB8,0x80,0x70,0x00,0x00,0x00,0x00],
        b'A' => [0x00,0x00,0x20,0x50,0x88,0x88,0xF8,0x88,0x88,0x00,0x00,0x00,0x00],
        b'B' => [0x00,0x00,0xF0,0x88,0x88,0xF0,0x88,0x88,0xF0,0x00,0x00,0x00,0x00],
        b'C' => [0x00,0x00,0x70,0x88,0x80,0x80,0x80,0x88,0x70,0x00,0x00,0x00,0x00],
        b'D' => [0x00,0x00,0xE0,0x90,0x88,0x88,0x88,0x90,0xE0,0x00,0x00,0x00,0x00],
        b'E' => [0x00,0x00,0xF8,0x80,0x80,0xF0,0x80,0x80,0xF8,0x00,0x00,0x00,0x00],
        b'F' => [0x00,0x00,0xF8,0x80,0x80,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,0x00],
        b'G' => [0x00,0x00,0x70,0x88,0x80,0xB8,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'H' => [0x00,0x00,0x88,0x88,0x88,0xF8,0x88,0x88,0x88,0x00,0x00,0x00,0x00],
        b'I' => [0x00,0x00,0x70,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,0x00],
        b'J' => [0x00,0x00,0x38,0x10,0x10,0x10,0x10,0x90,0x60,0x00,0x00,0x00,0x00],
        b'K' => [0x00,0x00,0x88,0x90,0xA0,0xC0,0xA0,0x90,0x88,0x00,0x00,0x00,0x00],
        b'L' => [0x00,0x00,0x80,0x80,0x80,0x80,0x80,0x80,0xF8,0x00,0x00,0x00,0x00],
        b'M' => [0x00,0x00,0x88,0xD8,0xA8,0x88,0x88,0x88,0x88,0x00,0x00,0x00,0x00],
        b'N' => [0x00,0x00,0x88,0xC8,0xA8,0x98,0x88,0x88,0x88,0x00,0x00,0x00,0x00],
        b'O' => [0x00,0x00,0x70,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'P' => [0x00,0x00,0xF0,0x88,0x88,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,0x00],
        b'Q' => [0x00,0x00,0x70,0x88,0x88,0x88,0xA8,0x90,0x68,0x00,0x00,0x00,0x00],
        b'R' => [0x00,0x00,0xF0,0x88,0x88,0xF0,0xA0,0x90,0x88,0x00,0x00,0x00,0x00],
        b'S' => [0x00,0x00,0x70,0x88,0x80,0x70,0x08,0x88,0x70,0x00,0x00,0x00,0x00],
        b'T' => [0x00,0x00,0xF8,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,0x00],
        b'U' => [0x00,0x00,0x88,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'V' => [0x00,0x00,0x88,0x88,0x88,0x50,0x50,0x20,0x20,0x00,0x00,0x00,0x00],
        b'W' => [0x00,0x00,0x88,0x88,0x88,0x88,0xA8,0xD8,0x88,0x00,0x00,0x00,0x00],
        b'X' => [0x00,0x00,0x88,0x88,0x50,0x20,0x50,0x88,0x88,0x00,0x00,0x00,0x00],
        b'Y' => [0x00,0x00,0x88,0x88,0x50,0x20,0x20,0x20,0x20,0x00,0x00,0x00,0x00],
        b'Z' => [0x00,0x00,0xF8,0x08,0x10,0x20,0x40,0x80,0xF8,0x00,0x00,0x00,0x00],
        b'[' => [0x00,0x00,0x70,0x40,0x40,0x40,0x40,0x40,0x70,0x00,0x00,0x00,0x00],
        b'\\' => [0x00,0x00,0x80,0x80,0x40,0x20,0x10,0x08,0x08,0x00,0x00,0x00,0x00],
        b']' => [0x00,0x00,0x70,0x10,0x10,0x10,0x10,0x10,0x70,0x00,0x00,0x00,0x00],
        b'^' => [0x00,0x00,0x20,0x50,0x88,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        b'_' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xF8,0x00,0x00,0x00,0x00],
        b'`' => [0x00,0x00,0x40,0x20,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        b'a' => [0x00,0x00,0x00,0x00,0x70,0x08,0x78,0x88,0x78,0x00,0x00,0x00,0x00],
        b'b' => [0x00,0x00,0x80,0x80,0xF0,0x88,0x88,0x88,0xF0,0x00,0x00,0x00,0x00],
        b'c' => [0x00,0x00,0x00,0x00,0x70,0x88,0x80,0x88,0x70,0x00,0x00,0x00,0x00],
        b'd' => [0x00,0x00,0x08,0x08,0x78,0x88,0x88,0x88,0x78,0x00,0x00,0x00,0x00],
        b'e' => [0x00,0x00,0x00,0x00,0x70,0x88,0xF8,0x80,0x70,0x00,0x00,0x00,0x00],
        b'f' => [0x00,0x00,0x30,0x48,0x40,0xF0,0x40,0x40,0x40,0x00,0x00,0x00,0x00],
        b'g' => [0x00,0x00,0x00,0x00,0x78,0x88,0x88,0x78,0x08,0x88,0x70,0x00,0x00],
        b'h' => [0x00,0x00,0x80,0x80,0xF0,0x88,0x88,0x88,0x88,0x00,0x00,0x00,0x00],
        b'i' => [0x00,0x00,0x20,0x00,0x60,0x20,0x20,0x20,0x70,0x00,0x00,0x00,0x00],
        b'j' => [0x00,0x00,0x10,0x00,0x30,0x10,0x10,0x10,0x90,0x60,0x00,0x00,0x00],
        b'k' => [0x00,0x00,0x80,0x80,0x90,0xA0,0xC0,0xA0,0x90,0x00,0x00,0x00,0x00],
        b'l' => [0x00,0x00,0x60,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,0x00],
        b'm' => [0x00,0x00,0x00,0x00,0xD0,0xA8,0xA8,0xA8,0x88,0x00,0x00,0x00,0x00],
        b'n' => [0x00,0x00,0x00,0x00,0xF0,0x88,0x88,0x88,0x88,0x00,0x00,0x00,0x00],
        b'o' => [0x00,0x00,0x00,0x00,0x70,0x88,0x88,0x88,0x70,0x00,0x00,0x00,0x00],
        b'p' => [0x00,0x00,0x00,0x00,0xF0,0x88,0x88,0xF0,0x80,0x80,0x00,0x00,0x00],
        b'q' => [0x00,0x00,0x00,0x00,0x78,0x88,0x88,0x78,0x08,0x08,0x00,0x00,0x00],
        b'r' => [0x00,0x00,0x00,0x00,0xB0,0xC8,0x80,0x80,0x80,0x00,0x00,0x00,0x00],
        b's' => [0x00,0x00,0x00,0x00,0x78,0x80,0x70,0x08,0xF0,0x00,0x00,0x00,0x00],
        b't' => [0x00,0x00,0x40,0x40,0xF0,0x40,0x40,0x48,0x30,0x00,0x00,0x00,0x00],
        b'u' => [0x00,0x00,0x00,0x00,0x88,0x88,0x88,0x88,0x78,0x00,0x00,0x00,0x00],
        b'v' => [0x00,0x00,0x00,0x00,0x88,0x88,0x50,0x50,0x20,0x00,0x00,0x00,0x00],
        b'w' => [0x00,0x00,0x00,0x00,0x88,0x88,0xA8,0xA8,0x50,0x00,0x00,0x00,0x00],
        b'x' => [0x00,0x00,0x00,0x00,0x88,0x50,0x20,0x50,0x88,0x00,0x00,0x00,0x00],
        b'y' => [0x00,0x00,0x00,0x00,0x88,0x88,0x88,0x78,0x08,0x88,0x70,0x00,0x00],
        b'z' => [0x00,0x00,0x00,0x00,0xF8,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,0x00],
        b'{' => [0x00,0x00,0x18,0x20,0x20,0xC0,0x20,0x20,0x18,0x00,0x00,0x00,0x00],
        b'|' => [0x00,0x00,0x20,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,0x00],
        b'}' => [0x00,0x00,0xC0,0x20,0x20,0x18,0x20,0x20,0xC0,0x00,0x00,0x00,0x00],
        b'~' => [0x00,0x00,0x48,0xB0,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        // Default: filled rectangle for unprintable chars
        _ => [0x00,0x00,0xF8,0xF8,0xF8,0xF8,0xF8,0xF8,0xF8,0x00,0x00,0x00,0x00],
    }
}
