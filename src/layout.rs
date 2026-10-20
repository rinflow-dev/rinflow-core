//! Device arrangement: where peers sit relative to this device.
//!
//! Every device describes its displays in logical points (pixels at 100 %
//! scale, the unit of macOS points) in its own desktop space. A placement
//! puts a peer's desktop into this device's space at an [`Offset`]. Because
//! both sides use the same unit, the peer's view of the same arrangement is
//! simply the offset negated, which is what keeps the two sides in sync.
//!
//! Wherever an edge of one of this device's displays touches an edge of one
//! of the placed peer's displays, the overlapping part is a [`Portal`]: the
//! segment through which the pointer passes, point for point.

use serde::{Deserialize, Serialize};

/// Displays one device may describe.
pub const MAX_DISPLAYS: usize = 16;
/// Shortest shared edge a placement may have, in points.
pub const MIN_OVERLAP: f64 = 64.0;
/// A placement this close to an alignment (ends or centres) snaps to it.
const SNAP_DISTANCE: f64 = 24.0;
/// Edges this close count as touching (rounding of scaled coordinates).
const TOUCH_TOLERANCE: f64 = 0.5;

/// One display in logical points within its device's desktop space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Physical pixels per point, e.g. 2.0 for a Retina or 200 % display.
    #[serde(default = "unit_scale")]
    pub scale: f64,
}

fn unit_scale() -> f64 {
    1.0
}

impl Display {
    /// Assumed for a peer that has not described its displays yet.
    pub const DEFAULT: Display = Display {
        x: 0.0,
        y: 0.0,
        width: 1440.0,
        height: 900.0,
        scale: 1.0,
    };

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    fn translated(&self, offset: Offset) -> Display {
        Display {
            x: self.x + offset.x,
            y: self.y + offset.y,
            ..*self
        }
    }

    fn overlaps(&self, other: &Display) -> bool {
        let tolerance = TOUCH_TOLERANCE;
        self.x < other.right() - tolerance
            && other.x < self.right() - tolerance
            && self.y < other.bottom() - tolerance
            && other.y < self.bottom() - tolerance
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let values = [self.x, self.y, self.width, self.height, self.scale];
        if !values.iter().all(|value| value.is_finite()) {
            return Err("display geometry must be finite");
        }
        if self.width < 1.0 || self.height < 1.0 || self.scale <= 0.0 {
            return Err("display must have a positive size and scale");
        }
        if values.iter().any(|value| value.abs() > 1.0e7) {
            return Err("display geometry is out of range");
        }
        Ok(())
    }
}

pub fn validate_displays(displays: &[Display]) -> Result<(), &'static str> {
    if displays.len() > MAX_DISPLAYS {
        return Err("too many displays");
    }
    displays.iter().try_for_each(Display::validate)
}

/// Translation from a peer's desktop space into this device's space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Offset {
    pub x: f64,
    pub y: f64,
}

impl Offset {
    /// The same arrangement as seen from the peer.
    pub fn mirrored(self) -> Offset {
        Offset {
            x: -self.x,
            y: -self.y,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.x.abs() < 1.0e7 && self.y.abs() < 1.0e7
    }
}

/// A side of a display on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// Where the pointer passes from a local display into a peer display.
///
/// `line` is the shared edge (x for left/right, y for top/bottom) and
/// `from..to` its extent along the edge, all in this device's space. A point
/// `a` along the edge continues at `a - offset` on the peer's side, i.e. the
/// mapping is point for point in logical points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Portal {
    pub side: Side,
    pub local_display: usize,
    pub peer_display: usize,
    pub line: f64,
    pub from: f64,
    pub to: f64,
}

impl Portal {
    /// The same portal as seen from the peer (in the peer's space).
    pub fn mirrored(&self, offset: Offset) -> Portal {
        let (line_shift, along_shift) = if self.side.is_vertical() {
            (offset.x, offset.y)
        } else {
            (offset.y, offset.x)
        };
        Portal {
            side: self.side.opposite(),
            local_display: self.peer_display,
            peer_display: self.local_display,
            line: self.line - line_shift,
            from: self.from - along_shift,
            to: self.to - along_shift,
        }
    }
}

