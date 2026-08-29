use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSCompositingOperation, NSFloatingWindowLevel,
    NSFont, NSGraphicsContext, NSParagraphStyle, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableCopying, NSPoint, NSRect, NSSize, NSString,
};

#[derive(Clone, PartialEq)]
pub struct BorderParams {
    pub color: (f64, f64, f64),
    pub opacity: f64,
    pub width: f64,
    pub radius: f64,
}

/// Parameters for the fullscreen dim + cutout overlay.
#[derive(Clone, PartialEq)]
pub struct DimParams {
    pub opacity: f32,
    pub color: (f64, f64, f64),
    /// The focused window rect to cut out (in Cocoa screen coordinates).
    /// `None` means dim everything (no focused window).
    pub cutout: Option<NSRect>,
    pub border: Option<BorderParams>,
}

// ── DimView: fullscreen dark overlay with a transparent cutout + border ──

#[derive(Debug, Clone)]
struct DimViewIvars {
    opacity: f32,
    dim_r: f64,
    dim_g: f64,
    dim_b: f64,
    // Cutout rect in the view's local coordinates.
    cutout_x: f64,
    cutout_y: f64,
    cutout_w: f64,
    cutout_h: f64,
    has_cutout: bool,
    // Border params (only drawn if has_border is true).
    has_border: bool,
    border_r: f64,
    border_g: f64,
    border_b: f64,
    border_opacity: f64,
    border_width: f64,
    border_radius: f64,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruDimView"]
    #[ivars = DimViewIvars]
    #[derive(Debug)]
    struct DimView;

    impl DimView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let ivars = self.ivars();
            let bounds = self.bounds();

