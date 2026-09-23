//! The overview's on-screen half: a plain-data [`OverviewScene`] built by the
//! ECS each frame, and the [`OverviewRenderer`] that draws it in a borderless
//! window above every application.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use bevy::math::{IRect, IVec2};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSCompositingOperation, NSFont, NSGraphicsContext, NSImage,
    NSParagraphStyle, NSRunningApplication, NSScreenSaverWindowLevel, NSView, NSWindow,
};
use objc2_core_foundation::CGFloat;
use objc2_core_graphics::CGBitmapContextCreateImage;
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableCopying, NSPoint, NSRect, NSSize, NSString,
};

use super::{cg_abs_to_cocoa, make_overlay_window, primary_screen_height};
use crate::events::EventSender;
use crate::platform::{Pid, WinID};
use crate::util::rgba_bitmap_context;

/// Everything one overview frame needs, as plain data. Compared against the
/// previous frame so an unchanged scene skips the redraw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverviewScene {
    /// The whole display the overview covers, in absolute CG coordinates.
    pub display: IRect,
    /// 0.0 = closed, 1.0 = fully open. Fades the scrim and tiles.
    pub progress: f32,
    pub scrim_opacity: f32,
    pub scrim_color: [f64; 3],
    /// Height of each band's label strip, in points.
    pub label_height: i32,
    /// Whether to capture window thumbnails (`[overview] thumbnails`).
    pub thumbnails: bool,
    pub rows: Vec<SceneRow>,
    pub tiles: Vec<SceneTile>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneRow {
    /// Absolute CG coordinates.
    pub band: IRect,
    pub label: String,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneTile {
    pub window_id: WinID,
    pub pid: Pid,
    /// Absolute CG coordinates, already interpolated for `progress`.
    pub frame: IRect,
    pub title: String,
    pub tab_count: usize,
    pub selected: bool,
    pub hovered: bool,
}

fn ns_rect(rect: IRect) -> NSRect {
    NSRect::new(
        NSPoint::new(f64::from(rect.min.x), f64::from(rect.min.y)),
        NSSize::new(f64::from(rect.width()), f64::from(rect.height())),
    )
}

/// `rect` in the view's flipped local coordinates, given the display origin.
fn local_rect(rect: IRect, origin: IVec2) -> NSRect {
    ns_rect(IRect::from_corners(rect.min - origin, rect.max - origin))
}

fn srgb(rgb: [f64; 3], alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        rgb[0] as CGFloat,
        rgb[1] as CGFloat,
        rgb[2] as CGFloat,
        alpha as CGFloat,
    )
}

const WHITE: [f64; 3] = [1.0, 1.0, 1.0];
const BAND_RADIUS: CGFloat = 12.0;

/// Draws `text` inside `rect` on one line, truncating the tail, either centred
/// or left-aligned. Same attributed-string idiom as `FlashMessageView`.
fn draw_text(text: &str, rect: NSRect, font: &NSFont, color: &NSColor, centered: bool) {
    let paragraph_style = unsafe {
        let style = NSParagraphStyle::defaultParagraphStyle().mutableCopy();
        // NSTextAlignmentLeft = 0, NSTextAlignmentCenter = 1.
        let _: () = msg_send![&style, setAlignment: isize::from(centered)];
        let _: () = msg_send![&style, setLineBreakMode: 4isize]; // NSLineBreakByTruncatingTail
        style
    };
    let font_key = NSString::from_str("NSFont");
    let color_key = NSString::from_str("NSColor");
    let para_key = NSString::from_str("NSParagraphStyle");
    let keys = [&*font_key, &*color_key, &*para_key];
    let objects = [
        font as &AnyObject,
        color as &AnyObject,
        &*paragraph_style as &AnyObject,
    ];
    let attributes = NSDictionary::from_slices(&keys, &objects);
    let message = NSString::from_str(text);
    unsafe {
        let string: Retained<NSAttributedString> = msg_send![
            NSAttributedString::alloc(),
            initWithString: &*message,
            attributes: &*attributes
        ];
        let _: () = msg_send![&string, drawInRect: rect];
    }
}