/// Bounding box `(left, top, right, bottom)` of a desktop.
pub fn bounds(displays: &[Display]) -> Option<(f64, f64, f64, f64)> {
    displays.iter().fold(None, |acc, display| {
        let (left, top, right, bottom) =
            acc.unwrap_or((display.x, display.y, display.right(), display.bottom()));
        Some((
            left.min(display.x),
            top.min(display.y),
            right.max(display.right()),
            bottom.max(display.bottom()),
        ))
    })
}

/// The peer's displays, or the default display if it has not described any.
pub fn peer_or_default(displays: &[Display]) -> Vec<Display> {
    if displays.is_empty() {
        vec![Display::DEFAULT]
    } else {
        displays.to_vec()
    }
}

/// Every segment where a local display edge touches a placed peer display.
pub fn portals(local: &[Display], peer: &[Display], offset: Offset) -> Vec<Portal> {
    let near = |a: f64, b: f64| (a - b).abs() <= TOUCH_TOLERANCE;
    let mut found = Vec::new();
    for (local_index, l) in local.iter().enumerate() {
        for (peer_index, p) in peer.iter().enumerate() {
            let p = p.translated(offset);
            let vertical = (l.y.max(p.y), l.bottom().min(p.bottom()));
            let horizontal = (l.x.max(p.x), l.right().min(p.right()));
            let candidates = [
                (near(l.right(), p.x), Side::Right, l.right(), vertical),
                (near(l.x, p.right()), Side::Left, l.x, vertical),
                (near(l.bottom(), p.y), Side::Bottom, l.bottom(), horizontal),
                (near(l.y, p.bottom()), Side::Top, l.y, horizontal),
            ];
            for (touching, side, line, (from, to)) in candidates {
                if touching && to - from >= 1.0 {
                    found.push(Portal {
                        side,
                        local_display: local_index,
                        peer_display: peer_index,
                        line,
                        from,
                        to,
                    });
                }
            }
        }
    }
    found
}

/// Whether a placement is usable: it touches this desktop with at least one
/// portal, and its displays overlap neither this desktop nor `obstacles`
/// (other placed peers, in this device's space).
pub fn is_valid_placement(
    local: &[Display],
    peer: &[Display],
    offset: Offset,
    obstacles: &[Display],
) -> bool {
    let placed: Vec<Display> = peer.iter().map(|p| p.translated(offset)).collect();
    let collides = placed
        .iter()
        .any(|p| local.iter().chain(obstacles).any(|other| p.overlaps(other)));
    !collides && !portals(local, peer, offset).is_empty()
}

/// Moves a dropped placement to the nearest valid spot: touching one of this
/// device's display edges with at least [`MIN_OVERLAP`] of shared edge, snapped
/// to aligned ends or centres when close, and clear of other placed peers.
pub fn snap(
    local: &[Display],
    peer: &[Display],
    proposed: Offset,
    obstacles: &[Display],
) -> Option<Offset> {
    let (peer_left, peer_top, peer_right, peer_bottom) = bounds(peer)?;
    let (peer_width, peer_height) = (peer_right - peer_left, peer_bottom - peer_top);
    // Where the peer's bounding box would be at the proposed offset.
    let (want_left, want_top) = (peer_left + proposed.x, peer_top + proposed.y);

    let mut best: Option<(f64, Offset)> = None;
    for l in local {
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            let (left, top) = match side {
                Side::Right | Side::Left => {
                    let left = if side == Side::Right {
                        l.right()
                    } else {
                        l.x - peer_width
                    };
                    let top = slide(want_top, peer_height, l.y, l.height);
                    (left, top)
                }
                Side::Bottom | Side::Top => {
                    let top = if side == Side::Bottom {
                        l.bottom()
                    } else {
                        l.y - peer_height
                    };
                    let left = slide(want_left, peer_width, l.x, l.width);
                    (left, top)
                }
            };
            let candidate = Offset {
                x: left - peer_left,
                y: top - peer_top,
            };
            if !is_valid_placement(local, peer, candidate, obstacles) {
                continue;
            }
            let distance = (left - want_left).hypot(top - want_top);
            if best.is_none_or(|(best_distance, _)| distance < best_distance) {
                best = Some((distance, candidate));
            }
        }
    }
    best.map(|(_, offset)| offset)
}

