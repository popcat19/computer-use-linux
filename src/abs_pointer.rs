//! Direct uinput **absolute** pointer.
//!
//! ydotool's virtual device is relative-only (`EV=7`: SYN|KEY|REL), so its
//! `--absolute` is faked as "pin-to-corner + relative move", which the
//! compositor then distorts with pointer acceleration and fractional display
//! scaling — clicks land in the wrong place on multi-monitor / HiDPI setups.
//!
//! Here we create our own uinput device that exposes a true `ABS_X`/`ABS_Y`
//! axis whose range equals the **logical desktop size** (the same coordinate
//! space the portal screenshot reports). The compositor maps an absolute
//! device's axis range across the whole logical layout, so `ABS(x, y)` lands at
//! screenshot pixel `(x, y)` regardless of scaling — and with no approval
//! dialog (we already hold `/dev/uinput` access).

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode,
    PropType, UinputAbsSetup,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "pointer input may be clamped; inspect requested and emitted coordinates"]
pub(crate) struct PointerLanding {
    pub(crate) requested: (i32, i32),
    pub(crate) emitted: (i32, i32),
}

#[derive(Clone, Copy)]
struct AbsPointerGeometry {
    max_x: i32,
    max_y: i32,
}

impl AbsPointerGeometry {
    fn from_dimensions(width: i32, height: i32) -> Self {
        Self {
            max_x: width.max(1).saturating_sub(1),
            max_y: height.max(1).saturating_sub(1),
        }
    }

    fn axis_maxima(self) -> (i32, i32) {
        (self.max_x, self.max_y)
    }

    fn clamp_coordinates(self, x: i32, y: i32) -> (i32, i32) {
        (x.clamp(0, self.max_x), y.clamp(0, self.max_y))
    }

    fn landing_for(self, x: i32, y: i32) -> PointerLanding {
        PointerLanding {
            requested: (x, y),
            emitted: self.clamp_coordinates(x, y),
        }
    }
}

pub struct AbsPointer {
    device: VirtualDevice,
    geometry: AbsPointerGeometry,
}

