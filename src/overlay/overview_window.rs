//! The overview's on-screen half: a plain-data [`OverviewScene`] built by the
//! ECS each frame, and the [`OverviewRenderer`] that shows it as a Core
//! Animation layer tree in a borderless window above every application.

use std::collections::HashMap;

use bevy::math::{IRect, IVec2};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSFont, NSImage, NSRunningApplication, NSScreen, NSScreenSaverWindowLevel, NSView, NSWindow,
    NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CFType, CGFloat};
use objc2_core_graphics::{CGBitmapContextCreateImage, CGColor, CGDirectDisplayID};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CALayer, CATextLayer, CATransaction, kCAAlignmentCenter, kCAGravityResize,
    kCAGravityResizeAspect, kCAGravityResizeAspectFill, kCATruncationEnd,
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
    /// Where `frame` settles once open, in absolute CG coordinates. Known from
    /// the first frame of the open.
    pub target: IRect,
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

fn srgb(rgb: [f64; 3], alpha: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(rgb[0], rgb[1], rgb[2], alpha)
}

/// Runs `changes` with Core Animation's implicit animations off. Otherwise
/// every frame, contents and opacity change gets its own 0.25 s tween,
/// fighting the zoom's ease-out.
fn without_animation(changes: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    changes();
    CATransaction::commit();
}

const WHITE: [f64; 3] = [1.0, 1.0, 1.0];

const TILE_FILL: [f64; 3] = [0.16, 0.16, 0.18];
const TILE_RADIUS: CGFloat = 10.0;
const TITLE_FONT_SIZE: CGFloat = 12.0;

/// One tile's layers: a rounded card holding the window's live `content` once
/// a frame has arrived, and until then the app icon with the title beneath it.
#[derive(Clone)]
struct TileLayers {
    card: Retained<CALayer>,
    content: Retained<CALayer>,
    icon: Retained<CALayer>,
    title: Retained<CATextLayer>,
}

impl TileLayers {
    /// `scale` is the display's backing scale, for crisp icon and text.
    fn new(pid: Pid, scale: CGFloat) -> Self {
        let card = CALayer::new();
        card.setBackgroundColor(Some(&srgb(TILE_FILL, 0.92)));
        card.setMasksToBounds(true);

        let content = CALayer::new();
        content.setContentsGravity(unsafe { kCAGravityResize });
        content.setHidden(true);

        let icon = CALayer::new();
        icon.setContentsGravity(unsafe { kCAGravityResizeAspect });
        icon.setContentsScale(scale);
        if let Some(image) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|app| app.icon())
        {
            // SAFETY: an `NSImage` is one of the types `contents` takes.
            unsafe { icon.setContents(Some(&image)) };
        }

        let title = CATextLayer::new();
        let font = NSFont::systemFontOfSize(TITLE_FONT_SIZE);
        // SAFETY: `NSFont` is toll-free bridged with `CTFont`, one of the
        // types `font` takes.
        unsafe { title.setFont(Some(&*Retained::as_ptr(&font).cast::<CFType>())) };
        title.setFontSize(TITLE_FONT_SIZE);
        title.setForegroundColor(Some(&srgb(WHITE, 0.9)));
        title.setAlignmentMode(unsafe { kCAAlignmentCenter });
        title.setTruncationMode(unsafe { kCATruncationEnd });
        title.setContentsScale(scale);

        card.addSublayer(&content);
        card.addSublayer(&icon);
        card.addSublayer(&title);
        Self {
            card,
            content,
            icon,
            title,
        }
    }

    /// Places the card at `tile`'s drawn frame and lays out what's in it.
    fn layout(&self, tile: &SceneTile, origin: IVec2) {
        let rect = local_rect(tile.frame, origin);
        let NSSize { width, height } = rect.size;
        self.card.setHidden(width < 1.0 || height < 1.0);
        self.card.setFrame(rect);
        self.card
            .setCornerRadius(TILE_RADIUS.min(width / 2.0).min(height / 2.0));
        let (alpha, border) = if tile.hovered {
            (0.5, 2.0)
        } else {
            (0.18, 1.0)
        };
        self.card.setBorderColor(Some(&srgb(WHITE, alpha)));
        self.card.setBorderWidth(border);
        self.content.setFrame(NSRect::new(NSPoint::ZERO, rect.size));

        // The icon and title only stand in until the first frame.
        let framed = !self.content.isHidden();
        let title_height = TITLE_FONT_SIZE * 1.5;
        let gap = 6.0;
        let icon_size = 64.0_f64
            .min(width * 0.5)
            .min((height - title_height - gap) * 0.6)
            .max(0.0);
        let top = (height - (icon_size + gap + title_height)).max(0.0) / 2.0;
        self.icon.setHidden(framed || icon_size < 8.0);
        self.icon.setFrame(NSRect::new(
            NSPoint::new((width - icon_size) / 2.0, top),
            NSSize::new(icon_size, icon_size),
        ));

        let caption = if tile.tab_count > 1 {
            format!("{} · {} tabs", tile.title, tile.tab_count)
        } else {
            tile.title.clone()
        };
        let padding = 8.0;
        self.title.setHidden(framed);
        self.title.setFrame(NSRect::new(
            NSPoint::new(padding, top + icon_size + gap),
            NSSize::new((width - 2.0 * padding).max(1.0), title_height),
        ));
        // SAFETY: an `NSString` is one of the types `string` takes.
        unsafe { self.title.setString(Some(&NSString::from_str(&caption))) };
    }
}