            // Fill the entire view with the dim color.
            let dim_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                ivars.dim_r as CGFloat,
                ivars.dim_g as CGFloat,
                ivars.dim_b as CGFloat,
                CGFloat::from(ivars.opacity),
            );
            dim_color.setFill();
            NSBezierPath::fillRect(bounds);

            if ivars.has_cutout {
                let half = if ivars.has_border { ivars.border_width / 2.0 } else { 0.0 };
                let radius = ivars.border_radius as CGFloat;

                // Expand the cutout by half the border width so the clear hole
                // extends just past the window edge. The border straddles the
                // window edge: outer half visible in the cutout, inner half
                // hidden behind the window.
                let cutout = NSRect::new(
                    NSPoint::new(ivars.cutout_x - half, ivars.cutout_y - half),
                    NSSize::new(ivars.cutout_w + ivars.border_width, ivars.cutout_h + ivars.border_width),
                );

                // Punch a rounded transparent hole using Clear compositing.
                if let Some(ctx) = NSGraphicsContext::currentContext() {
                    ctx.setCompositingOperation(NSCompositingOperation::Clear);
                    let hole = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        cutout, radius, radius,
                    );
                    hole.fill();
                    ctx.setCompositingOperation(NSCompositingOperation::SourceOver);
                }

                // Draw border centered on the window edge — half grows
                // outward (visible in the cutout), half grows inward (behind
                // the window).
                if ivars.has_border {
                    let border_rect = NSRect::new(
                        NSPoint::new(ivars.cutout_x, ivars.cutout_y),
                        NSSize::new(ivars.cutout_w, ivars.cutout_h),
                    );
                    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        border_rect, radius, radius,
                    );
                    path.setLineWidth(ivars.border_width as CGFloat);
                    let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                        ivars.border_r as CGFloat,
                        ivars.border_g as CGFloat,
                        ivars.border_b as CGFloat,
                        ivars.border_opacity as CGFloat,
                    );
                    border_color.setStroke();
                    path.stroke();
                }
            }
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl DimView {
    fn new(mtm: MainThreadMarker, frame: NSRect, params: &DimParams) -> Retained<Self> {
        let (has_cutout, cx, cy, cw, ch) = params.cutout.map_or((false, 0.0, 0.0, 0.0, 0.0), |r| {
            (true, r.origin.x, r.origin.y, r.size.width, r.size.height)
        });
        let (has_border, br, bg, bb, bo, bw, brad) =
            params
                .border
                .as_ref()
                .map_or((false, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0), |b| {
                    (
                        true, b.color.0, b.color.1, b.color.2, b.opacity, b.width, b.radius,
                    )
                });
        let this = Self::alloc(mtm).set_ivars(DimViewIvars {
            opacity: params.opacity,
            dim_r: params.color.0,
            dim_g: params.color.1,
            dim_b: params.color.2,
            cutout_x: cx,
            cutout_y: cy,
            cutout_w: cw,
            cutout_h: ch,
            has_cutout,
            has_border,
            border_r: br,
            border_g: bg,
            border_b: bb,
            border_opacity: bo,
            border_width: bw,
            border_radius: brad,
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

// ── Coordinate helpers ──────────────────────────────────────────────────

/// Convert an absolute CG screen frame (origin top-left, y-down) to Cocoa
/// screen coordinates (origin bottom-left of primary screen, y-up).
fn cg_abs_to_cocoa(frame: NSRect, primary_screen_height: f64) -> NSRect {
    let cocoa_y = primary_screen_height - frame.origin.y - frame.size.height;
    NSRect::new(NSPoint::new(frame.origin.x, cocoa_y), frame.size)
}

/// Height of the display at the global origin (the main display, whose Cocoa
/// frame origin is `(0, 0)`). The CG↔Cocoa Y-flip is anchored to this display,
/// so it must be *that* screen — `NSScreen::screens()[0]` is NOT reliably the
/// main display, and using the wrong one offsets the overlay (and, when
/// displays are stacked, lands it on the wrong monitor).
fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let mut fallback = 0.0;
    let mut first = true;
    for screen in &screens {
        let frame = screen.frame();
        if first {
            fallback = frame.size.height;
            first = false;
        }
        if frame.origin.x == 0.0 && frame.origin.y == 0.0 {
            return frame.size.height;
        }
    }
    fallback
}

/// Do two Cocoa rects overlap? Used to decide which displays a focused window
/// touches (a window straddling a seam touches both).
fn rects_intersect(a: NSRect, b: NSRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

// ── Overlay window factory ──────────────────────────────────────────────

fn make_overlay_window(mtm: MainThreadMarker, cocoa_frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cocoa_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenNone,
    );

    window
}
// ── OverlayManager ──────────────────────────────────────────────────────

pub struct OverlayManager {
    mtm: MainThreadMarker,
    /// One overlay window per display. macOS will not reliably let a single
    /// window span multiple displays (with "Displays have separate Spaces" it
    /// renders on only one), so each screen gets its own overlay drawn in that
    /// screen's local coordinates. Indexed in lockstep with `NSScreen::screens`.
    overlays: Vec<(Retained<NSWindow>, DimParams)>,
    hidden: bool,
}

impl OverlayManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            overlays: Vec::new(),
            hidden: false,
        }
    }

    /// Update the per-display overlays.
    /// `focused_abs_cg` is the focused window rect in absolute CG coords,
    /// or `None` if no window is focused.
    pub fn update(
        &mut self,
        dim_opacity: f32,
        dim_color: (f64, f64, f64),
        focused_abs_cg: Option<NSRect>,
        border: Option<&BorderParams>,
    ) {
        let screen_h = primary_screen_height(self.mtm);
        let screens = NSScreen::screens(self.mtm);

        // The focused window in Cocoa global coords (shared across all screens).
        let focused_cocoa = focused_abs_cg.map(|cg| cg_abs_to_cocoa(cg, screen_h));

        // A display was added/removed — tear down and rebuild from scratch.
        if self.overlays.len() != screens.len() {
            for (window, _) in self.overlays.drain(..) {
                window.orderOut(None::<&AnyObject>);
            }
        }

        for (i, screen) in (&screens).into_iter().enumerate() {
            let frame = screen.frame();

            // Cut out the focused window on every display it touches, each in
            // that display's local (flipped, top-left origin) coordinates. A
            // window straddling a seam draws on both, clipped to each.
            let cutout_local = focused_cocoa
                .filter(|wc| rects_intersect(*wc, frame))
                .map(|wc| {
                    NSRect::new(
                        NSPoint::new(
                            wc.origin.x - frame.origin.x,
                            (frame.origin.y + frame.size.height) - (wc.origin.y + wc.size.height),
                        ),
                        wc.size,
                    )
                });

            let params = DimParams {
                opacity: dim_opacity,
                color: dim_color,
                cutout: cutout_local,
                border: border.cloned(),
            };

            if let Some((window, stored)) = self.overlays.get_mut(i) {
                if *stored == params {
                    // Keep geometry in sync cheaply (no forced redraw).
                    window.setFrame_display(frame, false);
                } else {
                    let view = DimView::new(self.mtm, frame, &params);
                    window.setContentView(Some(&view));
                    window.setFrame_display(frame, true);
                    *stored = params;
                }
                if self.hidden {
                    window.orderFront(None::<&AnyObject>);
                }
            } else {
                let window = make_overlay_window(self.mtm, frame);
                let view = DimView::new(self.mtm, frame, &params);
                window.setContentView(Some(&view));
                window.orderFront(None::<&AnyObject>);
                self.overlays.push((window, params));
            }
        }
        self.hidden = false;
    }

    pub fn remove_all(&mut self) {
        for (window, _) in self.overlays.drain(..) {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = false;
    }

    pub fn hide_all(&mut self) {
        if self.hidden {
            return;
        }
        for (window, _) in &self.overlays {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = true;
    }
}

// ── FlashMessage ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FlashMessageViewIvars {
    opacity: f32,
    message: Retained<NSString>,
    is_badge: bool,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruFlashMessageView"]
    #[ivars = FlashMessageViewIvars]
    #[derive(Debug)]
    struct FlashMessageView;

    impl FlashMessageView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let ivars = self.ivars();
            let bounds = self.bounds();
            let is_badge = ivars.is_badge;

            // 1. Draw semi-transparent bezel
            let bezel_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                0.12, 0.12, 0.12,
                CGFloat::from(ivars.opacity * 0.88),
            );
            bezel_color.setFill();
            let radius = if is_badge {
                24.0
            } else {
                bounds.size.height / 2.0
            };
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                bounds, radius, radius,
            );
            path.fill();

            // Draw subtle border for contrast
            let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                1.0, 1.0, 1.0,
                CGFloat::from(ivars.opacity * 0.15),
            );
            border_color.setStroke();
            path.setLineWidth(1.0);
            path.stroke();

            // 2. Draw text
            let font = if is_badge {
                NSFont::boldSystemFontOfSize(bounds.size.height * 0.62)
            } else {
                NSFont::systemFontOfSize(30.0)
            };
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(
                1.0, 1.0, 1.0,
                CGFloat::from(ivars.opacity * 0.95),
            );

            let paragraph_style = unsafe {
                let style = NSParagraphStyle::defaultParagraphStyle().mutableCopy();
                let _: () = msg_send![&style, setAlignment: 1isize]; // Center (NSTextAlignmentCenter = 1)
                let _: () = msg_send![&style, setLineBreakMode: 4isize]; // NSLineBreakByTruncatingTail = 4
                style
            };

            let attr_str: Retained<NSAttributedString> = unsafe {
                let font_key = NSString::from_str("NSFont");
                let color_key = NSString::from_str("NSColor");
                let para_key = NSString::from_str("NSParagraphStyle");

                let keys = [&*font_key, &*color_key, &*para_key];
                let objects = [
                    &*font as &AnyObject,
                    &*color as &AnyObject,
                    &*paragraph_style as &AnyObject,
                ];

                let attributes = NSDictionary::from_slices(&keys, &objects);
                let alloc = NSAttributedString::alloc();
                msg_send![alloc, initWithString: &*ivars.message, attributes: &*attributes]
            };

            let text_size: NSSize = unsafe { msg_send![&attr_str, size] };

            let text_rect = if is_badge {
                NSRect::new(
                    NSPoint::new(
                        bounds.origin.x + (bounds.size.width - text_size.width) / 2.0,
                        bounds.origin.y + (bounds.size.height - text_size.height) / 2.0,
                    ),
                    text_size,
                )
            } else {
                let h_pad = 24.0;
                let available_width = (bounds.size.width - (2.0 * h_pad)).max(1.0);
                NSRect::new(
                    NSPoint::new(
                        bounds.origin.x + h_pad,
                        bounds.origin.y + (bounds.size.height - text_size.height) / 2.0,
                    ),
                    NSSize::new(available_width, text_size.height),
                )
            };

            unsafe {
                let _: () = msg_send![&attr_str, drawInRect: text_rect];
            };
        }
    }
);