/// Positions a span of `length` along an edge starting at `edge_start` with
/// `edge_length`: kept as close to `wanted` as the minimum overlap allows,
/// then snapped to aligned starts, ends or centres within [`SNAP_DISTANCE`].
fn slide(wanted: f64, length: f64, edge_start: f64, edge_length: f64) -> f64 {
    let overlap = MIN_OVERLAP.min(length).min(edge_length);
    let lowest = edge_start - length + overlap;
    let highest = edge_start + edge_length - overlap;
    let position = wanted.clamp(lowest, highest);
    let alignments = [
        edge_start,
        edge_start + edge_length - length,
        edge_start + (edge_length - length) / 2.0,
    ];
    alignments
        .into_iter()
        .filter(|aligned| (aligned - position).abs() <= SNAP_DISTANCE)
        .min_by(|a, b| (a - position).abs().total_cmp(&(b - position).abs()))
        .unwrap_or(position)
}

/// Places the peer beside this device's outermost edge on `side`, centred on
/// the display that forms it.
pub fn place_beside(
    local: &[Display],
    peer: &[Display],
    side: Side,
    obstacles: &[Display],
) -> Option<Offset> {
    let (peer_left, peer_top, peer_right, peer_bottom) = bounds(peer)?;
    let (peer_width, peer_height) = (peer_right - peer_left, peer_bottom - peer_top);
    let outermost = local.iter().max_by(|a, b| {
        let key = |d: &Display| match side {
            Side::Right => d.right(),
            Side::Left => -d.x,
            Side::Bottom => d.bottom(),
            Side::Top => -d.y,
        };
        key(a).total_cmp(&key(b))
    })?;
    let (left, top) = match side {
        Side::Right => (
            outermost.right(),
            outermost.y + (outermost.height - peer_height) / 2.0,
        ),
        Side::Left => (
            outermost.x - peer_width,
            outermost.y + (outermost.height - peer_height) / 2.0,
        ),
        Side::Bottom => (
            outermost.x + (outermost.width - peer_width) / 2.0,
            outermost.bottom(),
        ),
        Side::Top => (
            outermost.x + (outermost.width - peer_width) / 2.0,
            outermost.y - peer_height,
        ),
    };
    snap(
        local,
        peer,
        Offset {
            x: left - peer_left,
            y: top - peer_top,
        },
        obstacles,
    )
}

