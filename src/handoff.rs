//! Screen geometry and deterministic cursor mapping, independent of OS hooks.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub fn right(self) -> f64 {
        self.x + self.width
    }
    pub fn bottom(self) -> f64 {
        self.y + self.height
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub start: f64,
    pub end: f64,
    pub y: f64,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub monitor: Rect,
}
impl Edge {
    #[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
    pub fn normalized(self, x: f64) -> f64 {
        ((x - self.start) / (self.end - self.start - 1.0).max(1.0)).clamp(0.0, 1.0)
    }
    pub fn landing(self, normalized: f64) -> (f64, f64) {
        (
            self.start + normalized.clamp(0.0, 1.0) * (self.end - self.start - 1.0).max(0.0),
            self.y - 4.0,
        )
    }
}
/// Use an exposed bottom edge of the real monitor under the pointer. Subtract
/// Windows monitors below it so an internal Windows seam never triggers a handoff.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub fn exit_edge(monitors: &[Rect], percent: f32, x: f64, y: f64, dy: f64) -> Option<Edge> {
    if dy <= 0.0 || !x.is_finite() || !y.is_finite() {
        return None;
    }
    let left = monitors.iter().map(|r| r.x).reduce(f64::min)?;
    let right = monitors.iter().map(|r| r.right()).reduce(f64::max)?;
    let threshold = left + (right - left) * f64::from(percent) / 100.0;
    for monitor in monitors {
        if x < monitor.x
            || x >= monitor.right()
            || y < monitor.bottom() - 2.0
            || y >= monitor.bottom()
        {
            continue;
        }
        let mut segments = vec![(monitor.x.max(threshold), monitor.right())];
        for other in monitors {
            if other.y <= monitor.bottom() && other.bottom() > monitor.bottom() {
                segments = segments
                    .into_iter()
                    .flat_map(|(a, b)| {
                        let mut out = Vec::new();
                        if other.x > a {
                            out.push((a, b.min(other.x)));
                        }
                        if other.right() < b {
                            out.push((a.max(other.right()), b));
                        }
                        out
                    })
                    .collect();
            }
        }
        for (start, end) in segments {
            if end - start >= 2.0 && x >= start && x < end {
                return Some(Edge {
                    start,
                    end,
                    y: monitor.bottom(),
                    monitor: *monitor,
                });
            }
        }
    }
    None
}
/// A fast proposed move can overshoot the final screen row entirely.
/// Restrict the search to the screen containing the previous position, then
/// evaluate its boundary rather than requiring the proposed point to land on it.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub fn crossed_exit_edge(
    monitors: &[Rect],
    percent: f32,
    x: f64,
    y: f64,
    previous_x: f64,
    previous_y: f64,
) -> Option<Edge> {
    let dy = y - previous_y;
    if dy <= 0.0 {
        return None;
    }
    for monitor in monitors {
        if previous_x >= monitor.x
            && previous_x < monitor.right()
            && previous_y >= monitor.y
            && previous_y < monitor.bottom()
            && y >= monitor.bottom() - 2.0
        {
            let crossing_x = if y >= monitor.bottom() {
                let t = ((monitor.bottom() - 1.0 - previous_y) / dy).clamp(0.0, 1.0);
                previous_x + (x - previous_x) * t
            } else {
                x
            };
            if let Some(edge) = exit_edge(monitors, percent, crossing_x, monitor.bottom() - 1.0, dy)
            {
                return Some(edge);
            }
        }
    }
    None
}

#[derive(Debug)]
pub struct RemotePointer {
    pub bounds: Rect,
    pub x: f64,
    pub y: f64,
}
impl RemotePointer {
    pub fn enter(bounds: Rect, normalized: f64) -> Self {
        Self {
            bounds,
            x: bounds.x + normalized * (bounds.width - 1.0),
            y: bounds.y + 3.0,
        }
    }
    /// Returns a normalized Windows landing position when crossing the Mac's top.
    pub fn motion(&mut self, dx: f64, dy: f64) -> Option<f64> {
        self.x = (self.x + dx).clamp(self.bounds.x, self.bounds.right() - 1.0);
        let next_y = self.y + dy;
        if dy < 0.0 && next_y < self.bounds.y {
            return Some((self.x - self.bounds.x) / (self.bounds.width - 1.0).max(1.0));
        }
        self.y = next_y.clamp(self.bounds.y, self.bounds.bottom() - 1.0);
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const LAPTOP: Rect = Rect {
        x: 0.,
        y: 300.,
        width: 1920.,
        height: 1080.,
    };
    const UPPER: Rect = Rect {
        x: 1920.,
        y: 0.,
        width: 2560.,
        height: 1440.,
    };
    #[test]
    fn l_desk_uses_monitor_bottom_and_segment() {
        let screens = [LAPTOP, UPPER];
        assert!(exit_edge(&screens, 42.9, 1000., 1379., 5.).is_none());
        assert!(exit_edge(&screens, 42.9, 3000., 1439., -5.).is_none());
        assert!(exit_edge(&screens, 42.9, 3000., 1000., 5.).is_none());
        let e = exit_edge(&screens, 42.9, 3000., 1439., 5.).unwrap();
        let (x, y) = e.landing(e.normalized(3000.));
        assert!((x - 3000.).abs() < 0.001);
        assert_eq!(y, 1436.);
    }
    #[test]
    fn internal_seams_are_excluded() {
        let top = Rect {
            x: -1600.,
            y: -900.,
            width: 1600.,
            height: 900.,
        };
        let below = Rect {
            x: -800.,
            y: 0.,
            width: 800.,
            height: 600.,
        };
        assert!(exit_edge(&[top, below], 0., -400., -1., 10.).is_none());
        let e = exit_edge(&[top, below], 0., -1200., -1., 10.).unwrap();
        assert_eq!(e.end, -800.);
    }
    #[test]
    fn fast_motion_crosses_bottom_without_landing_on_last_row() {
        let screens = [LAPTOP, UPPER];
        assert!(crossed_exit_edge(&screens, 42.9, 3000., 1500., 3000., 1300.).is_some());
        assert!(crossed_exit_edge(&screens, 42.9, 3000., 1439., 3000., 1400.).is_some());
        assert!(crossed_exit_edge(&screens, 42.9, 1000., 1500., 1000., 1300.).is_none());
        assert!(crossed_exit_edge(&screens, 42.9, 3000., 1300., 3000., 1500.).is_none());
        let top = Rect {
            x: 0.,
            y: 0.,
            width: 1000.,
            height: 1000.,
        };
        let below = Rect {
            x: 0.,
            y: 1000.,
            width: 1000.,
            height: 1000.,
        };
        assert!(crossed_exit_edge(&[top, below], 0., 500., 1100., 500., 900.).is_none());
    }
    #[test]
    fn mac_maps_entry_clamps_and_returns_at_top() {
        let bounds = Rect {
            x: 0.,
            y: 0.,
            width: 1440.,
            height: 900.,
        };
        let mut p = RemotePointer::enter(bounds, 0.5);
        assert_eq!(p.x, 719.5);
        assert!(p.motion(10000., 10000.).is_none());
        assert_eq!((p.x, p.y), (1439., 899.));
        assert_eq!(p.motion(0., -1000.), Some(1.));
    }
}