impl FlashMessageView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        message: &str,
        opacity: f32,
        is_badge: bool,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(FlashMessageViewIvars {
            opacity,
            message: NSString::from_str(message),
            is_badge,
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct FlashMessageManager {
    mtm: MainThreadMarker,
    window: Option<Retained<NSWindow>>,
}

// ── Spatial Overview / jump map ─────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct OverviewRow {
    pub frame: NSRect,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct OverviewItem {
    pub frame: NSRect,
    pub mark: String,
    pub focused: bool,
}

#[derive(Debug)]
struct OverviewViewIvars {
    rows: Vec<OverviewRow>,
    items: Vec<OverviewItem>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruOverviewView"]
    #[ivars = OverviewViewIvars]
    #[derive(Debug)]
    struct OverviewView;

    impl OverviewView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let bounds = self.bounds();
            NSColor::colorWithSRGBRed_green_blue_alpha(0.035, 0.04, 0.055, 0.84).setFill();
            NSBezierPath::fillRect(bounds);

            for row in &self.ivars().rows {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.14, 0.15, 0.19, 0.9).setFill();
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    row.frame, 14.0, 14.0,
                );
                path.fill();
                draw_overview_text(&row.label, row.frame.origin.x + 12.0, row.frame.origin.y + row.frame.size.height - 26.0, 13.0, false);
            }

            for item in &self.ivars().items {
                if item.focused {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.18, 0.48, 0.78, 0.72).setFill();
                } else {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.25, 0.27, 0.33, 0.94).setFill();
                }
                let card = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    item.frame, 9.0, 9.0,
                );
                card.fill();
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, if item.focused { 0.9 } else { 0.24 }).setStroke();
                card.setLineWidth(if item.focused { 2.0 } else { 1.0 });
                card.stroke();

                let badge_size = 34.0_f64.min(item.frame.size.width * 0.55).min(item.frame.size.height * 0.55).max(18.0);
                let badge = NSRect::new(
                    NSPoint::new(
                        item.frame.origin.x + (item.frame.size.width - badge_size) / 2.0,
                        item.frame.origin.y + (item.frame.size.height - badge_size) / 2.0,
                    ),
                    NSSize::new(badge_size, badge_size),
                );
                NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.045, 0.06, 0.94).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    badge, badge_size / 2.0, badge_size / 2.0,
                ).fill();
                draw_overview_text(
                    &item.mark,
                    badge.origin.x,
                    badge.origin.y + (badge_size - 17.0) / 2.0,
                    15.0,
                    true,
                );
            }
        }
    }
);

