//! Window thumbnails and the desktop picture for the overview, captured with
//! `ScreenCaptureKit`.
//!
//! Everything here is best effort: below macOS 14, without the Screen Recording
//! permission, or for a window `ScreenCaptureKit` does not know, nothing arrives
//! and the overview keeps drawing its icon + title tile, or its scrim-coloured
//! backdrop. Nothing ever errors.

use std::collections::HashMap;

use bevy::math::IRect;
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGDirectDisplayID, CGImage};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};
use tracing::debug;

use crate::events::{Event, EventSender};
use crate::manager::irect_from;
use crate::platform::WinID;
use crate::util::rgba_bitmap_context;

/// Requests a thumbnail per `(window, pixel width, pixel height)`. Returns
/// immediately; each result arrives later as an `Event::OverviewThumbnail`.
pub fn request_thumbnails(requests: Vec<(WinID, u32, u32)>, events: EventSender) {
    if requests.is_empty() || !objc2::available!(macos = 14.0) {
        return;
    }

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // Null content is how a denied Screen Recording permission shows
            // up. Not an error worth surfacing: the tiles already have their
            // icon + title.
            let Some(content) = (unsafe { content.as_ref() }) else {
                debug!("no shareable content: {:?}", unsafe { error.as_ref() });
                return;
            };
            let by_id = unsafe { content.windows() }
                .iter()
                .map(|window| (unsafe { window.windowID() }, window))
                .collect::<HashMap<u32, Retained<SCWindow>>>();
            for &(window_id, width, height) in &requests {
                let Some(window) = u32::try_from(window_id).ok().and_then(|id| by_id.get(&id))
                else {
                    continue;
                };
                capture(
                    window,
                    width,
                    height,
                    events.clone(),
                    move |width, height, rgba| Event::OverviewThumbnail {
                        window_id,
                        width,
                        height,
                        rgba,
                    },
                );
            }
        },
    );
    // `onScreenWindowsOnly` still covers every managed window, including those
    // the overview exists to reveal: a parked strip keeps `PARKED_STRIP_SLIVER`
    // on screen and a scrolled-away column keeps `sliver_width`, precisely so
    // macOS never treats them as hidden. That parking design is load-bearing
    // here — a window pushed fully off-display would silently lose its
    // thumbnail.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &handler,
        );
    }
}

/// Requests the desktop picture macOS is showing on `display` (absolute CG
/// points, menubar included) at `width` x `height` pixels, by capturing the
/// Dock's `Wallpaper-` window for it: the picture alone, without desktop icons,
/// widgets or windows. Returns immediately; the result arrives later as an
/// `Event::OverviewWallpaper`.
pub fn request_wallpaper(
    display_id: CGDirectDisplayID,
    display: IRect,
    width: u32,
    height: u32,
    events: EventSender,
) {
    if !objc2::available!(macos = 14.0) {
        return;
    }

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let Some(content) = (unsafe { content.as_ref() }) else {
                debug!("no shareable content: {:?}", unsafe { error.as_ref() });
                return;
            };
            let windows = unsafe { content.windows() };
            let Some(window) = windows.iter().find(|window| {
                let bundle_id = unsafe { window.owningApplication() }
                    .map(|app| unsafe { app.bundleIdentifier() }.to_string());
                let title = unsafe { window.title() }.map(|title| title.to_string());
                is_wallpaper(
                    bundle_id.as_deref().unwrap_or_default(),
                    title.as_deref().unwrap_or_default(),
                    irect_from(unsafe { window.frame() }),
                    display,
                )
            }) else {
                debug!("no wallpaper window for display {display_id}");
                return;
            };
            capture(
                &window,
                width,
                height,
                events.clone(),
                move |width, height, rgba| Event::OverviewWallpaper {
                    display_id,
                    width,
                    height,
                    rgba,
                },
            );
        },
    );
    // Desktop windows included: the wallpaper is one.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &handler,
        );
    }
}

/// Screenshots one window at `width` x `height` pixels, occluded or partly
/// off-screen as it may be, and sends the event `make` builds from the pixels.
fn capture(
    window: &SCWindow,
    width: u32,
    height: u32,
    events: EventSender,
    make: impl Fn(u32, u32, Vec<u8>) -> Event + 'static,
) {
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setShowsCursor(false);
        config.setScalesToFit(true);
        config.setIgnoreShadowsSingleWindow(true);
    }
    let handler = RcBlock::new(move |image: *mut CGImage, _error: *mut NSError| {
        // Flattened to plain bytes right here, on ScreenCaptureKit's queue:
        // a `CGImage` is not `Send`, bytes are, so nothing else has to be.
        let Some((width, height, rgba)) = (unsafe { image.as_ref() }).and_then(rgba_bytes) else {
            return;
        };
        _ = events.send(make(width, height, rgba));
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&handler),
        );
    }
}

/// Draws `image` into a fresh RGBA buffer.
#[allow(clippy::cast_precision_loss)]
fn rgba_bytes(image: &CGImage) -> Option<(u32, u32, Vec<u8>)> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    let context = unsafe { rgba_bitmap_context(rgba.as_mut_ptr().cast(), width, height) }?;
    let bounds = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
    CGContext::draw_image(Some(&context), bounds, Some(image));
    drop(context);
    Some((
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        rgba,
    ))
}

/// Whether a window is the Dock's desktop-picture window covering `display`
/// (absolute CG points, menubar included).
fn is_wallpaper(bundle_id: &str, title: &str, frame: IRect, display: IRect) -> bool {
    bundle_id == "com.apple.dock" && title.starts_with("Wallpaper") && frame == display
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wallpaper_picks_the_dock_window_covering_the_display() {
        let display = IRect::new(0, 0, 1512, 982);
        let other = IRect::new(1512, 0, 3432, 1080);
        assert!(is_wallpaper(
            "com.apple.dock",
            "Wallpaper-",
            display,
            display
        ));
        assert!(
            !is_wallpaper("com.apple.finder", "", display, display),
            "desktop icons"
        );
        assert!(
            !is_wallpaper(
                "com.apple.notificationcenterui",
                "Month",
                IRect::new(20, 40, 200, 220),
                display
            ),
            "a widget"
        );
        assert!(
            !is_wallpaper("com.apple.dock", "Wallpaper-", other, display),
            "another display's wallpaper"
        );
        assert!(
            !is_wallpaper("com.apple.dock", "Dock", display, display),
            "another Dock window"
        );
    }
}