/// A row band: rounded outline plus its label in the strip above the tiles.
fn draw_row(row: &SceneRow, origin: IVec2, label_height: i32, progress: f64) {
    let band = local_rect(row.band, origin);
    let (alpha, width) = if row.is_active {
        (0.35, 2.0)
    } else {
        (0.12, 1.0)
    };
    let path =
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(band, BAND_RADIUS, BAND_RADIUS);
    path.setLineWidth(width);
    srgb(WHITE, alpha * progress).setStroke();
    path.stroke();

    if label_height <= 0 {
        return;
    }
    let height = f64::from(label_height);
    let font_size = (height * 0.7).max(1.0);
    let font = if row.is_active {
        NSFont::boldSystemFontOfSize(font_size)
    } else {
        NSFont::systemFontOfSize(font_size)
    };
    let text_alpha = if row.is_active { 0.95 } else { 0.6 };
    let label = NSRect::new(
        NSPoint::new(
            band.origin.x + BAND_RADIUS,
            band.origin.y + (height - font_size) / 4.0,
        ),
        NSSize::new((band.size.width - 2.0 * BAND_RADIUS).max(1.0), height),
    );
    draw_text(
        &row.label,
        label,
        &font,
        &srgb(WHITE, text_alpha * progress),
        false,
    );
}

const TILE_FILL: [f64; 3] = [0.16, 0.16, 0.18];
const SELECTED: [f64; 3] = [0.30, 0.60, 1.0];
const TILE_RADIUS: CGFloat = 10.0;
const TITLE_FONT_SIZE: CGFloat = 12.0;

/// Draws `image` over the whole of `rect` at `alpha`.
fn draw_image(image: &NSImage, rect: NSRect, alpha: f64) {
    unsafe {
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            rect,
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            alpha,
            true,
            None,
        );
    }
}

/// One window: its captured thumbnail if one has arrived, otherwise a rounded
/// card with the app icon centred and the title beneath it.
fn draw_tile(
    tile: &SceneTile,
    icon: Option<&NSImage>,
    thumbnail: Option<&NSImage>,
    origin: IVec2,
    progress: f64,
) {
    let rect = local_rect(tile.frame, origin);
    if rect.size.width < 1.0 || rect.size.height < 1.0 {
        return;
    }
    let radius = TILE_RADIUS
        .min(rect.size.width / 2.0)
        .min(rect.size.height / 2.0);
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
    srgb(TILE_FILL, 0.92 * progress).setFill();
    path.fill();
    let (border, alpha, width) = if tile.selected {
        (SELECTED, progress, 3.0)
    } else if tile.hovered {
        (WHITE, 0.5 * progress, 2.0)
    } else {
        (WHITE, 0.18 * progress, 1.0)
    };
    path.setLineWidth(width);
    srgb(border, alpha).setStroke();

    if let Some(thumbnail) = thumbnail
        && let Some(context) = NSGraphicsContext::currentContext()
    {
        context.saveGraphicsState();
        path.addClip();
        draw_image(thumbnail, rect, progress);
        context.restoreGraphicsState();
        path.stroke();
        return;
    }
    path.stroke();

    let title_height = TITLE_FONT_SIZE * 1.5;
    let gap = 6.0;
    let icon_size = 64.0_f64
        .min(rect.size.width * 0.5)
        .min((rect.size.height - title_height - gap) * 0.6)
        .max(0.0);
    let content_height = icon_size + gap + title_height;
    let top = rect.origin.y + (rect.size.height - content_height).max(0.0) / 2.0;

    if let Some(icon) = icon
        && icon_size >= 8.0
    {
        let icon_rect = NSRect::new(
            NSPoint::new(rect.origin.x + (rect.size.width - icon_size) / 2.0, top),
            NSSize::new(icon_size, icon_size),
        );
        draw_image(icon, icon_rect, progress);
    }

    let caption = if tile.tab_count > 1 {
        format!("{} · {} tabs", tile.title, tile.tab_count)
    } else {
        tile.title.clone()
    };
    let padding = 8.0;
    let title_rect = NSRect::new(
        NSPoint::new(rect.origin.x + padding, top + icon_size + gap),
        NSSize::new((rect.size.width - 2.0 * padding).max(1.0), title_height),
    );
    let font = NSFont::systemFontOfSize(TITLE_FONT_SIZE);
    draw_text(
        &caption,
        title_rect,
        &font,
        &srgb(WHITE, 0.9 * progress),
        true,
    );
}

