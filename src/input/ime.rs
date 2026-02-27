// IME Bridge — connects macOS NSTextInputClient to XIM protocol
//
// This is the critical path for Japanese input support.
// The coordinate pipeline:
//   X11 client -> XIM SET_IC_VALUES(XNSpotLocation)
//   -> ImeBridge -> DisplayCommand::UpdateImeSpot
//   -> X11View ivars (ime_spot, ime_line_height)
//   -> macOS calls firstRectForCharacterRange
//   -> CoordContext.x11_caret_to_macos_screen_rect()
//   -> IME candidate window appears at correct position

use std::collections::HashMap;

use crate::display::Xid;
use crate::util::coord::X11Point;

/// XIM input style bits.
pub mod input_style {
    pub const PREEDIT_AREA: u32 = 0x0001;
    pub const PREEDIT_CALLBACKS: u32 = 0x0002;
    pub const PREEDIT_POSITION: u32 = 0x0004;
    pub const PREEDIT_NOTHING: u32 = 0x0008;
    pub const PREEDIT_NONE: u32 = 0x0010;
    pub const STATUS_AREA: u32 = 0x0100;
    pub const STATUS_CALLBACKS: u32 = 0x0200;
    pub const STATUS_NOTHING: u32 = 0x0400;
    pub const STATUS_NONE: u32 = 0x0800;
}

/// State for an XIM input context.
#[derive(Debug)]
pub struct ImeContext {
    pub ic_id: u16,
    pub im_id: u16,
    pub window: Xid,
    pub input_style: u32,
    /// Spot location in X11 window-local coordinates (physical pixels).
    pub spot_location: X11Point,
    /// Font for preedit rendering.
    pub preedit_font: Option<String>,
    /// Current preedit state.
    pub preedit_state: PreeditState,
}

#[derive(Debug)]
pub enum PreeditState {
    Inactive,
    Active {
        text: String,
        cursor_pos: usize,
    },
}

/// The IME bridge manages all XIM input contexts.
pub struct ImeBridge {
    /// Active input contexts: IC ID -> ImeContext.
    contexts: HashMap<u16, ImeContext>,
    /// Next IC ID to assign.
    next_ic_id: u16,
    /// IM ID (from XIM_OPEN).
    im_id: u16,
}

impl ImeBridge {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            next_ic_id: 1,
            im_id: 0,
        }
    }

    /// Create a new input context for a window.
    pub fn create_ic(
        &mut self,
        window: Xid,
        input_style: u32,
    ) -> u16 {
        let ic_id = self.next_ic_id;
        self.next_ic_id += 1;

        let ctx = ImeContext {
            ic_id,
            im_id: self.im_id,
            window,
            input_style,
            spot_location: X11Point::default(),
            preedit_font: None,
            preedit_state: PreeditState::Inactive,
        };

        self.contexts.insert(ic_id, ctx);
        ic_id
    }

    /// Destroy an input context.
    pub fn destroy_ic(&mut self, ic_id: u16) {
        self.contexts.remove(&ic_id);
    }

    /// Update spot location for an input context.
    /// This is called when the X11 client sends XIM_SET_IC_VALUES
    /// with XNSpotLocation attribute.
    pub fn update_spot_location(&mut self, ic_id: u16, spot: X11Point) {
        if let Some(ctx) = self.contexts.get_mut(&ic_id) {
            ctx.spot_location = spot;
            // The display backend must be notified to update the
            // NSView's ime_spot ivar, so that macOS can query the
            // correct position via firstRectForCharacterRange.
        }
    }

    /// Get the spot location for a window.
    pub fn get_spot_for_window(&self, window: Xid) -> Option<X11Point> {
        self.contexts.values()
            .find(|ctx| ctx.window == window)
            .map(|ctx| ctx.spot_location)
    }

    /// Handle IME commit (from macOS insertText:).
    /// Returns the committed text and the target window.
    pub fn handle_commit(&mut self, window: Xid, text: &str) -> Option<(u16, String)> {
        if let Some(ctx) = self.contexts.values_mut().find(|c| c.window == window) {
            ctx.preedit_state = PreeditState::Inactive;
            Some((ctx.ic_id, text.to_string()))
        } else {
            None
        }
    }

    /// Handle preedit update (from macOS setMarkedText:).
    pub fn handle_preedit_update(
        &mut self,
        window: Xid,
        text: &str,
        cursor_pos: usize,
    ) -> Option<u16> {
        if let Some(ctx) = self.contexts.values_mut().find(|c| c.window == window) {
            ctx.preedit_state = PreeditState::Active {
                text: text.to_string(),
                cursor_pos,
            };
            Some(ctx.ic_id)
        } else {
            None
        }
    }

    /// Handle preedit end (from macOS unmarkText).
    pub fn handle_preedit_end(&mut self, window: Xid) -> Option<u16> {
        if let Some(ctx) = self.contexts.values_mut().find(|c| c.window == window) {
            ctx.preedit_state = PreeditState::Inactive;
            Some(ctx.ic_id)
        } else {
            None
        }
    }
}
