// Core Graphics renderer for X11 drawing commands
// Maps RenderCommand variants to CGContext operations

use crate::display::RenderCommand;

/// X11 GC raster operation: combine src and dst pixels according to gc_function,
/// then apply plane_mask. Alpha channel is always preserved at 0xFF (XQuartz approach).
/// gc_function values 0-15 map to the standard X11 GC functions (GXclear..GXset).
#[inline]
fn apply_rop(gc_function: u8, src: u32, dst: u32, plane_mask: u32) -> u32 {
    let result = match gc_function {
        0  => 0,                    // GXclear
        1  => src & dst,            // GXand
        2  => src & !dst,           // GXandReverse
        3  => src,                  // GXcopy (default)
        4  => !src & dst,           // GXandInverted
        5  => dst,                  // GXnoop
        6  => src ^ dst,            // GXxor
        7  => src | dst,            // GXor
        8  => !(src | dst),         // GXnor
        9  => !(src ^ dst),         // GXequiv
        10 => !dst,                 // GXinvert
        11 => src | !dst,           // GXorReverse
        12 => !src,                 // GXcopyInverted
        13 => !src | dst,           // GXorInverted
        14 => !(src & dst),         // GXnand
        15 => 0xFFFFFFFF,           // GXset
        _  => src,
    };
    // Apply plane_mask: only modify bits selected by plane_mask
    // Alpha always stays 0xFF
    ((dst & !plane_mask) | (result & plane_mask)) | 0xFF000000
}