define_class!(
    /// A view that only says it is flipped. AppKit keeps a hosted layer's
    /// `geometryFlipped` in step with `isFlipped`, resetting it on `setLayer`
    /// and every resize, so the top-left origin `local_rect` produces has to
    /// come from here rather than from the layer.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruOverviewView"]
    struct OverviewView;

    impl OverviewView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl OverviewView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// The overview window and the layers under its tiles. `host` is the content
/// view's layer, so the backdrop, the scrim and every tile are plain sublayers
/// drawn by Core Animation.
struct Stage {
    window: Retained<NSWindow>,
    host: Retained<CALayer>,
    /// Opaque either way, so no live window shows through: the desktop
    /// picture when there is one, over the scrim colour.
    backdrop: Retained<CALayer>,
    scrim: Retained<CALayer>,
}

impl Stage {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        let window = make_overlay_window(mtm, frame);
        // Above every application window, the menu bar and the Dock.
        window.setLevel(NSScreenSaverWindowLevel);
        // Hit-tested like any window, so the event tap can tell whether a
        // click lands on the overview or on something drawn above it. The
        // tap consumes the clicks it takes, so the window never becomes
        // key and never activates Paneru.
        window.setIgnoresMouseEvents(false);
        let host = CALayer::new();
        let view = OverviewView::new(mtm, NSRect::new(NSPoint::ZERO, frame.size));
        // Layer-hosting, not layer-backed: `setLayer` first, so AppKit leaves
        // the tree to us rather than making and owning a layer of its own.
        view.setLayer(Some(&host));
        view.setWantsLayer(true);
        window.setContentView(Some(&view));
        let backdrop = CALayer::new();
        backdrop.setContentsGravity(unsafe { kCAGravityResizeAspectFill });
        backdrop.setMasksToBounds(true);
        host.addSublayer(&backdrop);
        let scrim = CALayer::new();
        host.addSublayer(&scrim);
        window.orderFront(None::<&AnyObject>);
        set_overview_window(window.windowNumber());
        Self {
            window,
            host,
            backdrop,
            scrim,
        }
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
    window: Option<Stage>,
    scene: Option<OverviewScene>,
    /// Where captured wallpapers are delivered.
    #[cfg_attr(not(feature = "thumbnails"), allow(dead_code))]
    events: EventSender,
    /// The last desktop picture decoded, keyed by its URL. Kept across opens,
    /// so only the first open pays for decoding it.
    wallpaper: Option<(String, Retained<NSImage>)>,
    /// Desktop pictures captured because their file couldn't be loaded, per
    /// display. Kept across opens: shown at once, then refreshed by each open.
    captured: HashMap<CGDirectDisplayID, Retained<NSImage>>,
    /// The display whose captured picture this open shows, so a capture
    /// arriving for it replaces the backdrop.
    awaiting: Option<CGDirectDisplayID>,
    /// Each projected window's layers, created on its first frame.
    tiles: HashMap<WinID, TileLayers>,
}

impl OverviewRenderer {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            window: None,
            scene: None,
            events,
            wallpaper: None,
            captured: HashMap::new(),
            awaiting: None,
            tiles: HashMap::new(),
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
        let mtm = self.mtm;
        let stage = self.window.get_or_insert_with(|| Stage::new(mtm, frame));
        if resized {
            stage.window.setFrame_display(frame, false);
        }
        let scale = stage.window.backingScaleFactor();
        let bounds = NSRect::new(NSPoint::ZERO, frame.size);
        without_animation(|| {
            // Fades everything together, as one group.
            stage.host.setOpacity(scene.progress);
            stage.backdrop.setFrame(bounds);
            let opaque = srgb(scene.scrim_color, 1.0);
            stage.backdrop.setBackgroundColor(Some(&opaque));
            if let Some(wallpaper) = &wallpaper {
                // SAFETY: an `NSImage` is one of the types `contents` takes.
                unsafe {
                    stage
                        .backdrop
                        .setContents(wallpaper.as_deref().map(AsRef::as_ref));
                }
            }
            stage.scrim.setFrame(bounds);
            let scrim = srgb(scene.scrim_color, f64::from(scene.scrim_opacity));
            stage.scrim.setBackgroundColor(Some(&scrim));

            // Later tiles on top.
            for (tile, z) in scene.tiles.iter().zip(0_u32..) {
                // No window behind it: nothing to key its layers by.
                if tile.window_id == 0 {
                    continue;
                }
                let layers = self.tiles.entry(tile.window_id).or_insert_with(|| {
                    let layers = TileLayers::new(tile.pid, scale);
                    stage.host.addSublayer(&layers.card);
                    layers
                });
                layers.card.setZPosition(f64::from(z));
                layers.layout(tile, scene.display.min);
            }
            // A window closed while the overview is open.
            self.tiles.retain(|id, layers| {
                let projected = scene.tiles.iter().any(|tile| tile.window_id == *id);
                if !projected {
                    layers.card.removeFromSuperlayer();
                }
                projected
            });
        });

        #[cfg(feature = "thumbnails")]
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pixels = |points: i32| (f64::from(points) * scale).round().max(1.0) as u32;
        // Right away: the capture overlaps the fade-in.
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

        self.scene = Some(scene);
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
        if let Some(stage) = &self.window
            && self.awaiting == Some(display_id)
        {
            // SAFETY: an `NSImage` is one of the types `contents` takes.
            without_animation(|| unsafe { stage.backdrop.setContents(Some(&image)) });
        }
    }

    /// Takes the window down and forgets everything drawn in it.
    pub fn close(&mut self) {
        set_overview_window(0);
        if let Some(stage) = self.window.take() {
            stage.window.orderOut(None::<&AnyObject>);
        }
        self.tiles.clear();
        self.scene = None;
        self.awaiting = None;
    }
}