impl AbsPointer {
    /// Create the absolute pointer sized to the logical desktop `width`×`height`
    /// (the portal screenshot dimensions). Blocks ~`settle` ms so libinput picks
    /// the device up before the first event.
    pub fn create(width: i32, height: i32) -> Result<Self> {
        let geometry = AbsPointerGeometry::from_dimensions(width, height);
        let (max_x, max_y) = geometry.axis_maxima();
        // value, min, max, fuzz, flat, resolution. resolution=1 unit/px.
        let abs_x =
            UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, AbsInfo::new(0, 0, max_x, 0, 0, 1));
        let abs_y =
            UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, 0, max_y, 0, 0, 1));
        let keys =
            AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);
        // INPUT_PROP_DIRECT marks the device as a direct (absolute) pointer so
        // libinput maps its axes to screen coordinates rather than treating it
        // as a relative touchpad.
        let props = AttributeSet::from_iter([PropType::DIRECT]);

        let device = VirtualDevice::builder()
            .context("uinput builder (is /dev/uinput writable?)")?
            .name("computer-use-linux absolute pointer")
            .with_properties(&props)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_keys(&keys)?
            .build()
            .context("failed to create uinput absolute pointer device")?;

        // Give udev/libinput time to enumerate the new device.
        sleep(Duration::from_millis(500));

        Ok(Self { device, geometry })
    }

    /// Move the pointer to absolute logical coordinates `(x, y)` and report
    /// both the requested point and the values emitted after edge clamping.
    pub fn move_to(&mut self, x: i32, y: i32) -> Result<PointerLanding> {
        let landing = self.geometry.landing_for(x, y);
        let (emitted_x, emitted_y) = landing.emitted;
        self.device
            .emit(&[
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, emitted_x),
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, emitted_y),
            ])
            .context("failed to emit absolute motion")?;
        Ok(landing)
    }

    /// Move to `(x, y)` then press+release `button` `count` times.
    pub fn click(
        &mut self,
        x: i32,
        y: i32,
        button: PointerButton,
        count: u32,
    ) -> Result<PointerLanding> {
        let landing = self.move_to(x, y)?;
        sleep(Duration::from_millis(30));
        let code = button.key_code();
        for _ in 0..count.max(1) {
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])?;
            sleep(Duration::from_millis(30));
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])?;
            sleep(Duration::from_millis(40));
        }
        Ok(landing)
    }

    /// Press at `(start)`, move to `(end)`, release — a drag with `button`.
    pub fn drag(
        &mut self,
        start: (i32, i32),
        end: (i32, i32),
        button: PointerButton,
    ) -> Result<()> {
        let start = self.geometry.clamp_coordinates(start.0, start.1);
        let end = self.geometry.clamp_coordinates(end.0, end.1);
        run_drag(start, end, button, |action| match action {
            DragAction::Move(x, y) => self.move_to(x, y).map(|_| ()),
            DragAction::Button(code, value) => self
                .device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, value)])
                .context("failed to emit drag button"),
            DragAction::Wait(duration) => {
                sleep(duration);
                Ok(())
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragAction {
    Move(i32, i32),
    Button(u16, i32),
    Wait(Duration),
}

fn run_drag(
    start: (i32, i32),
    end: (i32, i32),
    button: PointerButton,
    mut perform: impl FnMut(DragAction) -> Result<()>,
) -> Result<()> {
    // This pacing and interpolation together worked on Hyprland/Excalidraw;
    // the diagnostic did not isolate timing from intermediate motion.
    const SETTLE: Duration = Duration::from_millis(150);
    const STEP_DELAY: Duration = Duration::from_millis(25);
    const STEPS: i64 = 20;

    perform(DragAction::Move(start.0, start.1))?;
    perform(DragAction::Wait(SETTLE))?;
    let code = button.key_code();
    perform(DragAction::Button(code, 1))?;
    let motion = (|| {
        perform(DragAction::Wait(SETTLE))?;
        for step in 1..=STEPS {
            let interpolate = |from: i32, to: i32| {
                (i64::from(from) + (i64::from(to) - i64::from(from)) * step / STEPS) as i32
            };
            perform(DragAction::Move(
                interpolate(start.0, end.0),
                interpolate(start.1, end.1),
            ))?;
            perform(DragAction::Wait(STEP_DELAY))?;
        }
        perform(DragAction::Wait(SETTLE))
    })();
    // A failed motion must still attempt release so the button is not left held.
    let release = perform(DragAction::Button(code, 0));
    motion.and(release)
}

/// Pointer buttons we can synthesize.
#[derive(Clone, Copy, Debug)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

impl PointerButton {
    pub fn from_name(name: Option<&str>) -> Option<Self> {
        match name.unwrap_or("left").to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" => Some(Self::Middle),
            _ => None,
        }
    }

    fn key_code(self) -> u16 {
        match self {
            Self::Left => KeyCode::BTN_LEFT.0,
            Self::Right => KeyCode::BTN_RIGHT.0,
            Self::Middle => KeyCode::BTN_MIDDLE.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_drag, AbsPointerGeometry, DragAction, PointerButton};
    use std::time::Duration;

    #[test]
    fn drag_paces_motion_between_press_and_release_for_every_button() {
        for button in [
            PointerButton::Left,
            PointerButton::Right,
            PointerButton::Middle,
        ] {
            let mut actions = Vec::new();
            run_drag((100, 200), (500, 600), button, |action| {
                actions.push(action);
                Ok(())
            })
            .unwrap();
            let mut expected = vec![
                DragAction::Move(100, 200),
                DragAction::Wait(Duration::from_millis(150)),
                DragAction::Button(button.key_code(), 1),
                DragAction::Wait(Duration::from_millis(150)),
            ];
            for step in 1..=20 {
                expected.push(DragAction::Move(100 + 20 * step, 200 + 20 * step));
                expected.push(DragAction::Wait(Duration::from_millis(25)));
            }
            expected.push(DragAction::Wait(Duration::from_millis(150)));
            expected.push(DragAction::Button(button.key_code(), 0));
            assert_eq!(actions, expected);
        }
    }

    #[test]
    fn drag_interpolation_handles_reverse_short_stationary_and_extreme_paths() {
        for (start, end) in [
            ((500, 600), (100, 200)),
            ((0, 7), (1, 6)),
            ((4, 4), (4, 4)),
            ((i32::MIN, i32::MAX), (i32::MAX, i32::MIN)),
        ] {
            let mut points = Vec::new();
            run_drag(start, end, PointerButton::Left, |action| {
                if let DragAction::Move(x, y) = action {
                    points.push((x, y));
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(points.len(), 21);
            assert_eq!(points[0], start);
            assert_eq!(*points.last().unwrap(), end);
            for pair in points.windows(2) {
                for (from, to, a, b) in [
                    (start.0, end.0, pair[0].0, pair[1].0),
                    (start.1, end.1, pair[0].1, pair[1].1),
                ] {
                    assert!((from.min(to)..=from.max(to)).contains(&b));
                    assert!(if from <= to { a <= b } else { a >= b });
                }
            }
        }
    }

    #[test]
    fn drag_releases_after_each_possible_motion_failure() {
        for failed_move in 1..=20 {
            let mut moves = 0;
            let mut last = None;
            let error = run_drag((0, 0), (100, 100), PointerButton::Right, |action| {
                last = Some(action);
                if matches!(action, DragAction::Move(..)) {
                    moves += 1;
                    if moves == failed_move + 1 {
                        anyhow::bail!("motion failed");
                    }
                }
                Ok(())
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "motion failed");
            assert_eq!(
                last,
                Some(DragAction::Button(PointerButton::Right.key_code(), 0))
            );
        }
    }

    #[test]
    fn drag_propagates_release_failure_and_stops_before_press_on_start_failure() {
        let error = run_drag((0, 0), (100, 100), PointerButton::Left, |action| {
            if matches!(action, DragAction::Button(_, 0)) {
                anyhow::bail!("release failed");
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "release failed");
        let mut count = 0;
        assert!(run_drag((0, 0), (100, 100), PointerButton::Left, |_| {
            count += 1;
            anyhow::bail!("start failed")
        })
        .is_err());
        assert_eq!(count, 1);
    }

    #[test]
    fn axis_range_ends_at_last_desktop_pixel() {
        let geometry = AbsPointerGeometry::from_dimensions(1920, 1080);

        assert_eq!(geometry.axis_maxima(), (1919, 1079));
    }

    #[test]
    fn pointer_landing_preserves_the_request_and_emitted_coordinates() {
        let geometry = AbsPointerGeometry::from_dimensions(1920, 1080);

        for (requested, emitted) in [
            ((640, 480), (640, 480)),
            ((1920, 1080), (1919, 1079)),
            ((-1, -1), (0, 0)),
            ((i32::MAX, i32::MAX), (1919, 1079)),
        ] {
            let landing = geometry.landing_for(requested.0, requested.1);
            assert_eq!(landing.requested, requested);
            assert_eq!(landing.emitted, emitted);
        }
    }

    #[test]
    fn unsupported_buttons_fall_through_to_other_backends() {
        assert!(matches!(
            PointerButton::from_name(None),
            Some(PointerButton::Left)
        ));
        assert!(matches!(
            PointerButton::from_name(Some("right")),
            Some(PointerButton::Right)
        ));
        assert!(matches!(
            PointerButton::from_name(Some("middle")),
            Some(PointerButton::Middle)
        ));

        for button in ["side", "extra", "forward", "back"] {
            assert!(
                PointerButton::from_name(Some(button)).is_none(),
                "{button} must fall through instead of becoming a left click"
            );
        }
    }
}
