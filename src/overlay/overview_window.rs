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
    NSParagraphStyle, NSRunningApplication, NSScreen, NSScreenSaverWindowLevel, NSView, NSWindow,
    NSWorkspace,
};
use objc2_core_foundation::CGFloat;
use objc2_core_graphics::{CGBitmapContextCreateImage, CGDirectDisplayID};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableCopying, NSPoint, NSRect, NSSize, NSString,
};

use super::{cg_abs_to_cocoa, make_overlay_window, primary_screen_height};
use crate::events::EventSender;
use crate::platform::input::set_overview_window;
use crate::platform::{Pid, WinID};
use crate::util::{read_screen_property, rgba_bitmap_context};

/// Everything one overview frame needs, as plain data. Compared against the
/// previous frame so an unchanged scene skips the redraw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverviewScene {
    /// The whole display the overview covers, in absolute CG coordinates.
    pub display: IRect,
    /// That display's id, for looking up its desktop picture.
    pub display_id: CGDirectDisplayID,
    /// 0.0 = closed, 1.0 = fully open. Fades the scrim and tiles.
    pub progress: f32,
    pub scrim_opacity: f32,
    pub scrim_color: [f64; 3],
    /// Whether to capture window thumbnails (`[overview] thumbnails`).
    pub thumbnails: bool,
    pub tiles: Vec<SceneTile>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneTile {
    pub window_id: WinID,
    pub pid: Pid,
    /// Absolute CG coordinates, already interpolated for `progress` and the
    /// slide.
    pub frame: IRect,
    pub title: String,
    pub tab_count: usize,
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

const TILE_FILL: [f64; 3] = [0.16, 0.16, 0.18];
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

/// Where to draw `image` so it covers `bounds` at its own aspect ratio:
/// scaled up or down to fill, centred, overflowing on one axis.
fn aspect_fill(image: NSSize, bounds: NSRect) -> NSRect {
    if image.width <= 0.0 || image.height <= 0.0 {
        return bounds;
    }
    let scale = (bounds.size.width / image.width).max(bounds.size.height / image.height);
    let size = NSSize::new(image.width * scale, image.height * scale);
    NSRect::new(
        NSPoint::new(
            bounds.origin.x + (bounds.size.width - size.width) / 2.0,
            bounds.origin.y + (bounds.size.height - size.height) / 2.0,
        ),
        size,
    )
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
    let (alpha, width) = if tile.hovered {
        (0.5 * progress, 2.0)
    } else {
        (0.18 * progress, 1.0)
    };
    path.setLineWidth(width);
    srgb(WHITE, alpha).setStroke();

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
    /// The display's desktop picture, if it has a still one.
    wallpaper: Option<Retained<NSImage>>,
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
            let bounds = self.bounds();

            // Opaque once open either way, so no live window shows through.
            // ponytail: a full-size wallpaper is rescaled on every frame of the
            // zoom; if that stutters, pre-render it once at display size with
            // `rgba_bitmap_context` + `CGBitmapContextCreateImage` and cache it.
            if let Some(wallpaper) = &state.wallpaper {
                draw_image(wallpaper, aspect_fill(wallpaper.size(), bounds), progress);
            } else {
                srgb(scene.scrim_color, progress).setFill();
                NSBezierPath::fillRect(bounds);
            }
            srgb(scene.scrim_color, f64::from(scene.scrim_opacity) * progress).setFill();
            NSBezierPath::fillRect(bounds);

            let origin = scene.display.min;
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

/// Rebuilds an image, here on the main thread, from the plain RGBA bytes a
/// capture callback sent.
fn rgba_image(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Retained<NSImage>> {
    let (width, height) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if rgba.len() < width * height * 4 {
        return None;
    }
    let context = unsafe { rgba_bitmap_context(rgba.as_mut_ptr().cast(), width, height) }?;
    let image = CGBitmapContextCreateImage(Some(&context))?;
    Some(NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &image,
        NSSize::ZERO,
    ))
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
    /// The last desktop picture decoded, keyed by its URL. Kept across opens,
    /// so only the first open pays for decoding it.
    wallpaper: Option<(String, Retained<NSImage>)>,
    /// Desktop pictures captured because their file couldn't be loaded, per
    /// display. Kept across opens: shown at once, then refreshed by each open.
    captured: HashMap<CGDirectDisplayID, Retained<NSImage>>,
    /// The display whose captured picture this open shows, so a capture
    /// arriving for it replaces the backdrop.
    awaiting: Option<CGDirectDisplayID>,
    /// THROWAWAY Phase 1 probe (removed in Phase 3): live streams per window.
    #[cfg(feature = "thumbnails")]
    probe: HashMap<WinID, crate::manager::capture::LiveStream>,
}

impl OverviewRenderer {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            window: None,
            scene: None,
            events,
            requested: HashSet::new(),
            wallpaper: None,
            captured: HashMap::new(),
            awaiting: None,
            #[cfg(feature = "thumbnails")]
            probe: HashMap::new(),
        }
    }

    /// `display`'s desktop picture. `None` for a wallpaper with no still image
    /// behind it (dynamic, aerial) or one that fails to load.
    fn wallpaper(&mut self, display: CGDirectDisplayID) -> Option<Retained<NSImage>> {
        let url = read_screen_property(&NSScreen::screens(self.mtm), display, |screen| {
            NSWorkspace::sharedWorkspace().desktopImageURLForScreen(&screen)
        })
        .flatten()?;
        let key = url.absoluteString()?.to_string();
        if let Some((cached, image)) = &self.wallpaper
            && *cached == key
        {
            return Some(image.clone());
        }
        let image = NSImage::initWithContentsOfURL(NSImage::alloc(), &url)?;
        self.wallpaper = Some((key, image.clone()));
        Some(image)
    }

    /// Shows `scene`, creating the window on first use. A no-op when the scene
    /// matches the last one drawn.
    // THROWAWAY Phase 1 probe pushes this over the limit; gone in Phase 3.
    #[allow(clippy::too_many_lines)]
    pub fn render(&mut self, scene: OverviewScene) {
        if self.scene.as_ref() == Some(&scene) {
            return;
        }
        let frame = cg_abs_to_cocoa(ns_rect(scene.display), primary_screen_height(self.mtm));
        let resized = self.scene.as_ref().map(|old| old.display) != Some(scene.display);
        // Also true on the first frame of an open: `close` forgets the scene.
        let first = self.scene.as_ref().map(|old| old.display_id) != Some(scene.display_id);
        // An unreadable wallpaper file falls back to the last capture of the
        // display, if any, until this open's own capture lands.
        let wallpaper = first.then(|| {
            let file = self.wallpaper(scene.display_id);
            self.awaiting = file.is_none().then_some(scene.display_id);
            file.or_else(|| self.captured.get(&scene.display_id).cloned())
        });
        let (window, view) = self.window.get_or_insert_with(|| {
            let window = make_overlay_window(self.mtm, frame);
            // Above every application window, the menu bar and the Dock.
            window.setLevel(NSScreenSaverWindowLevel);
            // Hit-tested like any window, so the event tap can tell whether a
            // click lands on the overview or on something drawn above it. The
            // tap consumes the clicks it takes, so the window never becomes
            // key and never activates Paneru.
            window.setIgnoresMouseEvents(false);
            let view = OverviewView::new(self.mtm, NSRect::new(NSPoint::ZERO, frame.size));
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            set_overview_window(window.windowNumber());
            (window, view)
        });
        if resized {
            window.setFrame_display(frame, false);
        }

        {
            let mut state = view.ivars().borrow_mut();
            if let Some(wallpaper) = wallpaper {
                state.wallpaper = wallpaper;
            }
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

        let scale = window.backingScaleFactor();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pixels = |points: i32| (f64::from(points) * scale).round().max(1.0) as u32;
        // Right away, unlike thumbnails: the capture overlaps the fade-in.
        #[cfg(feature = "thumbnails")]
        if first && scene.thumbnails && self.awaiting == Some(scene.display_id) {
            crate::manager::capture::request_wallpaper(
                scene.display_id,
                scene.display,
                pixels(scene.display.width()),
                pixels(scene.display.height()),
                self.events.clone(),
            );
        }

        // Only once the window is up and settled: a slow capture round-trip
        // must never hold up the open animation, and settled tiles have their
        // final size to capture at.
        if scene.thumbnails && scene.progress >= 1.0 {
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

        // THROWAWAY Phase 1 probe (removed in Phase 3): once settled, stream
        // each tile into a plain sublayer, to see whether covered windows keep
        // updating.
        #[cfg(feature = "thumbnails")]
        if scene.thumbnails && scene.progress >= 1.0 {
            use crate::manager::capture::{FrameSink, start_streams};
            use dispatch2::MainThreadBound;
            use objc2_io_surface::IOSurfaceRef;
            use objc2_quartz_core::{CALayer, CATransaction};
            use std::sync::Arc;

            view.setWantsLayer(true);
            if let Some(host) = view.layer() {
                let height = view.bounds().size.height;
                let flipped = host.isGeometryFlipped();
                let mut requests = Vec::new();
                for tile in &scene.tiles {
                    if tile.window_id == 0 || self.probe.contains_key(&tile.window_id) {
                        continue;
                    }
                    let mut rect = local_rect(tile.frame, scene.display.min);
                    if !flipped {
                        rect.origin.y = height - rect.origin.y - rect.size.height;
                    }
                    let layer = CALayer::new();
                    layer.setFrame(rect);
                    host.addSublayer(&layer);
                    let sink: FrameSink = Arc::new(MainThreadBound::new(
                        Box::new(move |surface: &IOSurfaceRef| {
                            CATransaction::begin();
                            CATransaction::setDisableActions(true);
                            let contents = std::ptr::from_ref(surface).cast::<AnyObject>();
                            unsafe { layer.setContents(Some(&*contents)) };
                            CATransaction::commit();
                        }),
                        self.mtm,
                    ));
                    requests.push((
                        tile.window_id,
                        pixels(tile.frame.width()),
                        pixels(tile.frame.height()),
                        sink,
                    ));
                }
                tracing::debug!(flipped, count = requests.len(), "overview probe streams");
                self.probe.extend(start_streams(requests));
            }
        }
        self.scene = Some(scene);
    }

    /// Caches a captured thumbnail.
    pub fn store_thumbnail(&mut self, window_id: WinID, width: u32, height: u32, rgba: Vec<u8>) {
        let Some((_, view)) = &self.window else {
            return;
        };
        let Some(image) = rgba_image(width, height, rgba) else {
            return;
        };
        view.ivars()
            .borrow_mut()
            .thumbnails
            .insert(window_id, image);
        view.setNeedsDisplay(true);
    }

    /// Caches a captured desktop picture, and shows it if this open is
    /// waiting for one of that display.
    pub fn store_wallpaper(
        &mut self,
        display_id: CGDirectDisplayID,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) {
        let Some(image) = rgba_image(width, height, rgba) else {
            return;
        };
        self.captured.insert(display_id, image.clone());
        if let Some((_, view)) = &self.window
            && self.awaiting == Some(display_id)
        {
            view.ivars().borrow_mut().wallpaper = Some(image);
            view.setNeedsDisplay(true);
        }
    }

    /// Takes the window down and forgets everything drawn in it.
    pub fn close(&mut self) {
        #[cfg(feature = "thumbnails")]
        self.probe.clear();
        set_overview_window(0);
        if let Some((window, view)) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
            // Explicitly, rather than trusting AppKit to free the view with
            // the window: a stale thumbnail is worse than a placeholder.
            *view.ivars().borrow_mut() = OverviewViewState::default();
        }
        self.scene = None;
        self.requested.clear();
        self.awaiting = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp, reason = "exact binary fractions")]
    fn wide_wallpaper_fills_the_height_and_overflows_the_width_centred() {
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1000.0, 800.0));
        let rect = aspect_fill(NSSize::new(3000.0, 1000.0), bounds);
        assert_eq!(rect.size.height, 800.0);
        assert_eq!(rect.size.width, 2400.0);
        assert_eq!(rect.origin.x, -700.0);
        assert_eq!(rect.origin.y, 0.0);
        assert_eq!(aspect_fill(NSSize::new(0.0, 0.0), bounds), bounds);
    }
}
