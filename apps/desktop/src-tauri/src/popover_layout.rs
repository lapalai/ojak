pub const WIDTH: f64 = 340.0;
pub const MAX_HEIGHT: f64 = 560.0;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// All rectangles use physical desktop pixels; only the requested height is logical.
pub fn place(tray: Rect, work: Rect, scale: f64, height: f64, macos: bool) -> Placement {
    let margin = (8.0 * scale).ceil().min(work.width / 4.0).min(work.height / 4.0);
    let width = (WIDTH * scale).min(work.width - 2.0 * margin).floor().max(1.0);
    let height = (height.min(MAX_HEIGHT) * scale).min(work.height - 2.0 * margin).floor().max(1.0);
    let right = work.left + work.width;
    let bottom = work.top + work.height;
    let gap = (4.0 * scale).ceil();
    let anchor = if macos { tray.left } else { tray.left + (tray.width - width) / 2.0 };
    let middle = tray.top + tray.height / 2.0;
    let side = !macos && (tray.left >= right - 1.0 || tray.left + tray.width <= work.left + 1.0);
    let (x, y) = if side {
        let x = if tray.left >= right - 1.0 { tray.left - width - gap } else { tray.left + tray.width + gap };
        (x, middle - height / 2.0)
    } else if middle > work.top + work.height / 2.0 {
        (anchor, tray.top - height - gap)
    } else {
        (anchor, tray.top + tray.height + gap)
    };
    Placement {
        x: x.round().clamp(work.left + margin, right - width - margin),
        y: y.round().clamp(work.top + margin, bottom - height - margin),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fits(p: &Placement, work: Rect) {
        assert!(p.x >= work.left && p.y >= work.top);
        assert!(p.x + p.width <= work.left + work.width);
        assert!(p.y + p.height <= work.top + work.height);
    }

    #[test]
    fn bottom_tray_keeps_its_lower_edge_when_content_grows() {
        let work = Rect { left: 0.0, top: 0.0, width: 1920.0, height: 1040.0 };
        let tray = Rect { left: 1800.0, top: 1040.0, width: 24.0, height: 40.0 };
        let collapsed = place(tray, work, 1.0, 180.0, false);
        let expanded = place(tray, work, 1.0, 560.0, false);
        assert_eq!(collapsed.y + collapsed.height, expanded.y + expanded.height);
        assert_eq!(expanded.height, 560.0);
        fits(&collapsed, work);
        fits(&expanded, work);
    }

    #[test]
    fn high_dpi_short_negative_monitor_bounds_both_dimensions() {
        let work = Rect { left: -1280.0, top: -200.0, width: 1280.0, height: 680.0 };
        let tray = Rect { left: -80.0, top: 480.0, width: 40.0, height: 60.0 };
        let placement = place(tray, work, 2.0, 560.0, false);
        assert_eq!(placement.width, 680.0);
        assert_eq!(placement.height, 648.0);
        fits(&placement, work);
        let narrow = Rect { width: 500.0, ..work };
        let placement = place(tray, narrow, 2.0, 560.0, false);
        assert_eq!(placement.width, 468.0);
        fits(&placement, narrow);
    }

    #[test]
    fn top_and_vertical_taskbars_and_mac_menu_bar_stay_in_the_work_area() {
        for tray in [
            Rect { left: 900.0, top: 0.0, width: 24.0, height: 40.0 },
            Rect { left: 0.0, top: 500.0, width: 40.0, height: 24.0 },
            Rect { left: 1880.0, top: 500.0, width: 40.0, height: 24.0 },
        ] {
            let work = Rect { left: 40.0, top: 40.0, width: 1840.0, height: 1000.0 };
            fits(&place(tray, work, 1.25, 560.0, false), work);
        }
        let work = Rect { left: -1920.0, top: 24.0, width: 1920.0, height: 1056.0 };
        let tray = Rect { left: -1700.0, top: 0.0, width: 24.0, height: 24.0 };
        let placement = place(tray, work, 1.0, 460.0, true);
        assert_eq!(placement.x, tray.left);
        assert!(placement.y >= work.top);
        fits(&placement, work);
    }
}