/// Apply ROP to a single pixel in BGRA buffer (bounds-checked).
/// Fast path: gc_function==3 && plane_mask==0xFFFFFFFF → direct copy (no ROP overhead).
#[inline]
fn rop_pixel(buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
             px: i32, py: i32, src_color: u32, gc_function: u8, plane_mask: u32) {
    if px >= 0 && (px as u32) < buf_w && py >= 0 && (py as u32) < buf_h {
        let offset = (py as usize) * (stride as usize) + (px as usize) * 4;
        if offset + 4 <= buffer.len() {
            if gc_function == 3 && plane_mask == 0xFFFFFFFF {
                // Fast path: GXcopy with full plane_mask — just write the pixel
                let pixel_u32 = (src_color & 0x00FFFFFF) | 0xFF000000;
                buffer[offset..offset + 4].copy_from_slice(&pixel_u32.to_ne_bytes());
            } else if gc_function == 5 {
                // GXnoop — do nothing
                return;
            } else {
                let dst = u32::from_ne_bytes([buffer[offset], buffer[offset+1], buffer[offset+2], buffer[offset+3]]);
                let result = apply_rop(gc_function, src_color | 0xFF000000, dst, plane_mask & 0x00FFFFFF);
                buffer[offset..offset + 4].copy_from_slice(&result.to_ne_bytes());
            }
        }
    }
}

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
        RenderCommand::FillRectangle { x, y, width: w, height: h, color, gc_function } => {
            let x0 = (*x as i32).max(0) as u32;
            let y0 = (*y as i32).max(0) as u32;
            let x1 = ((*x as i32 + *w as i32).max(0) as u32).min(width);
            let y1 = ((*y as i32 + *h as i32).max(0) as u32).min(height);

            if x0 >= x1 || y0 >= y1 { return; }

            if *gc_function == 5 { return; } // GXnoop

            if *gc_function == 3 {
                // Fast path: GXcopy — direct fill, no ROP overhead (preserves IME perf)
                let pixel_u32: u32 = (*color & 0x00FFFFFF) | 0xFF000000;
                let row_bytes = (x1 - x0) as usize * 4;

                let first_start = (y0 * stride + x0 * 4) as usize;
                let first_end = first_start + row_bytes;
                if first_end > buffer.len() { return; }
                {
                    let row = &mut buffer[first_start..first_end];
                    for chunk in row.chunks_exact_mut(4) {
                        chunk.copy_from_slice(&pixel_u32.to_ne_bytes());
                    }
                }
                for py in (y0 + 1)..y1 {
                    let dst_start = (py * stride + x0 * 4) as usize;
                    let dst_end = dst_start + row_bytes;
                    if dst_end > buffer.len() { break; }
                    buffer.copy_within(first_start..first_end, dst_start);
                }
            } else if *gc_function == 6 {
                // GXxor on FillRectangle: light blue translucent highlight (selection)
                let hr: u32 = 60;  let hg: u32 = 140; let hb: u32 = 255;
                let alpha: u32 = 77; // ~30%
                let inv_alpha: u32 = 255 - alpha;
                for py in y0..y1 {
                    let row_off = (py * stride) as usize;
                    let off_start = row_off + (x0 as usize) * 4;
                    let off_end = row_off + (x1 as usize) * 4;
                    if off_end > buffer.len() { break; }
                    for chunk in buffer[off_start..off_end].chunks_exact_mut(4) {
                        let db = chunk[0] as u32;
                        let dg = chunk[1] as u32;
                        let dr = chunk[2] as u32;
                        chunk[0] = ((hb * alpha + db * inv_alpha) / 255) as u8;
                        chunk[1] = ((hg * alpha + dg * inv_alpha) / 255) as u8;
                        chunk[2] = ((hr * alpha + dr * inv_alpha) / 255) as u8;
                        chunk[3] = 255;
                    }
                }
            } else {
                // All other GC functions: per-pixel ROP
                let src = (*color & 0x00FFFFFF) | 0xFF000000;
                for py in y0..y1 {
                    let row_off = (py * stride) as usize;
                    let off_start = row_off + (x0 as usize) * 4;
                    let off_end = row_off + (x1 as usize) * 4;
                    if off_end > buffer.len() { break; }
                    for chunk in buffer[off_start..off_end].chunks_exact_mut(4) {
                        let dst = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        let result = apply_rop(*gc_function, src, dst, 0x00FFFFFF);
                        chunk.copy_from_slice(&result.to_ne_bytes());
                    }
                }
            }
        }
        RenderCommand::ClearArea { x, y, width: w, height: h, bg_color } => {
            // Same as FillRectangle with background color
            let fill = RenderCommand::FillRectangle {
                x: *x, y: *y, width: *w, height: *h, color: *bg_color, gc_function: 3,
            };
            render_to_buffer(buffer, width, height, stride, &fill);
        }
        RenderCommand::DrawLine { x1, y1, x2, y2, color, gc_function, .. } => {
            if *gc_function == 5 { return; } // GXnoop
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

            loop {
                rop_pixel(buffer, width, height, stride, x, y, *color, *gc_function, 0xFFFFFFFF);

                if x == x2 && y == y2 { break; }
                let e2 = 2 * err;
                if e2 >= dy { err += dy; x += sx; }
                if e2 <= dx { err += dx; y += sy; }
            }
        }
        RenderCommand::PutImage { x, y, width: w, height: h, depth, format, data, gc_function } => {
            if *gc_function == 5 { return; } // GXnoop
            let dst_x = (*x as i32).max(0) as u32;
            let dst_y = (*y as i32).max(0) as u32;
            let src_w = *w as u32;
            let src_h = *h as u32;

            if *format == 2 && (*depth == 24 || *depth == 32) {
                let src_stride = src_w * 4;
                if *gc_function == 3 {
                    // Fast path: GXcopy — bulk memcpy per row then fix alpha.
                    // memcpy is SIMD-optimized by the OS; touching pixels individually is 5-10x slower.
                    let copy_w = src_w.min(width.saturating_sub(dst_x)) as usize;
                    let row_bytes = copy_w * 4;
                    for row in 0..src_h {
                        let dy = dst_y + row;
                        if dy >= height { break; }
                        let src_off = (row * src_stride) as usize;
                        let dst_off = (dy * stride + dst_x * 4) as usize;
                        if src_off + row_bytes <= data.len() && dst_off + row_bytes <= buffer.len() {
                            buffer[dst_off..dst_off + row_bytes]
                                .copy_from_slice(&data[src_off..src_off + row_bytes]);
                            // Set alpha to 0xFF for each pixel (IOSurface requires opaque alpha).
                            // Process as u32 slices for auto-vectorization.
                            let dst_pixels = unsafe {
                                std::slice::from_raw_parts_mut(
                                    buffer[dst_off..].as_mut_ptr() as *mut u32,
                                    copy_w,
                                )
                            };
                            for p in dst_pixels.iter_mut() {
                                *p |= 0xFF000000;
                            }
                        }
                    }
                } else {
                    // Per-pixel ROP for non-copy functions
                    for row in 0..src_h {
                        let dy = dst_y + row;
                        if dy >= height { break; }
                        let copy_w = src_w.min(width.saturating_sub(dst_x));
                        for col in 0..copy_w {
                            let src_off = (row * src_stride + col * 4) as usize;
                            let dst_off = (dy * stride + (dst_x + col) * 4) as usize;
                            if src_off + 4 <= data.len() && dst_off + 4 <= buffer.len() {
                                let src_pixel = u32::from_ne_bytes([data[src_off], data[src_off+1], data[src_off+2], 0xFF]);
                                let dst_pixel = u32::from_ne_bytes([buffer[dst_off], buffer[dst_off+1], buffer[dst_off+2], buffer[dst_off+3]]);
                                let result = apply_rop(*gc_function, src_pixel, dst_pixel, 0x00FFFFFF);
                                buffer[dst_off..dst_off + 4].copy_from_slice(&result.to_ne_bytes());
                            }
                        }
                    }
                }
            } else {
                log::debug!("PutImage: unsupported format={} depth={}", format, depth);
            }
        }
        RenderCommand::DrawRectangle { x, y, width: w, height: h, color, line_width: _, gc_function } => {
            if *gc_function == 5 { return; } // GXnoop
            let x0 = *x as i32;
            let y0 = *y as i32;
            let x1 = x0 + *w as i32;
            let y1 = y0 + *h as i32;
            // Top and bottom edges
            for px in x0..=x1 {
                rop_pixel(buffer, width, height, stride, px, y0, *color, *gc_function, 0xFFFFFFFF);
                rop_pixel(buffer, width, height, stride, px, y1, *color, *gc_function, 0xFFFFFFFF);
            }
            // Left and right edges
            for py in y0..=y1 {
                rop_pixel(buffer, width, height, stride, x0, py, *color, *gc_function, 0xFFFFFFFF);
                rop_pixel(buffer, width, height, stride, x1, py, *color, *gc_function, 0xFFFFFFFF);
            }
        }
        RenderCommand::FillArc { x, y, width: w, height: h, angle1, angle2, color, gc_function } => {
            fill_arc(buffer, width, height, stride,
                     *x, *y, *w, *h, *angle1, *angle2, *color, *gc_function);
        }
        RenderCommand::DrawArc { x, y, width: w, height: h, angle1, angle2, color, gc_function, .. } => {
            draw_arc(buffer, width, height, stride,
                     *x, *y, *w, *h, *angle1, *angle2, *color, *gc_function);
        }
        RenderCommand::DrawText { x, y, text, font_id: _, color, bg_color, gc_function } => {
            if *gc_function == 5 { return; } // GXnoop
            // log::debug!("DrawText at ({},{}) len={} fg=0x{:06X} bg={:?}", x, y, text.len(), color, bg_color);
            draw_text_bitmap(buffer, width, height, stride,
                              *x, *y, text, *color, *bg_color);
        }
        RenderCommand::CopyArea { src_x, src_y, dst_x, dst_y, width: w, height: h, gc_function } => {
            copy_area(buffer, width, height, stride,
                      *src_x, *src_y, *dst_x, *dst_y, *w, *h, *gc_function);
        }
        RenderCommand::FillPolygon { points, color, gc_function } => {
            fill_polygon(buffer, width, height, stride, points, *color, *gc_function);
        }
        _ => {
            log::trace!("Unimplemented render command: {:?}", command);
        }
    }
}

