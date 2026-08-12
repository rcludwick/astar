// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Tray + popover window shell (astar-22cf).
//!
//! Mirrors the Mac's `StatusItemController`: a menu-bar/tray asterisk tinted
//! live by call state ([`status_icon`] ⇄ `StatusIconState`), a left click that
//! toggles the main window near the tray, a small Show/Hide + Quit menu, and
//! close-hides-to-tray while the tray keeps the app alive.
//!
//! Platform split:
//! * **Windows / macOS** — the `tray-icon` crate (Shell_NotifyIcon /
//!   NSStatusItem). The icon must be created on the thread that pumps native
//!   messages, so [`init`] runs on the main thread *before* the iced event
//!   loop starts; winit's loop then drives it. Events arrive on the crate's
//!   global channels, drained each poll tick by [`poll_events`].
//! * **Linux** — `ksni`, a pure-Rust StatusNotifierItem over D-Bus (no GTK/
//!   libappindicator C stack, which the build container doesn't carry). It
//!   runs on its own thread, and `spawn()` fails fast when the session has no
//!   StatusNotifier host — GNOME without the AppIndicator extension, or a
//!   headless X server — which is exactly the fallback signal: [`init`]
//!   records [`fallback_hint`] and the app runs as a normal window.
//!
//! The testable brains — snapshot→icon mapping, popover placement, the
//! fallback decision — are plain functions below; the per-platform plumbing
//! is thin glue at the bottom.

use std::sync::atomic::{AtomicBool, Ordering};

use iced::{Point, Size};

use crate::app::Mode;
use crate::icons::StatusIcon;
use crate::snapshot::{Snapshot, Status};

/// Whether a tray icon is live (set once by [`init`]).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Something the user did on the tray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// The icon was clicked: toggle the main window. `anchor` is the tray
    /// icon's on-screen rect when the platform reports one (Windows/macOS);
    /// Linux hosts rarely do.
    Toggle { anchor: Option<Anchor> },
    /// The menu's "Show/Hide astar Window" item.
    ShowHide,
    /// The menu's "Quit astar" item.
    Quit,
}

/// Where the tray icon sits on screen, in PHYSICAL pixels (what both
/// `tray-icon` rects and StatusNotifier activation coords speak). Width and
/// height are zero when the platform only gives a point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Map a snapshot to the tray asterisk's state — the Mac's `StatusIconState`
/// priority exactly: transmit > receive > connected > idle, where "connected"
/// requires an actual dialed call (a WT session can sit answered while idle,
/// which must not tint the asterisk).
#[must_use]
pub fn status_icon(snap: &Snapshot) -> StatusIcon {
    if snap.transmitting {
        StatusIcon::Transmitting
    } else if snap.receiving {
        StatusIcon::Receiving
    } else if snap.status == Status::Connected && snap.dialed_node.is_some() {
        StatusIcon::Connected
    } else {
        StatusIcon::Idle
    }
}

/// Whether this run mode should get a tray at all. `--shot` runs headless
/// (CI, Xvfb) and must never touch the tray; demo and live runs want it.
#[must_use]
pub fn wants_tray(mode: &Mode) -> bool {
    !matches!(mode, Mode::Shot { .. })
}

/// Gap between the tray icon and the window, and the screen-edge margin
/// (logical px). The Mac drops its window 4 pt under the asterisk; a touch
/// more breathing room reads better next to chunkier Windows/Linux taskbars.
const GAP: f32 = 8.0;
const MARGIN: f32 = 8.0;

/// Best-effort popover placement: center the window on the tray icon, below
/// it when the tray sits in the top half of the monitor (macOS menu bar,
/// top taskbars) and above it otherwise (the usual bottom taskbar), clamped
/// to the monitor. `anchor` is physical, `window`/`monitor` logical (iced's
/// `move_to` speaks logical coordinates), `scale` converts.
#[must_use]
pub fn popover_point(anchor: Anchor, window: Size, monitor: Option<Size>, scale: f32) -> Point {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let (ax, ay) = (anchor.x as f32 / scale, anchor.y as f32 / scale);
    let (aw, ah) = (anchor.w as f32 / scale, anchor.h as f32 / scale);

    let mut x = ax + aw / 2.0 - window.width / 2.0;
    // Without a monitor size, assume anchors near the top of the desktop are
    // a top bar; 200 logical px is far below any menu bar yet far above any
    // bottom taskbar.
    let below = match monitor {
        Some(m) => ay + ah / 2.0 < m.height / 2.0,
        None => ay < 200.0,
    };
    let mut y = if below {
        ay + ah + GAP
    } else {
        ay - window.height - GAP
    };

    if let Some(m) = monitor {
        x = x.clamp(MARGIN, (m.width - window.width - MARGIN).max(MARGIN));
        y = y.clamp(MARGIN, (m.height - window.height - MARGIN).max(MARGIN));
    } else {
        x = x.max(MARGIN);
        y = y.max(MARGIN);
    }
    Point::new(x, y)
}

