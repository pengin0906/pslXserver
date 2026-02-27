// Mouse input handling

/// Convert macOS mouse button number to X11 button number.
/// macOS: 0=left, 1=right, 2=middle, 3+=other
/// X11:   1=left, 2=middle, 3=right, 4=scrollUp, 5=scrollDown
pub fn macos_button_to_x11(macos_button: i32) -> u8 {
    match macos_button {
        0 => 1, // Left
        1 => 3, // Right (macOS swaps middle and right vs X11)
        2 => 2, // Middle
        n => (n + 1) as u8,
    }
}

/// Convert macOS scroll delta to X11 button presses.
/// X11 uses button 4 (scroll up) and button 5 (scroll down).
/// Returns (button, count) pairs.
pub fn scroll_delta_to_x11_buttons(delta_x: f64, delta_y: f64) -> Vec<(u8, u32)> {
    let mut buttons = Vec::new();

    if delta_y > 0.0 {
        // Scroll up
        buttons.push((4, delta_y.ceil() as u32));
    } else if delta_y < 0.0 {
        // Scroll down
        buttons.push((5, (-delta_y).ceil() as u32));
    }

    if delta_x > 0.0 {
        // Scroll right (button 7 by convention, or 6)
        buttons.push((6, delta_x.ceil() as u32));
    } else if delta_x < 0.0 {
        // Scroll left
        buttons.push((7, (-delta_x).ceil() as u32));
    }

    buttons
}