// ── OverviewView ────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct OverviewViewState {
    scene: OverviewScene,
    icons: HashMap<Pid, Retained<NSImage>>,
    /// Window captures, filled in as they arrive. Dropped with the view when
    /// the overview closes: a stale thumbnail is worse than a placeholder.
    thumbnails: HashMap<WinID, Retained<NSImage>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruOverviewView"]
    #[ivars = RefCell<OverviewViewState>]
    #[derive(Debug)]
    struct OverviewView;

    impl OverviewView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let state = self.ivars().borrow();
            let scene = &state.scene;
            let progress = f64::from(scene.progress);

            srgb(scene.scrim_color, f64::from(scene.scrim_opacity) * progress).setFill();
            NSBezierPath::fillRect(self.bounds());

            let origin = scene.display.min;
            for row in &scene.rows {
                draw_row(row, origin, scene.label_height, progress);
            }
            for tile in &scene.tiles {
                let icon = state.icons.get(&tile.pid).map(|icon| &**icon);
                let thumbnail = state.thumbnails.get(&tile.window_id).map(|image| &**image);
                draw_tile(tile, icon, thumbnail, origin, progress);
            }
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl OverviewView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// Owns the overview window. Exists for the process lifetime; the window only
/// while the overview is open.
pub struct OverviewRenderer {
    mtm: MainThreadMarker,
    window: Option<(Retained<NSWindow>, Retained<OverviewView>)>,
    scene: Option<OverviewScene>,
    /// Where captured thumbnails are delivered.
    #[cfg_attr(not(feature = "thumbnails"), allow(dead_code))]
    events: EventSender,
    /// Windows a capture was already asked for during this open.
    requested: HashSet<WinID>,
}

impl OverviewRenderer {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            window: None,
            scene: None,
            events,
            requested: HashSet::new(),
        }
    }

    /// Shows `scene`, creating the window on first use. A no-op when the scene
    /// matches the last one drawn.
    pub fn render(&mut self, scene: OverviewScene) {
        if self.scene.as_ref() == Some(&scene) {
            return;
        }
        let frame = cg_abs_to_cocoa(ns_rect(scene.display), primary_screen_height(self.mtm));
        let resized = self.scene.as_ref().map(|old| old.display) != Some(scene.display);
        let (window, view) = self.window.get_or_insert_with(|| {
            let window = make_overlay_window(self.mtm, frame);
            // Above every application window, the menu bar and the Dock.
            // Mouse events stay ignored: clicks reach the overview through the
            // event tap, so the window never becomes key.
            window.setLevel(NSScreenSaverWindowLevel);
            let view = OverviewView::new(self.mtm, NSRect::new(NSPoint::ZERO, frame.size));
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            (window, view)
        });
        if resized {
            window.setFrame_display(frame, false);
        }

        {
            let mut state = view.ivars().borrow_mut();
            for tile in &scene.tiles {
                state.icons.entry(tile.pid).or_insert_with(|| {
                    NSRunningApplication::runningApplicationWithProcessIdentifier(tile.pid)
                        .and_then(|app| app.icon())
                        .unwrap_or_default()
                });
            }
            state.scene = scene.clone();
        }
        view.setNeedsDisplay(true);

        // Only once the window is up and settled: a slow capture round-trip
        // must never hold up the open animation, and settled tiles have their
        // final size to capture at.
        if scene.thumbnails && scene.progress >= 1.0 {
            let scale = window.backingScaleFactor();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pixels = |points: i32| (f64::from(points) * scale).round().max(1.0) as u32;
            let requests = scene
                .tiles
                .iter()
                .filter(|tile| self.requested.insert(tile.window_id))
                .map(|tile| {
                    (
                        tile.window_id,
                        pixels(tile.frame.width()),
                        pixels(tile.frame.height()),
                    )
                })
                .collect::<Vec<_>>();
            #[cfg(feature = "thumbnails")]
            crate::manager::capture::request_thumbnails(requests, self.events.clone());
            #[cfg(not(feature = "thumbnails"))]
            drop(requests);
        }
        self.scene = Some(scene);
    }

    /// Caches a captured thumbnail, rebuilding the image here on the main
    /// thread from the plain RGBA bytes the capture callback sent.
    pub fn store_thumbnail(
        &mut self,
        window_id: WinID,
        width: u32,
        height: u32,
        mut rgba: Vec<u8>,
    ) {
        let Some((_, view)) = &self.window else {
            return;
        };
        let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
            return;
        };
        if rgba.len() < width * height * 4 {
            return;
        }
        let Some(context) =
            (unsafe { rgba_bitmap_context(rgba.as_mut_ptr().cast(), width, height) })
        else {
            return;
        };
        let Some(image) = CGBitmapContextCreateImage(Some(&context)) else {
            return;
        };
        let image = NSImage::initWithCGImage_size(NSImage::alloc(), &image, NSSize::ZERO);
        view.ivars()
            .borrow_mut()
            .thumbnails
            .insert(window_id, image);
        view.setNeedsDisplay(true);
    }

    /// Takes the window down and forgets everything drawn in it.
    pub fn close(&mut self) {
        if let Some((window, view)) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
            // Explicitly, rather than trusting AppKit to free the view with
            // the window: a stale thumbnail is worse than a placeholder.
            *view.ivars().borrow_mut() = OverviewViewState::default();
        }
        self.scene = None;
        self.requested.clear();
    }
}