fn draw_overview_text(text: &str, x: f64, y: f64, size: f64, centered: bool) {
    let font = NSFont::boldSystemFontOfSize(size);
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.96);
    let paragraph_style = unsafe {
        let style = NSParagraphStyle::defaultParagraphStyle().mutableCopy();
        let alignment = isize::from(centered);
        let _: () = msg_send![&style, setAlignment: alignment];
        style
    };
    let value = NSString::from_str(text);
    let font_key = NSString::from_str("NSFont");
    let color_key = NSString::from_str("NSColor");
    let para_key = NSString::from_str("NSParagraphStyle");
    let keys = [&*font_key, &*color_key, &*para_key];
    let objects = [
        &*font as &AnyObject,
        &*color as &AnyObject,
        &*paragraph_style as &AnyObject,
    ];
    let attributes = NSDictionary::from_slices(&keys, &objects);
    let styled_text: Retained<NSAttributedString> = unsafe {
        msg_send![NSAttributedString::alloc(), initWithString: &*value, attributes: &*attributes]
    };
    let width = if centered { 34.0 } else { 240.0 };
    unsafe {
        let _: () = msg_send![&styled_text, drawInRect: NSRect::new(NSPoint::new(x, y), NSSize::new(width, size + 4.0))];
    }
}

impl OverviewView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        rows: Vec<OverviewRow>,
        items: Vec<OverviewItem>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(OverviewViewIvars { rows, items });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct OverviewManager {
    mtm: MainThreadMarker,
    window: Option<Retained<NSWindow>>,
}