/// Copy a rectangular region within the same buffer (for scrolling).
/// Uses copy_within for non-overlapping GXcopy (zero allocation).
/// Falls back to a thread-local temp buffer for overlapping regions.
fn copy_area(
    buffer: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    stride: u32,
    src_x: i16, src_y: i16,
    dst_x: i16, dst_y: i16,
    width: u16, height: u16,
    gc_function: u8,
) {
    if gc_function == 5 { return; } // GXnoop

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
    let stride_usize = stride as usize;

    if gc_function == 3 {
        // Fast path: GXcopy — use copy_within (zero allocation)
        // For same-column copies (common scroll), rows don't overlap across stride boundaries.
        // Choose forward or reverse row order to handle vertical overlap.
        if sx0 == dx0 {
            // Same X: pure vertical scroll — copy_within per row, ordered to avoid overlap
            if dy0 <= sy0 {
                // Scrolling up (or same): copy top-to-bottom
                for row in 0..actual_h {
                    let src_off = ((sy0 + row) as usize) * stride_usize + (sx0 as usize) * 4;
                    let dst_off = ((dy0 + row) as usize) * stride_usize + (dx0 as usize) * 4;
                    if src_off + row_bytes <= buffer.len() && dst_off + row_bytes <= buffer.len() {
                        buffer.copy_within(src_off..src_off + row_bytes, dst_off);
                    }
                }
            } else {
                // Scrolling down: copy bottom-to-top
                for row in (0..actual_h).rev() {
                    let src_off = ((sy0 + row) as usize) * stride_usize + (sx0 as usize) * 4;
                    let dst_off = ((dy0 + row) as usize) * stride_usize + (dx0 as usize) * 4;
                    if src_off + row_bytes <= buffer.len() && dst_off + row_bytes <= buffer.len() {
                        buffer.copy_within(src_off..src_off + row_bytes, dst_off);
                    }
                }
            }
        } else {
            // Different X positions: rows might overlap within a row.
            // Use thread-local temp buffer to avoid per-call allocation.
            thread_local! {
                static COPY_TMP: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
            }
            COPY_TMP.with(|tmp_cell| {
                let mut tmp = tmp_cell.borrow_mut();
                let needed = row_bytes * actual_h as usize;
                if tmp.len() < needed { tmp.resize(needed, 0); }
                for row in 0..actual_h {
                    let src_off = ((sy0 + row) as usize) * stride_usize + (sx0 as usize) * 4;
                    let tmp_off = (row as usize) * row_bytes;
                    if src_off + row_bytes <= buffer.len() {
                        tmp[tmp_off..tmp_off + row_bytes].copy_from_slice(&buffer[src_off..src_off + row_bytes]);
                    }
                }
                for row in 0..actual_h {
                    let dst_off = ((dy0 + row) as usize) * stride_usize + (dx0 as usize) * 4;
                    let tmp_off = (row as usize) * row_bytes;
                    if dst_off + row_bytes <= buffer.len() {
                        buffer[dst_off..dst_off + row_bytes].copy_from_slice(&tmp[tmp_off..tmp_off + row_bytes]);
                    }
                }
            });
        }
    } else {
        // Non-GXcopy: use thread-local temp buffer + per-pixel ROP
        thread_local! {
            static ROP_TMP: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        ROP_TMP.with(|tmp_cell| {
            let mut tmp = tmp_cell.borrow_mut();
            let needed = row_bytes * actual_h as usize;
            if tmp.len() < needed { tmp.resize(needed, 0); }
            for row in 0..actual_h {
                let src_off = ((sy0 + row) as usize) * stride_usize + (sx0 as usize) * 4;
                let tmp_off = (row as usize) * row_bytes;
                if src_off + row_bytes <= buffer.len() {
                    tmp[tmp_off..tmp_off + row_bytes].copy_from_slice(&buffer[src_off..src_off + row_bytes]);
                }
            }
            for row in 0..actual_h {
                for col in 0..actual_w {
                    let tmp_off = (row as usize) * row_bytes + (col as usize) * 4;
                    let dst_off = ((dy0 + row) as usize) * stride_usize + ((dx0 + col) as usize) * 4;
                    if tmp_off + 4 <= needed && dst_off + 4 <= buffer.len() {
                        let src_pixel = u32::from_ne_bytes([tmp[tmp_off], tmp[tmp_off+1], tmp[tmp_off+2], tmp[tmp_off+3]]);
                        let dst_pixel = u32::from_ne_bytes([buffer[dst_off], buffer[dst_off+1], buffer[dst_off+2], buffer[dst_off+3]]);
                        let result = apply_rop(gc_function, src_pixel, dst_pixel, 0x00FFFFFF);
                        buffer[dst_off..dst_off + 4].copy_from_slice(&result.to_ne_bytes());
                    }
                }
            }
        });
    }
}

/// Set a single pixel in BGRA buffer (bounds-checked). Used by text rendering (always GXcopy).
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
    angle1: i16, angle2: i16, color: u32, gc_function: u8,
) {
    if gc_function == 5 { return; } // GXnoop

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
        let ry_term = (dy / ry) * (dy / ry);
        if ry_term > 1.0 { continue; }
        let dx_max = rx * (1.0 - ry_term).sqrt();
        let left = ((cx - dx_max).ceil() as i32).max(x0);
        let right = ((cx + dx_max).floor() as i32 + 1).min(x1);

        for px in left..right {
            let in_arc = if full_circle {
                true
            } else {
                let dxf = px as f64 - cx;
                let dy2 = py as f64 - cy;
                let angle = (-dy2).atan2(dxf);
                let angle_deg64 = angle.to_degrees() * 64.0;
                angle_in_arc(angle_deg64, angle1, angle2)
            };
            if in_arc {
                rop_pixel(buffer, buf_w, buf_h, stride, px, py, color, gc_function, 0xFFFFFFFF);
            }
        }
    }
}