/// The peer's displays moved into this device's space, e.g. to use as
/// obstacles when placing another peer.
pub fn placed(peer: &[Display], offset: Offset) -> Vec<Display> {
    peer.iter()
        .map(|display| display.translated(offset))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(x: f64, y: f64, width: f64, height: f64) -> Display {
        Display {
            x,
            y,
            width,
            height,
            scale: 2.0,
        }
    }

    const LAPTOP: Display = Display {
        x: 0.0,
        y: 0.0,
        width: 1440.0,
        height: 900.0,
        scale: 2.0,
    };

    #[test]
    fn touching_edges_form_a_portal_that_mirrors_onto_the_peer() {
        let mac = [display(0.0, 0.0, 1512.0, 982.0)];
        let offset = Offset {
            x: 1440.0,
            y: 100.0,
        };
        let found = portals(&[LAPTOP], &mac, offset);
        assert_eq!(
            found,
            vec![Portal {
                side: Side::Right,
                local_display: 0,
                peer_display: 0,
                line: 1440.0,
                from: 100.0,
                to: 900.0,
            }]
        );

        // The Mac sees the PC on its left, along its own y 0..800.
        let back = portals(&mac, &[LAPTOP], offset.mirrored());
        assert_eq!(back, vec![found[0].mirrored(offset)]);
        assert_eq!(
            (back[0].side, back[0].line, back[0].from, back[0].to),
            (Side::Left, 0.0, 0.0, 800.0)
        );
    }

    #[test]
    fn a_drop_snaps_to_the_nearest_edge_and_keeps_a_minimum_overlap() {
        let peer = [display(0.0, 0.0, 1280.0, 800.0)];
        // Dropped a little away from the right edge: pulled onto it.
        let snapped = snap(
            &[LAPTOP],
            &peer,
            Offset {
                x: 1500.0,
                y: 300.0,
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            snapped,
            Offset {
                x: 1440.0,
                y: 300.0
            }
        );
        // Dropped far below and to the right: the bottom edge is nearest, and
        // the peer slides back until it shares the minimum overlap.
        let snapped = snap(
            &[LAPTOP],
            &peer,
            Offset {
                x: 1500.0,
                y: 2000.0,
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            snapped,
            Offset {
                x: 1440.0 - MIN_OVERLAP,
                y: 900.0
            }
        );
        // Dropped on top of the laptop: pushed out to the closest side.
        let snapped = snap(&[LAPTOP], &peer, Offset { x: 1000.0, y: 50.0 }, &[]).unwrap();
        assert!(is_valid_placement(&[LAPTOP], &peer, snapped, &[]));
    }

    #[test]
    fn near_alignments_snap() {
        let peer = [display(0.0, 0.0, 1280.0, 800.0)];
        // 10 points below top alignment.
        let snapped = snap(&[LAPTOP], &peer, Offset { x: 1440.0, y: 10.0 }, &[]).unwrap();
        assert_eq!(snapped.y, 0.0);
        // Near the centre (900 - 800) / 2 = 50.
        let snapped = snap(&[LAPTOP], &peer, Offset { x: 1440.0, y: 40.0 }, &[]).unwrap();
        assert_eq!(snapped.y, 50.0);
        // Far from any alignment: kept where it was dropped.
        let snapped = snap(
            &[LAPTOP],
            &peer,
            Offset {
                x: 1440.0,
                y: -300.0,
            },
            &[],
        )
        .unwrap();
        assert_eq!(snapped.y, -300.0);
    }

    #[test]
    fn inner_edges_between_local_displays_are_not_used() {
        // Two monitors side by side: the edge at x = 1440 is shared.
        let local = [LAPTOP, display(1440.0, 0.0, 1920.0, 1080.0)];
        let peer = [display(0.0, 0.0, 1280.0, 800.0)];
        let snapped = snap(&local, &peer, Offset { x: 1440.0, y: 0.0 }, &[]).unwrap();
        let found = portals(&local, &peer, snapped);
        assert!(!found.is_empty());
        assert!(
            found
                .iter()
                .all(|portal| portal.line != 1440.0 || !portal.side.is_vertical())
        );
    }

    #[test]
    fn placed_peers_do_not_overlap_each_other() {
        let first = [display(0.0, 0.0, 1280.0, 800.0)];
        let second = [display(0.0, 0.0, 1024.0, 768.0)];
        let first_offset = place_beside(&[LAPTOP], &first, Side::Right, &[]).unwrap();
        let obstacles = placed(&first, first_offset);
        let second_offset =
            snap(&[LAPTOP], &second, first_offset, &obstacles).expect("room elsewhere");
        assert!(is_valid_placement(
            &[LAPTOP],
            &second,
            second_offset,
            &obstacles
        ));
    }

    #[test]
    fn place_beside_centres_on_the_outermost_display() {
        let local = [LAPTOP, display(1440.0, -90.0, 1920.0, 1080.0)];
        let peer = [display(0.0, 0.0, 1280.0, 800.0)];
        let right = place_beside(&local, &peer, Side::Right, &[]).unwrap();
        assert_eq!(
            right,
            Offset {
                x: 3360.0,
                y: -90.0 + (1080.0 - 800.0) / 2.0
            }
        );
        let left = place_beside(&local, &peer, Side::Left, &[]).unwrap();
        assert_eq!(left.x, -1280.0);
        let top = place_beside(&local, &peer, Side::Top, &[]).unwrap();
        assert_eq!(top.y, -90.0 - 800.0);
    }

    #[test]
    fn displays_are_validated() {
        assert!(LAPTOP.validate().is_ok());
        assert!(display(0.0, 0.0, 0.0, 10.0).validate().is_err());
        assert!(display(f64::NAN, 0.0, 10.0, 10.0).validate().is_err());
        assert!(validate_displays(&[LAPTOP; MAX_DISPLAYS + 1]).is_err());
    }
}