impl OverviewManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self { mtm, window: None }
    }

    pub fn show(
        &mut self,
        display_abs_cg: NSRect,
        rows: Vec<OverviewRow>,
        items: Vec<OverviewItem>,
    ) {
        let screen_h = primary_screen_height(self.mtm);
        let frame = cg_abs_to_cocoa(display_abs_cg, screen_h);
        let local = NSRect::new(NSPoint::new(0.0, 0.0), frame.size);
        let view = OverviewView::new(self.mtm, local, rows, items);
        if let Some(window) = &self.window {
            window.setContentView(Some(&view));
            window.setFrame_display(frame, true);
            window.orderFront(None::<&AnyObject>);
        } else {
            let window = make_overlay_window(self.mtm, frame);
            window.setLevel(NSFloatingWindowLevel + 2);
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            self.window = Some(window);
        }
    }

    pub fn remove(&mut self) {
        if let Some(window) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
        }
    }
}

impl FlashMessageManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self { mtm, window: None }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn show(&mut self, message: &str, opacity: f32, top_right_abs_cg: NSPoint) {
        let is_badge = message.chars().count() <= 2;
        let screen_h = primary_screen_height(self.mtm);

        let size = if is_badge {
            NSSize::new(150.0, 150.0)
        } else {
            let font = NSFont::systemFontOfSize(30.0);
            let font_key = NSString::from_str("NSFont");
            let keys = [&*font_key];
            let objects = [&*font as &AnyObject];
            let attributes = NSDictionary::from_slices(&keys, &objects);
            let msg_ns = NSString::from_str(message);
            let attr_str: Retained<NSAttributedString> = unsafe {
                let alloc = NSAttributedString::alloc();
                msg_send![alloc, initWithString: &*msg_ns, attributes: &*attributes]
            };
            let text_size: NSSize = unsafe { msg_send![&attr_str, size] };
            let horizontal_padding = 48.0;
            let max_width = 780.0;
            let min_width = 140.0;
            let width = (text_size.width + horizontal_padding).clamp(min_width, max_width);
            NSSize::new(width, 64.0)
        };

        let padding = 20.0;

        let cocoa_origin_x = top_right_abs_cg.x - size.width - padding;
        let cocoa_origin_y = screen_h - (top_right_abs_cg.y + size.height + padding);

        let frame = NSRect::new(NSPoint::new(cocoa_origin_x, cocoa_origin_y), size);

        if let Some(window) = &self.window {
            let view = FlashMessageView::new(
                self.mtm,
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                message,
                opacity,
                is_badge,
            );
            window.setContentView(Some(&view));
            window.setFrame_display(frame, true);
            window.orderFront(None::<&AnyObject>);
        } else {
            let window = make_overlay_window(self.mtm, frame);
            window.setLevel(NSFloatingWindowLevel + 1);
            let view = FlashMessageView::new(
                self.mtm,
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                message,
                opacity,
                is_badge,
            );
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            self.window = Some(window);
        }
    }

    pub fn remove(&mut self) {
        if let Some(window) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
        }
    }
}