/// The guidance shown when Linux has no tray host — the GNOME caveat from the
/// design doc. Rendered as a dim footer note by the view.
#[cfg(target_os = "linux")]
const LINUX_FALLBACK_HINT: &str = "No system tray found — running as a normal window. \
     On GNOME, install the “AppIndicator and KStatusNotifierItem Support” extension \
     to get the astar tray asterisk.";

/// Try to put the asterisk in the tray. Call ONCE from `main`, on the main
/// thread, before the iced event loop runs (a hard requirement on Windows,
/// where the icon belongs to the thread that pumps messages). Failure is the
/// graceful path: the app simply runs as a normal window.
pub fn init() {
    if backend::init() {
        ACTIVE.store(true, Ordering::Relaxed);
    }
}

/// Whether the tray is live. Drives close-to-hide vs close-to-quit.
#[must_use]
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The guidance note to show when the tray was wanted but unavailable —
/// `Some` only on Linux (the GNOME caveat); Windows/macOS trays are always
/// present, so a failure there stays silent.
#[must_use]
pub fn fallback_hint() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        if backend::attempted() && !is_active() {
            return Some(LINUX_FALLBACK_HINT);
        }
    }
    None
}

/// Drain pending tray events (icon clicks, menu picks). Called from the poll
/// tick; empty when the tray isn't live.
#[must_use]
pub fn poll_events() -> Vec<Event> {
    if !is_active() {
        return Vec::new();
    }
    backend::poll()
}

/// Re-tint the tray asterisk. No-op when the tray isn't live; callers gate on a
/// state *change* so this isn't hammered 20×/s.
pub fn set_icon(state: StatusIcon) {
    if is_active() {
        backend::set_icon(state);
    }
}

