# Record

**Overview input.** While the overview is open the `CGEventTap` in `src/platform/input.rs` consumes every key event and routes it to the overview instead of the focused application. Selection is deferred: moving the cursor changes no window state, and only activation (Enter or click) switches virtual workspace and moves focus. Cursor movement is delegated to `get_window_in_direction`, so overview navigation and the `window_focus_*` bindings share one rule.

**Screen capture.** Overview tiles carry window thumbnails captured through ScreenCaptureKit on macOS 14 and later, which requires the Screen Recording permission. This is the only feature in Paneru that needs a permission beyond Accessibility; when it is unavailable, tiles fall back to the application icon and window title and nothing else degrades.
