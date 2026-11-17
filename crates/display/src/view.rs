//! Where the guest's screen sits inside the window, and what a point in the
//! window means to the guest.

/// The part of the window the guest's screen is drawn in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Places a `screen_width` x `screen_height` picture in a window area,
/// as large as fits without changing its shape, centred.
pub fn fit(screen_width: u32, screen_height: u32, area_width: f64, area_height: f64) -> Option<Placement> {
    if screen_width == 0 || screen_height == 0 || area_width <= 0.0 || area_height <= 0.0 {
        return None;
    }
    let scale = (area_width / f64::from(screen_width)).min(area_height / f64::from(screen_height));
    let (width, height) = (f64::from(screen_width) * scale, f64::from(screen_height) * scale);
    Some(Placement { x: (area_width - width) / 2.0, y: (area_height - height) / 2.0, width, height })
}

/// The guest pixel under a point of the window area. Points on the border
/// around the picture map to the nearest edge, so a pointer leaving the
/// picture ends up at the guest's screen edge instead of stopping short.
pub fn guest_position(placement: Placement, screen_width: u32, screen_height: u32, x: f64, y: f64) -> (u32, u32) {
    let along = |position: f64, start: f64, length: f64, pixels: u32| -> u32 {
        let fraction = ((position - start) / length).clamp(0.0, 1.0);
        ((fraction * f64::from(pixels)) as u32).min(pixels.saturating_sub(1))
    };
    (along(x, placement.x, placement.width, screen_width), along(y, placement.y, placement.height, screen_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_keeps_its_shape_and_is_centred() {
        // A 4:3 screen in a wide window: full height, bars left and right.
        assert_eq!(fit(800, 600, 1600.0, 600.0), Some(Placement { x: 400.0, y: 0.0, width: 800.0, height: 600.0 }));
        // In a tall window: full width, bars above and below; scaled down by half.
        assert_eq!(fit(800, 600, 400.0, 1000.0), Some(Placement { x: 0.0, y: 350.0, width: 400.0, height: 300.0 }));
        assert_eq!(fit(0, 600, 400.0, 300.0), None);
        assert_eq!(fit(800, 600, 0.0, 300.0), None);
    }

    #[test]
    fn window_points_map_to_guest_pixels() {
        let placement = fit(800, 600, 400.0, 1000.0).unwrap();
        assert_eq!(guest_position(placement, 800, 600, 0.0, 350.0), (0, 0));
        assert_eq!(guest_position(placement, 800, 600, 200.0, 500.0), (400, 300));
        // The far corner is the last pixel, never one past it.
        assert_eq!(guest_position(placement, 800, 600, 400.0, 650.0), (799, 599));
        // Outside the picture: clamped to the edge.
        assert_eq!(guest_position(placement, 800, 600, -50.0, 0.0), (0, 0));
        assert_eq!(guest_position(placement, 800, 600, 900.0, 999.0), (799, 599));
    }
}