/// Draw an arc outline (ellipse arc).
fn draw_arc(
    buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
    ax: i16, ay: i16, aw: u16, ah: u16,
    angle1: i16, angle2: i16, color: u32, gc_function: u8,
) {
    if gc_function == 5 { return; } // GXnoop

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
        rop_pixel(buffer, buf_w, buf_h, stride, px, py, color, gc_function, 0xFFFFFFFF);
    }
}

/// Fill a polygon using scanline algorithm.
fn fill_polygon(
    buffer: &mut [u8], buf_w: u32, buf_h: u32, stride: u32,
    points: &[(i16, i16)], color: u32, gc_function: u8,
) {
    if points.len() < 3 { return; }
    if gc_function == 5 { return; } // GXnoop

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
                rop_pixel(buffer, buf_w, buf_h, stride, px as i32, y, color, gc_function, 0xFFFFFFFF);
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

    // Try to decode as UTF-8; if valid, render char-by-char with width awareness
    let text_str = std::str::from_utf8(text);
    let mut cursor_x = x as i32;

    if let Ok(s) = text_str {
        for ch in s.chars() {
            // Determine character width: CJK/fullwidth = 2 cells, ASCII = 1 cell
            let char_cells = if ch.is_ascii() { 1u32 } else { 2u32 };
            let cell_w = char_cells * GLYPH_W;

            // Fill background per character cell if ImageText
            if let Some(bg) = bg_color {
                let fill = RenderCommand::FillRectangle {
                    x: cursor_x as i16, y: top_y as i16,
                    width: cell_w as u16, height: GLYPH_H as u16,
                    color: bg, gc_function: 3,
                };
                render_to_buffer(buffer, buf_w, buf_h, stride, &fill);
            }

            if ch.is_ascii() {
                // ASCII: use bitmap font
                let glyph = get_glyph_bitmap(ch as u8);
                for row in 0..GLYPH_H {
                    let bits = glyph[row as usize];
                    if bits == 0 { continue; }
                    let py = top_y + row as i32;
                    if py < 0 || py >= buf_h as i32 { continue; }
                    for col in 0..GLYPH_W {
                        if bits & (0x80 >> col) != 0 {
                            let px = cursor_x + col as i32;
                            if px >= 0 && (px as u32) < buf_w {
                                let off = (py as usize) * (stride as usize) + (px as usize) * 4;
                                if off + 4 <= buffer.len() {
                                    buffer[off..off + 4].copy_from_slice(&pixel);
                                }
                            }
                        }
                    }
                }
            } else {
                // Non-ASCII (CJK etc.): render using CoreText
                render_coretext_char(buffer, buf_w, buf_h, stride, cursor_x, top_y, ch, &pixel, cell_w, GLYPH_H);
            }

            cursor_x += cell_w as i32;
        }
    } else {
        // Not valid UTF-8: fall back to byte-by-byte rendering
        for (i, &ch) in text.iter().enumerate() {
            let cx = x as i32 + (i as i32 * GLYPH_W as i32);

            if let Some(bg) = bg_color {
                let fill = RenderCommand::FillRectangle {
                    x: cx as i16, y: top_y as i16,
                    width: GLYPH_W as u16, height: GLYPH_H as u16,
                    color: bg, gc_function: 3,
                };
                render_to_buffer(buffer, buf_w, buf_h, stride, &fill);
            }

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

/// Thread-local glyph cache for CoreText-rendered characters.
/// Key: (char, cell_w, cell_h). Value: BGRA bitmap (white on transparent, alpha-masked).
/// At blit time, foreground color is applied per-pixel using cached alpha.
struct GlyphCache {
    /// Cached glyph bitmaps: key=(char, cell_w, cell_h), value=alpha-only bitmap
    entries: std::collections::HashMap<(char, u32, u32), Vec<u8>>,
    /// Cached CTFont pointer (retained). Invalidated on cell_h change.
    ct_font: *mut std::ffi::c_void,
    ct_font_cell_h: u32,
    /// Cached CGColorSpace (retained, never changes)
    colorspace: *mut std::ffi::c_void,
    /// Reusable temp bitmap buffer
    tmp_buf: Vec<u8>,
}

impl GlyphCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            ct_font: std::ptr::null_mut(),
            ct_font_cell_h: 0,
            colorspace: std::ptr::null_mut(),
            tmp_buf: Vec::new(),
        }
    }

    /// Get or create the CGColorSpace (lifetime = process)
    fn get_colorspace(&mut self) -> *mut std::ffi::c_void {
        if self.colorspace.is_null() {
            extern "C" { fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void; }
            self.colorspace = unsafe { CGColorSpaceCreateDeviceRGB() };
        }
        self.colorspace
    }

    /// Get or create CTFont for the given cell_h
    fn get_font(&mut self, cell_h: u32) -> *mut std::ffi::c_void {
        if self.ct_font.is_null() || self.ct_font_cell_h != cell_h {
            extern "C" {
                fn CTFontCreateWithName(name: *const std::ffi::c_void, size: f64, matrix: *const std::ffi::c_void) -> *mut std::ffi::c_void;
                fn CFRelease(cf: *mut std::ffi::c_void);
            }
            if !self.ct_font.is_null() {
                unsafe { CFRelease(self.ct_font); }
            }
            unsafe {
                use objc2::msg_send;
                use objc2::runtime::AnyObject;
                let font_name: *mut AnyObject = msg_send![
                    objc2::class!(NSString),
                    stringWithUTF8String: c"HiraginoSans-W3".as_ptr()
                ];
                self.ct_font = CTFontCreateWithName(font_name as *const _, cell_h as f64 - 2.0, std::ptr::null());
            }
            self.ct_font_cell_h = cell_h;
        }
        self.ct_font
    }

    /// Render a glyph and return its alpha bitmap (cell_w * cell_h bytes, one byte per pixel).
    fn get_alpha_bitmap(&mut self, ch: char, cell_w: u32, cell_h: u32) -> &[u8] {
        let key = (ch, cell_w, cell_h);
        if self.entries.contains_key(&key) {
            return &self.entries[&key];
        }

        let w = cell_w as usize;
        let h = cell_h as usize;
        let tmp_stride = w * 4;
        let tmp_size = tmp_stride * h;

        // Reuse tmp_buf — clear + resize is faster than resize + fill
        // because resize(n, 0) only zero-fills new elements, not existing ones
        if self.tmp_buf.len() == tmp_size {
            self.tmp_buf.fill(0);
        } else {
            self.tmp_buf.clear();
            self.tmp_buf.resize(tmp_size, 0);
        }

        extern "C" {
            fn CGBitmapContextCreate(
                data: *mut std::ffi::c_void, width: usize, height: usize,
                bits_per_component: usize, bytes_per_row: usize,
                colorspace: *mut std::ffi::c_void, bitmap_info: u32,
            ) -> *mut std::ffi::c_void;
            fn CGContextSetRGBFillColor(ctx: *mut std::ffi::c_void, r: f64, g: f64, b: f64, a: f64);
            fn CGContextRelease(ctx: *mut std::ffi::c_void);
            fn CTLineDraw(line: *const std::ffi::c_void, ctx: *mut std::ffi::c_void);
            fn CGContextSetTextPosition(ctx: *mut std::ffi::c_void, x: f64, y: f64);
            fn CTLineCreateWithAttributedString(attr_str: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CFRelease(cf: *mut std::ffi::c_void);
        }

        let cs = self.get_colorspace();
        let ct_font = self.get_font(cell_h);

        let alpha_bitmap = unsafe {
            let ctx = CGBitmapContextCreate(
                self.tmp_buf.as_mut_ptr() as *mut _, w, h, 8, tmp_stride, cs, 0x2002,
            );
            if ctx.is_null() || ct_font.is_null() {
                if !ctx.is_null() { CGContextRelease(ctx); }
                // Return empty alpha bitmap
                vec![0u8; w * h]
            } else {
                // Render in white so we can extract alpha
                CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);

                use objc2::msg_send;
                use objc2::runtime::AnyObject;

                let mut char_buf = [0u8; 4];
                let char_str = ch.encode_utf8(&mut char_buf);
                let ns_string: *mut AnyObject = msg_send![
                    objc2::class!(NSString),
                    stringWithUTF8String: char_str.as_ptr() as *const std::ffi::c_char
                ];

                if !ns_string.is_null() {
                    extern "C" {
                        static kCTFontAttributeName: *const std::ffi::c_void;
                        static kCTForegroundColorFromContextAttributeName: *const std::ffi::c_void;
                    }
                    let ct_font_key: *const AnyObject = kCTFontAttributeName as *const _;
                    let ct_fg_key: *const AnyObject = kCTForegroundColorFromContextAttributeName as *const _;
                    let yes: *mut AnyObject = msg_send![objc2::class!(NSNumber), numberWithBool: true];
                    let keys = [ct_font_key, ct_fg_key];
                    let vals = [ct_font as *const AnyObject, yes as *const AnyObject];
                    let attrs: *mut AnyObject = msg_send![
                        objc2::class!(NSDictionary),
                        dictionaryWithObjects: vals.as_ptr()
                        forKeys: keys.as_ptr()
                        count: 2usize
                    ];
                    let attr_str: *mut AnyObject = msg_send![
                        objc2::class!(NSAttributedString), alloc
                    ];
                    let attr_str: *mut AnyObject = msg_send![
                        attr_str, initWithString: ns_string attributes: attrs
                    ];
                    if !attr_str.is_null() {
                        let ct_line = CTLineCreateWithAttributedString(attr_str as *const _);
                        if !ct_line.is_null() {
                            CGContextSetTextPosition(ctx, 0.0, 2.0);
                            CTLineDraw(ct_line as *const _, ctx);
                            CFRelease(ct_line);
                        }
                        let _: () = msg_send![&*attr_str, release];
                    }
                }

                CGContextRelease(ctx);

                // Extract alpha channel into compact bitmap (1 byte per pixel)
                let mut alpha = vec![0u8; w * h];
                for row in 0..h {
                    for col in 0..w {
                        alpha[row * w + col] = self.tmp_buf[row * tmp_stride + col * 4 + 3];
                    }
                }
                alpha
            }
        };

        self.entries.insert(key, alpha_bitmap);
        &self.entries[&key]
    }
}