// ---------------------------------------------------------------------------
// Windows / macOS backend: the `tray-icon` crate.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod backend {
    use std::cell::RefCell;

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{Anchor, Event};
    use crate::icons::{status_asterisk_rgba, StatusIcon};

    const TOGGLE_ID: &str = "astar-toggle";
    const QUIT_ID: &str = "astar-quit";

    thread_local! {
        /// The live tray icon. `TrayIcon` is `!Send` and must stay on the
        /// thread that created it — the main thread, which is also where
        /// iced runs `update()`, so a thread-local fits exactly.
        static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    }

    /// The tray asterisk for `state` as a `tray_icon::Icon`. The 64 px rendering
    /// serves every backend: macOS scales it to 18 pt (crisp on retina) and
    /// Windows fits it to the small-icon metric.
    fn icon(state: StatusIcon) -> Option<tray_icon::Icon> {
        let (rgba, w, h) = status_asterisk_rgba(state, true)?;
        tray_icon::Icon::from_rgba(rgba, w, h).ok()
    }

    pub fn init() -> bool {
        let menu = Menu::new();
        let toggle = MenuItem::with_id(TOGGLE_ID, "Show/Hide astar Window", true, None);
        let quit = MenuItem::with_id(QUIT_ID, "Quit astar", true, None);
        if menu
            .append_items(&[&toggle, &PredefinedMenuItem::separator(), &quit])
            .is_err()
        {
            return false;
        }

        let mut builder = TrayIconBuilder::new()
            .with_id("astar")
            .with_tooltip("astar")
            .with_menu(Box::new(menu))
            // Left click toggles the window (the Mac's asterisk behavior); the
            // menu stays on right click.
            .with_menu_on_left_click(false);
        if let Some(icon) = icon(StatusIcon::Idle) {
            builder = builder.with_icon(icon);
        }

        match builder.build() {
            Ok(tray) => {
                TRAY.with(|t| *t.borrow_mut() = Some(tray));
                true
            }
            Err(_) => false,
        }
    }

    pub fn poll() -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = ev
            {
                out.push(Event::Toggle {
                    anchor: Some(Anchor {
                        x: rect.position.x,
                        y: rect.position.y,
                        w: f64::from(rect.size.width),
                        h: f64::from(rect.size.height),
                    }),
                });
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.as_ref() {
                TOGGLE_ID => out.push(Event::ShowHide),
                QUIT_ID => out.push(Event::Quit),
                _ => {}
            }
        }
        out
    }

    pub fn set_icon(state: StatusIcon) {
        TRAY.with(|t| {
            if let Some(tray) = t.borrow().as_ref() {
                let _ = tray.set_icon(icon(state));
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Linux backend: `ksni` (StatusNotifierItem over D-Bus).
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod backend {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Mutex, OnceLock};

    use ksni::blocking::{Handle, TrayMethods};
    use ksni::menu::{MenuItem, StandardItem};

    use super::{Anchor, Event};
    use crate::icons::{status_asterisk_rgba, StatusIcon};

    /// Whether `init` ran (regardless of outcome) — distinguishes "no tray
    /// host on this desktop" (show the GNOME hint) from "tray never wanted"
    /// (`--shot`, tests).
    static ATTEMPTED: AtomicBool = AtomicBool::new(false);
    static HANDLE: OnceLock<Handle<AstarTray>> = OnceLock::new();
    static EVENTS: OnceLock<Mutex<mpsc::Receiver<Event>>> = OnceLock::new();

    /// The StatusNotifierItem: holds the current star state and a sender into
    /// the app's poll loop. ksni calls the trait methods on its own thread.
    struct AstarTray {
        state: StatusIcon,
        tx: mpsc::Sender<Event>,
    }

    impl ksni::Tray for AstarTray {
        fn id(&self) -> String {
            "astar".into()
        }

        fn title(&self) -> String {
            "astar".into()
        }

        fn activate(&mut self, x: i32, y: i32) {
            // Most hosts pass (0,0); a real coordinate becomes the popover
            // anchor (a bare point — no icon rect over SNI).
            let anchor = (x != 0 || y != 0).then(|| Anchor {
                x: f64::from(x),
                y: f64::from(y),
                w: 0.0,
                h: 0.0,
            });
            let _ = self.tx.send(Event::Toggle { anchor });
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            [false, true]
                .iter()
                .filter_map(|&hidpi| argb_icon(self.state, hidpi))
                .collect()
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Show/Hide astar Window".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Event::ShowHide);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit astar".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Event::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// The tray asterisk for `state` in ksni's wire format: ARGB32, network byte
    /// order (the embedded PNGs are straight-alpha RGBA).
    fn argb_icon(state: StatusIcon, hidpi: bool) -> Option<ksni::Icon> {
        let (rgba, w, h) = status_asterisk_rgba(state, hidpi)?;
        let data = rgba
            .chunks_exact(4)
            .flat_map(|px| [px[3], px[0], px[1], px[2]])
            .collect();
        Some(ksni::Icon {
            width: w as i32,
            height: h as i32,
            data,
        })
    }

    pub fn init() -> bool {
        ATTEMPTED.store(true, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        // `spawn` registers with org.kde.StatusNotifierWatcher and fails fast
        // when there's no session bus or no watcher (GNOME without the
        // AppIndicator extension, headless CI) — the window-fallback signal.
        match (AstarTray {
            state: StatusIcon::Idle,
            tx,
        })
        .spawn()
        {
            Ok(handle) => {
                let _ = HANDLE.set(handle);
                let _ = EVENTS.set(Mutex::new(rx));
                true
            }
            Err(_) => false,
        }
    }

    pub fn attempted() -> bool {
        ATTEMPTED.load(Ordering::Relaxed)
    }

    pub fn poll() -> Vec<Event> {
        let Some(rx) = EVENTS.get() else {
            return Vec::new();
        };
        let Ok(rx) = rx.lock() else {
            return Vec::new();
        };
        rx.try_iter().collect()
    }

    pub fn set_icon(state: StatusIcon) {
        if let Some(handle) = HANDLE.get() {
            // ksni diffs the tray after `update` and emits NewIcon itself.
            let _ = handle.update(|t| t.state = state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(status: Status, dialed: bool, tx: bool, rx: bool) -> Snapshot {
        Snapshot {
            status,
            dialed_node: dialed.then(|| "546054".to_string()),
            transmitting: tx,
            receiving: rx,
            ..Snapshot::default()
        }
    }

    #[test]
    fn icon_priority_is_tx_over_rx_over_connected() {
        // The Mac's StatusIconState ordering, verbatim.
        let s = snap(Status::Connected, true, true, true);
        assert_eq!(status_icon(&s), StatusIcon::Transmitting);
        let s = snap(Status::Connected, true, false, true);
        assert_eq!(status_icon(&s), StatusIcon::Receiving);
        let s = snap(Status::Connected, true, false, false);
        assert_eq!(status_icon(&s), StatusIcon::Connected);
        let s = snap(Status::Disconnected, false, false, false);
        assert_eq!(status_icon(&s), StatusIcon::Idle);
    }

    #[test]
    fn connected_tint_requires_a_dialed_call() {
        // An answered-but-idle WT session must not tint the star (the Mac
        // gates on dialedNode != nil).
        let s = snap(Status::Connected, false, false, false);
        assert_eq!(status_icon(&s), StatusIcon::Idle);
        // Connecting isn't connected.
        let s = snap(Status::Connecting, true, false, false);
        assert_eq!(status_icon(&s), StatusIcon::Idle);
    }

    #[test]
    fn shot_mode_never_wants_a_tray() {
        use crate::app::ShotScene;
        use crate::conn::DemoState;
        let shot = Mode::Shot {
            scene: ShotScene::State(DemoState::Idle),
            file: "x.png".into(),
        };
        assert!(!wants_tray(&shot));
        assert!(wants_tray(&Mode::Real));
        assert!(wants_tray(&Mode::Demo(DemoState::Connected)));
    }

    const WIN: Size = Size::new(560.0, 700.0);

    #[test]
    fn popover_drops_below_a_top_bar_centered_on_the_icon() {
        // macOS-like: menu bar at the top, 2x scale.
        let anchor = Anchor {
            x: 2800.0,
            y: 10.0,
            w: 60.0,
            h: 48.0,
        };
        let monitor = Size::new(1728.0, 1117.0);
        let p = popover_point(anchor, WIN, Some(monitor), 2.0);
        // Centered under the icon: 1400 + 15 - 280.
        assert_eq!(p.x, 1135.0);
        // Below: 5 + 24 + GAP.
        assert_eq!(p.y, 37.0);
    }

    #[test]
    fn popover_rises_above_a_bottom_taskbar() {
        // Windows-like: taskbar at the bottom of a 1080p monitor, 1x scale.
        let anchor = Anchor {
            x: 1700.0,
            y: 1044.0,
            w: 24.0,
            h: 24.0,
        };
        let monitor = Size::new(1920.0, 1080.0);
        let p = popover_point(anchor, WIN, Some(monitor), 1.0);
        // Above the icon: 1044 - 700 - GAP.
        assert_eq!(p.y, 336.0);
        // Centered but clamped off the right edge: 1712 - 280 = 1432 →
        // 1920 - 560 - MARGIN = 1352.
        assert_eq!(p.x, 1352.0);
    }

    #[test]
    fn popover_clamps_inside_the_monitor() {
        // Icon hard in the top-left corner.
        let anchor = Anchor {
            x: 0.0,
            y: 0.0,
            w: 16.0,
            h: 16.0,
        };
        let p = popover_point(anchor, WIN, Some(Size::new(1920.0, 1080.0)), 1.0);
        assert_eq!(p.x, MARGIN);
        assert_eq!(p.y, 16.0 + GAP);
    }

    #[test]
    fn popover_without_monitor_uses_the_top_heuristic_and_floors() {
        // Near the top of the desktop → below the icon, floored to margins.
        let top = Anchor {
            x: 20.0,
            y: 4.0,
            w: 22.0,
            h: 22.0,
        };
        let p = popover_point(top, WIN, None, 1.0);
        assert_eq!(p.x, MARGIN);
        assert_eq!(p.y, 4.0 + 22.0 + GAP);
        // Far down the desktop → above the icon.
        let bottom = Anchor {
            x: 900.0,
            y: 1050.0,
            w: 22.0,
            h: 22.0,
        };
        let p = popover_point(bottom, WIN, None, 1.0);
        assert_eq!(p.y, 1050.0 - 700.0 - GAP);
    }

    #[test]
    fn popover_survives_a_degenerate_scale() {
        let anchor = Anchor {
            x: 100.0,
            y: 10.0,
            w: 20.0,
            h: 20.0,
        };
        let p = popover_point(anchor, WIN, None, 0.0);
        assert!(p.x.is_finite() && p.y.is_finite());
    }

    #[test]
    fn inactive_tray_yields_no_events_or_hint() {
        // Tests never call init(), so the tray is inactive: the poll drain
        // and the fallback hint must both be quiet no-ops.
        assert!(poll_events().is_empty());
        assert_eq!(fallback_hint(), None);
        set_icon(StatusIcon::Receiving); // must not panic
    }
}
