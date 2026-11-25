//! Scissor rects: the `VICTORY_WIDE_SCISSOR` diagnostic and the text its
//! log lines show, in plain numbers so it is tested on every platform.
//!
//! Why it exists: a replay shows all visible geometry confined to a
//! 1252-column band of a 2560-wide buffer, its edges on the title's own
//! scissor (742..1994) with viewport 2560x1440. Scarlight passes the
//! title's rects to the host unchanged (`draw::scissors`) and keeps no
//! copy, so the band is the state the title set; the switch below says
//! whether widening that state brings the rest of the frame back (see
//! `xcl-bin/notes/scissor.md`).

/// A scissor rect: left, top, right, bottom (right and bottom exclusive).
pub(crate) type Rect = [i32; 4];

/// A viewport: left, top, width, height.
pub(crate) type Viewport = [f32; 4];

/// `VICTORY_WIDE_SCISSOR=1`: whether scissors are widened to their
/// viewport. A diagnostic only: it makes the picture tell whether a narrow
/// scissor is what hides geometry.
pub(crate) fn wide() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = matches!(
            std::env::var("VICTORY_WIDE_SCISSOR").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );
        if on {
            tracing::warn!(
                "VICTORY_WIDE_SCISSOR: scissors are widened to their viewport (a diagnostic)"
            );
        }
        on
    })
}

/// The viewport rect `i` pairs with: D3D12 pairs rect `i` with viewport `i`,
/// and a title that sets one viewport for several rects gets that one.
pub(crate) fn pair(viewports: &[Viewport], i: usize) -> Option<&Viewport> {
    viewports.get(i).or_else(|| viewports.last())
}

/// `rect` widened horizontally to `viewport`, its vertical bounds kept.
///
/// The viewport's own bounds are taken as D3D12 does: a float top left and
/// size, rounded outwards. A viewport with no width leaves the rect alone.
fn widen(rect: &Rect, viewport: &Viewport) -> Rect {
    let left = viewport[0].floor();
    let right = (viewport[0] + viewport[2]).ceil();
    let left = if left.is_finite() {
        left as i32
    } else {
        rect[0]
    };
    let right = if right.is_finite() {
        right as i32
    } else {
        rect[2]
    };
    if right <= left {
        return *rect;
    }
    [rect[0].min(left), rect[1], rect[2].max(right), rect[3]]
}

/// `rects` widened horizontally to the viewport each pairs with, their
/// vertical bounds kept (whether to widen at all is [`wide`]'s call). `None`
/// when there are no rects or no viewport is known (nothing to widen to).
pub(crate) fn widened(rects: &[Rect], viewports: &[Viewport]) -> Option<Vec<Rect>> {
    if rects.is_empty() || viewports.is_empty() {
        return None;
    }
    Some(
        rects
            .iter()
            .enumerate()
            .map(|(i, r)| pair(viewports, i).map_or(*r, |v| widen(r, v)))
            .collect(),
    )
}

/// Whether widening changes `rect` against `viewport`: its left or right
/// bound lies inside the viewport's. The case the switch acts on and the
/// log names (a rect as wide as its viewport but shifted counts too).
pub(crate) fn widens(rect: &Rect, viewport: &Viewport) -> bool {
    widen(rect, viewport) != *rect
}

/// A rect as a log line shows it.
pub(crate) fn rect_text(r: &Rect) -> String {
    format!("{}..{} x {}..{}", r[0], r[2], r[1], r[3])
}

/// A viewport as a log line shows it.
pub(crate) fn viewport_text(v: &Viewport) -> String {
    format!("{}x{} at {:.0},{:.0}", v[2], v[3], v[0], v[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrow_scissor_widens_to_its_viewport_and_keeps_its_height() {
        // The report's own numbers: scissor 742..1994, viewport 2560x1440.
        let rects = [[742, 0, 1994, 1440]];
        let viewports = [[0.0, 0.0, 2560.0, 1440.0]];
        assert!(widens(&rects[0], &viewports[0]));
        let wide = widened(&rects, &viewports).expect("a rect and a viewport widen");
        assert_eq!(wide, [[0, 0, 2560, 1440]]);
        assert!(!widens(&wide[0], &viewports[0]));
    }

    #[test]
    fn a_rect_already_as_wide_keeps_its_bounds() {
        let rects = [[-8, 12, 2640, 720]];
        let viewports = [[0.0, 0.0, 2560.0, 1440.0]];
        assert!(!widens(&rects[0], &viewports[0]));
        assert_eq!(widened(&rects, &viewports), Some(rects.to_vec()));
    }

    #[test]
    fn a_shifted_rect_of_the_viewports_width_still_widens() {
        // Both are 512 wide, but the rect starts at 100.
        let rect = [100, 0, 612, 100];
        let viewport = [0.0, 0.0, 512.0, 512.0];
        assert!(widens(&rect, &viewport));
        assert_eq!(widened(&[rect], &[viewport]), Some(vec![[0, 0, 612, 100]]));
    }

    #[test]
    fn each_rect_takes_its_own_viewport_and_a_lone_one_is_reused() {
        let rects = [[100, 0, 200, 100], [300, 0, 400, 100]];
        let two = [[0.0, 0.0, 512.0, 512.0], [512.0, 0.0, 512.0, 512.0]];
        assert_eq!(
            widened(&rects, &two),
            // Widening only ever extends a rect: the second one starts at
            // 300, inside its viewport.
            Some(vec![[0, 0, 512, 100], [300, 0, 1024, 100]])
        );
        let one = [[0.0, 0.0, 512.0, 512.0]];
        assert_eq!(
            widened(&rects, &one),
            Some(vec![[0, 0, 512, 100], [0, 0, 512, 100]])
        );
    }

    #[test]
    fn fractions_round_outwards_and_empty_viewports_do_nothing() {
        let rects = [[10, 0, 20, 20]];
        let half_pixel = [[-0.5, 0.0, 100.5, 10.0]];
        assert_eq!(widened(&rects, &half_pixel), Some(vec![[-1, 0, 100, 20]]));
        let empty = [[4.0, 0.0, 0.0, 10.0]];
        assert_eq!(widened(&rects, &empty), Some(rects.to_vec()));
        assert!(!widens(&rects[0], &empty[0]));
        // Nothing to widen to.
        assert_eq!(widened(&rects, &[]), None);
        assert_eq!(widened(&[], &half_pixel), None);
    }

    #[test]
    fn text_names_both_bounds() {
        assert_eq!(rect_text(&[742, 0, 1994, 1440]), "742..1994 x 0..1440");
        assert_eq!(
            viewport_text(&[0.0, 0.0, 2560.0, 1440.0]),
            "2560x1440 at 0,0"
        );
    }
}