thread_local! {
    static GLYPH_CACHE: std::cell::RefCell<GlyphCache> = std::cell::RefCell::new(GlyphCache::new());
}

/// Render a single non-ASCII character using CoreText into the pixel buffer.
/// Uses glyph cache to avoid redundant CoreText rendering.
fn render_coretext_char(
    buffer: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    stride: u32,
    cx: i32,
    top_y: i32,
    ch: char,
    pixel: &[u8; 4],
    cell_w: u32,
    cell_h: u32,
) {
    let w = cell_w as usize;
    let h = cell_h as usize;

    GLYPH_CACHE.with(|cache_cell| {
        let mut cache = cache_cell.borrow_mut();
        let alpha_bitmap = cache.get_alpha_bitmap(ch, cell_w, cell_h);

        // Blit cached alpha bitmap with the requested foreground color
        for row in 0..h {
            let py = top_y + row as i32;
            if py < 0 || py >= buf_h as i32 { continue; }
            let dst_row_off = (py as usize) * (stride as usize);
            for col in 0..w {
                let a = alpha_bitmap[row * w + col];
                if a == 0 { continue; }
                let px = cx + col as i32;
                if px < 0 || px >= buf_w as i32 { continue; }
                let dst_off = dst_row_off + (px as usize) * 4;
                if dst_off + 4 > buffer.len() { continue; }
                if a >= 250 {
                    // Fully opaque — direct copy
                    buffer[dst_off..dst_off + 4].copy_from_slice(pixel);
                } else {
                    // Alpha blend with background
                    let inv_a = 255 - a as u32;
                    let a32 = a as u32;
                    buffer[dst_off]     = ((pixel[0] as u32 * a32 + buffer[dst_off] as u32 * inv_a) / 255) as u8;
                    buffer[dst_off + 1] = ((pixel[1] as u32 * a32 + buffer[dst_off + 1] as u32 * inv_a) / 255) as u8;
                    buffer[dst_off + 2] = ((pixel[2] as u32 * a32 + buffer[dst_off + 2] as u32 * inv_a) / 255) as u8;
                    buffer[dst_off + 3] = 0xFF;
                }
            }
        }
    });
}

/// Fallback box glyph for when CoreText rendering fails.
fn render_box_glyph(
    buffer: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    stride: u32,
    cx: i32,
    top_y: i32,
    pixel: &[u8; 4],
    cell_w: u32,
    cell_h: u32,
) {
    // Draw a box outline
    for row in 0..cell_h {
        let py = top_y + row as i32;
        if py < 0 || py >= buf_h as i32 { continue; }
        for col in 0..cell_w {
            let is_border = row == 0 || row == cell_h - 1 || col == 0 || col == cell_w - 1;
            if is_border {
                let px = cx + col as i32;
                if px >= 0 && (px as u32) < buf_w {
                    let off = (py as usize) * (stride as usize) + (px as usize) * 4;
                    if off + 4 <= buffer.len() {
                        buffer[off..off + 4].copy_from_slice(pixel);
                    }
                }
            }
        }
    }
}
