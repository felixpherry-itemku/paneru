# Record

**Overview input.** While the overview is open the `CGEventTap` in `src/platform/input.rs` routes bare Escape, Return and arrow keys to the overview, runs Paneru keybindings as usual, passes every other command, option or control chord through to macOS and other applications, and swallows the remaining bare keys. Selection is the real focus: focus and workspace moves made while the overview is open act on the live layout behind it, and closing leaves focus where it is. A click is taken by the overview only when the overview window is under the pointer.

**Overview backdrop.** The overview draws an opaque backdrop — the display's desktop wallpaper, else the scrim colour — so no application window shows through it.

**Screen capture.** Overview tiles carry window thumbnails captured through ScreenCaptureKit on macOS 14 and later, which requires the Screen Recording permission. This is the only feature in Paneru that needs a permission beyond Accessibility; when it is unavailable, tiles fall back to the application icon and window title and nothing else degrades.

**Dynamic workspaces.** With `dynamic_workspaces` enabled, each macOS Space numbers its virtual workspaces 1..N without gaps: a workspace with no managed windows is removed as soon as no display is showing it, and exactly one empty workspace always follows the last occupied one. Switches and moves by number, `last` or southward never go past that trailing workspace. Native fullscreen Spaces are exempt.

**Floating round trip.** Tiling a floating window back inserts it as a new column right of the active column — the last-focused tiled window on the active row — or appends it when that row has none. Floating the active column hands that role to the column now at its index, or to its left neighbour if it was last; a window that was just tiled back hands it back to the column it was inserted beside. A window floats again at its last floating size and position, kept proportional to the display and fully on screen; a `grid` rule places only its first float.
