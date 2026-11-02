// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
/// The smallest display side accepted, in points.
const MIN_DISPLAY_SIZE: f64 = 16.0;
/// A placement this close to an alignment (ends or centres) snaps to it.
const SNAP_DISTANCE: f64 = 24.0;
/// Among spots about as near to a drop, the one sharing more edge wins: each
/// point of shared edge counts as this much less distance. A rough drop then
/// lands where the desktops fit snugly, not beside a small gap.
const SNUG_BONUS: f64 = 0.05;
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
        // No real display is smaller; tinier ones only invite rounding trouble.
        if self.width < MIN_DISPLAY_SIZE || self.height < MIN_DISPLAY_SIZE || self.scale <= 0.0 {
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

/// Moves a dropped placement to the nearest valid spot: one of the peer's
/// displays against an edge of one of this device's displays, sharing at
/// least [`MIN_OVERLAP`] of it, snapped to aligned ends or centres when
/// close, and clear of this desktop and of other placed peers.
///
/// Each peer display is tried against each local edge, not the peer's
/// bounding box: a peer with displays of different sizes (an L shape) has
/// edges inside its bounding box, and those are the ones that touch. A spot
/// where another of its displays would collide is also tried moved along the
/// edge just clear of what it hits.
pub fn snap(
    local: &[Display],
    peer: &[Display],
    proposed: Offset,
    obstacles: &[Display],
) -> Option<Offset> {
    let blockers: Vec<Display> = local.iter().chain(obstacles).copied().collect();
    snap_against(
        local,
        &blockers,
        peer,
        proposed,
        |candidate| is_valid_placement(local, peer, candidate, obstacles),
        |candidate| shared_edge(local, peer, candidate),
    )
}

/// How much edge the peer at `offset` shares with `with`, in points.
fn shared_edge(with: &[Display], peer: &[Display], offset: Offset) -> f64 {
    portals(with, peer, offset)
        .iter()
        .map(|portal| portal.to - portal.from)
        .sum()
}

// ---- Chained arrangements ----------------------------------------------
//
// A peer may also sit against another placed peer instead of this device,
// so three devices can stand in a row whichever one has the keyboard: the
// pointer then crosses from one peer straight into the next (see
// `RemotePointer::step`). Every placed peer must still connect to this
// device, directly or through others, so nothing floats on its own.

/// Which of the other placed peers (each as its displays in this device's
/// space) connect to this device, touching it or a connected peer.
pub fn connected(local: &[Display], others: &[Vec<Display>]) -> Vec<bool> {
    let touches =
        |a: &[Display], b: &[Display]| !portals(a, b, Offset { x: 0.0, y: 0.0 }).is_empty();
    let mut reached: Vec<bool> = others.iter().map(|other| touches(local, other)).collect();
    loop {
        let mut grew = false;
        for index in 0..others.len() {
            if reached[index] {
                continue;
            }
            if (0..others.len()).any(|from| reached[from] && touches(&others[from], &others[index]))
            {
                reached[index] = true;
                grew = true;
            }
        }
        if !grew {
            return reached;
        }
    }
}

/// The displays a peer may sit against: this device's and those of the
/// other placed peers that connect to it.
fn anchors(local: &[Display], others: &[Vec<Display>]) -> Vec<Display> {
    let reached = connected(local, others);
    local
        .iter()
        .copied()
        .chain(
            others
                .iter()
                .zip(reached)
                .filter(|(_, reached)| *reached)
                .flat_map(|(other, _)| other.iter().copied()),
        )
        .collect()
}

/// Whether a placement is usable among the other placed peers (`others`,
/// in this device's space): it overlaps nothing, and touches this desktop
/// or a peer connected to it.
pub fn is_valid_in_arrangement(
    local: &[Display],
    peer: &[Display],
    offset: Offset,
    others: &[Vec<Display>],
) -> bool {
    let placed: Vec<Display> = peer.iter().map(|p| p.translated(offset)).collect();
    let collides = placed.iter().any(|p| {
        local
            .iter()
            .chain(others.iter().flatten())
            .any(|other| p.overlaps(other))
    });
    !collides && !portals(&anchors(local, others), peer, offset).is_empty()
}

/// [`snap`] among the other placed peers: the peer may also be put against
/// a peer that connects to this device.
pub fn snap_in_arrangement(
    local: &[Display],
    peer: &[Display],
    proposed: Offset,
    others: &[Vec<Display>],
) -> Option<Offset> {
    let anchors = anchors(local, others);
    let blockers: Vec<Display> = local
        .iter()
        .chain(others.iter().flatten())
        .copied()
        .collect();
    snap_against(
        &anchors,
        &blockers,
        peer,
        proposed,
        |candidate| is_valid_in_arrangement(local, peer, candidate, others),
        |candidate| shared_edge(&anchors, peer, candidate),
    )
}

/// The spot nearest to `proposed` where one of the peer's displays sits
/// against an edge of one of `edges` and `valid` holds, preferring spots
/// that share more edge. A spot where something of `blockers` is in the way
/// is also tried moved along the edge just clear of it.
fn snap_against(
    edges: &[Display],
    blockers: &[Display],
    peer: &[Display],
    proposed: Offset,
    valid: impl Fn(Offset) -> bool,
    shared: impl Fn(Offset) -> f64,
) -> Option<Offset> {
    let local = edges;
    let mut best: Option<(f64, Offset)> = None;
    let mut consider = |candidate: Offset| {
        if !valid(candidate) {
            return;
        }
        let score = (candidate.x - proposed.x).hypot(candidate.y - proposed.y)
            - SNUG_BONUS * shared(candidate);
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, candidate));
        }
    };
    for l in local {
        for p in peer {
            // Where this display would be at the proposed offset.
            let (want_x, want_y) = (p.x + proposed.x, p.y + proposed.y);
            for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
                // `across` puts the display against the edge; `along` is its
                // position along the edge, which may slide.
                let (across, along, length, edge_start, edge_length) = match side {
                    Side::Right => (l.right(), want_y, p.height, l.y, l.height),
                    Side::Left => (l.x - p.width, want_y, p.height, l.y, l.height),
                    Side::Bottom => (l.bottom(), want_x, p.width, l.x, l.width),
                    Side::Top => (l.y - p.height, want_x, p.width, l.x, l.width),
                };
                let offset_at = |along: f64| {
                    if side.is_vertical() {
                        Offset {
                            x: across - p.x,
                            y: along - p.y,
                        }
                    } else {
                        Offset {
                            x: along - p.x,
                            y: across - p.y,
                        }
                    }
                };
                let slid = slide(along, length, edge_start, edge_length);
                let candidate = offset_at(slid);
                consider(candidate);

                // Something in the way: just clear of it on either side, as
                // long as the display still shares the edge.
                let overlap = MIN_OVERLAP.min(length).min(edge_length);
                let (lowest, highest) = (
                    edge_start - length + overlap,
                    edge_start + edge_length - overlap,
                );
                for placed in placed(peer, candidate) {
                    for other in blockers.iter().filter(|other| placed.overlaps(other)) {
                        let shifts = if side.is_vertical() {
                            [other.y - placed.bottom(), other.bottom() - placed.y]
                        } else {
                            [other.x - placed.right(), other.right() - placed.x]
                        };
                        for shift in shifts {
                            let moved = slid + shift;
                            if (lowest..=highest).contains(&moved) {
                                consider(offset_at(moved));
                            }
                        }
                    }
                }
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
    let position = clamp_between(wanted, lowest, highest);
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
/// the display that forms it, clear of the other placed peers.
pub fn place_beside(
    local: &[Display],
    peer: &[Display],
    side: Side,
    obstacles: &[Display],
) -> Option<Offset> {
    let (left, top) = beside(local, peer, side)?;
    snap(local, peer, Offset { x: left, y: top }, obstacles)
}

/// [`place_beside`] among the other placed peers (see
/// [`snap_in_arrangement`]).
pub fn place_beside_in_arrangement(
    local: &[Display],
    peer: &[Display],
    side: Side,
    others: &[Vec<Display>],
) -> Option<Offset> {
    let (left, top) = beside(local, peer, side)?;
    snap_in_arrangement(local, peer, Offset { x: left, y: top }, others)
}

/// The offset that puts the peer centred beside this device's outermost
/// edge on `side`.
fn beside(local: &[Display], peer: &[Display], side: Side) -> Option<(f64, f64)> {
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
    Some((left - peer_left, top - peer_top))
}

/// The peer's displays moved into this device's space, e.g. to use as
/// obstacles when placing another peer.
pub fn placed(peer: &[Display], offset: Offset) -> Vec<Display> {
    peer.iter()
        .map(|display| display.translated(offset))
        .collect()
}

/// Distance, in points, between an edge and where the pointer lands after
/// crossing it, so a small move back does not bounce straight across again.
pub const CROSSING_MARGIN: f64 = 2.0;
/// How far inside its edge the pointer reappears when control is given back
/// without crossing (release key, peer gone).
const RETURN_INSET: f64 = 8.0;

/// The pointer while it drives a peer, tracked in the peer's space.
///
/// The controlling device keeps its own cursor parked meanwhile, so this is
/// the only position there is. Motion moves it within the peer's displays,
/// sliding along edges that lead nowhere as the peer's own cursor does, and
/// moving it out through a portal ends the crossing at the matching local
/// point. The peer plays the same motion, so both stay in step.
#[derive(Clone, Debug)]
pub struct RemotePointer {
    peer: Vec<Display>,
    /// Every portal to this peer, in this device's space: the pointer may
    /// come back through any of them.
    portals: Vec<Portal>,
    offset: Offset,
    /// Where the pointer left this device, for giving control back without
    /// crossing; none when it came by shortcut.
    entry: Option<Portal>,
    /// Other placed peers it may move on into (see [`RemotePointer::step`]).
    neighbors: Vec<Neighbor>,
    x: f64,
    y: f64,
}

/// Another placed peer, by its displays in this device's space.
#[derive(Clone, Debug)]
pub struct Neighbor {
    pub id: String,
    pub displays: Vec<Display>,
}

/// Where a move took the pointer (see [`RemotePointer::step`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Motion {
    /// Still on the peer.
    Stay,
    /// Back on this device, continuing at this local point: the crossing
    /// is over.
    Home(f64, f64),
    /// Into another placed peer that touches this one, at this point in
    /// this device's space. The pointer itself stopped at the edge, so a
    /// caller that does not move on simply has an edge there.
    Neighbor { id: String, x: f64, y: f64 },
}

impl RemotePointer {
    /// Arrives on the peer at the local point `(x, y)`: from another peer
    /// (`entry` is then the portal the pointer first left this device
    /// through), or by shortcut (no entry). The point is moved into the
    /// nearest of the peer's displays.
    pub fn enter_at(
        peer: &[Display],
        portals: &[Portal],
        offset: Offset,
        x: f64,
        y: f64,
        entry: Option<Portal>,
    ) -> Self {
        let peer = peer_or_default(peer);
        let (px, py) = (x - offset.x, y - offset.y);
        let distance = |display: &Display| {
            let (cx, cy) = clamp_into(display, px, py);
            (cx - px).hypot(cy - py)
        };
        let display = peer
            .iter()
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .copied()
            .unwrap_or(peer[0]);
        let (px, py) = clamp_into(&display, px, py);
        Self {
            peer,
            portals: portals.to_vec(),
            offset,
            entry,
            neighbors: Vec::new(),
            x: px,
            y: py,
        }
    }

    /// Lets the pointer move on into these peers where they touch this one.
    pub fn with_neighbors(mut self, neighbors: Vec<Neighbor>) -> Self {
        self.neighbors = neighbors;
        self
    }

    /// The portal the pointer first left this device through, if it crossed.
    pub fn entry(&self) -> Option<Portal> {
        self.entry
    }

    /// Where the pointer is, in this device's space.
    pub fn local_position(&self) -> (f64, f64) {
        (self.x + self.offset.x, self.y + self.offset.y)
    }

    /// Moves by `(dx, dy)` points: within the peer, back to this device
    /// through a portal, or on into a neighbouring peer.
    pub fn step(&mut self, dx: f64, dy: f64) -> Motion {
        let (x, y) = (self.x + dx, self.y + dy);
        if self.peer.iter().any(|display| contains(display, x, y)) {
            (self.x, self.y) = (x, y);
            return Motion::Stay;
        }
        let (local_x, local_y) = (x + self.offset.x, y + self.offset.y);
        if let Some((home_x, home_y)) = self
            .portals
            .iter()
            .find_map(|portal| exit_through(portal, local_x, local_y))
        {
            return Motion::Home(home_x, home_y);
        }
        let neighbor = self
            .neighbors
            .iter()
            .find(|neighbor| {
                neighbor
                    .displays
                    .iter()
                    .any(|display| contains(display, local_x, local_y))
            })
            .map(|neighbor| neighbor.id.clone());
        let current = self
            .peer
            .iter()
            .find(|display| contains(display, self.x, self.y))
            .copied()
            .unwrap_or(self.peer[0]);
        (self.x, self.y) = clamp_into(&current, x, y);
        match neighbor {
            Some(id) => Motion::Neighbor {
                id,
                x: local_x,
                y: local_y,
            },
            None => Motion::Stay,
        }
    }

    /// Crosses through `portal` from the local point `(x, y)` on its edge.
    /// The position is where the pointer appears on the peer (send it as an
    /// absolute move so the peer's cursor starts there too).
    pub fn enter(
        peer: &[Display],
        portals: &[Portal],
        offset: Offset,
        portal: Portal,
        x: f64,
        y: f64,
    ) -> Self {
        let peer = peer_or_default(peer);
        let display = peer.get(portal.peer_display).copied().unwrap_or(peer[0]);
        let (mut px, mut py) = clamp_into(&display, x - offset.x, y - offset.y);
        match portal.side {
            Side::Right => px = (px + CROSSING_MARGIN).min(display.right() - 1.0),
            Side::Left => px = (px - CROSSING_MARGIN).max(display.x),
            Side::Bottom => py = (py + CROSSING_MARGIN).min(display.bottom() - 1.0),
            Side::Top => py = (py - CROSSING_MARGIN).max(display.y),
        }
        Self {
            peer,
            portals: portals.to_vec(),
            offset,
            entry: Some(portal),
            neighbors: Vec::new(),
            x: px,
            y: py,
        }
    }

    /// Where the pointer is on the peer, in the peer's space.
    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// Moves by `(dx, dy)` points. Returns the local point to continue at
    /// when the move leaves the peer through a portal; the crossing is then
    /// over and this pointer should be dropped. Neighbours are edges here
    /// (see [`RemotePointer::step`]).
    pub fn move_by(&mut self, dx: f64, dy: f64) -> Option<(f64, f64)> {
        match self.step(dx, dy) {
            Motion::Home(x, y) => Some((x, y)),
            Motion::Stay | Motion::Neighbor { .. } => None,
        }
    }

    /// A local point just inside the edge the pointer left through, near
    /// `(x, y)`, for giving control back without crossing; `(x, y)` itself
    /// when it came by shortcut.
    pub fn return_point(&self, x: f64, y: f64) -> (f64, f64) {
        let Some(portal) = &self.entry else {
            return (x, y);
        };
        let along = |value: f64| clamp_between(value, portal.from, portal.to - 1.0);
        match portal.side {
            Side::Right => (portal.line - RETURN_INSET, along(y)),
            Side::Left => (portal.line + RETURN_INSET, along(y)),
            Side::Bottom => (along(x), portal.line - RETURN_INSET),
            Side::Top => (along(x), portal.line + RETURN_INSET),
        }
    }
}

fn contains(display: &Display, x: f64, y: f64) -> bool {
    x >= display.x && x < display.right() && y >= display.y && y < display.bottom()
}

/// The nearest point inside the display (the far edges are exclusive).
fn clamp_into(display: &Display, x: f64, y: f64) -> (f64, f64) {
    (
        clamp_between(x, display.x, display.right() - 1.0),
        clamp_between(y, display.y, display.bottom() - 1.0),
    )
}

/// `value` within `low..=high`, or `low` when rounding left `high` below
/// it. Unlike `f64::clamp` it never panics: geometry comes from peers.
fn clamp_between(value: f64, low: f64, high: f64) -> f64 {
    value.min(high).max(low)
}

/// If the local point `(x, y)` lies back on this device's side of the
/// portal's edge and within its extent, where the pointer continues locally.
fn exit_through(portal: &Portal, x: f64, y: f64) -> Option<(f64, f64)> {
    let (back, along) = match portal.side {
        Side::Right => (x < portal.line, y),
        Side::Left => (x >= portal.line, y),
        Side::Bottom => (y < portal.line, x),
        Side::Top => (y >= portal.line, x),
    };
    if !back || !(portal.from..portal.to).contains(&along) {
        return None;
    }
    Some(match portal.side {
        Side::Right => (portal.line - CROSSING_MARGIN, y),
        Side::Left => (portal.line + CROSSING_MARGIN, y),
        Side::Bottom => (x, portal.line - CROSSING_MARGIN),
        Side::Top => (x, portal.line + CROSSING_MARGIN),
    })
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
    fn the_remote_pointer_moves_on_the_peer_and_comes_back_through_the_portal() {
        let mac = [display(0.0, 0.0, 1512.0, 982.0)];
        let offset = Offset {
            x: 1440.0,
            y: 100.0,
        };
        let found = portals(&[LAPTOP], &mac, offset);
        // Crossing at local y 500 lands two points inside the Mac's left edge.
        let mut pointer = RemotePointer::enter(&mac, &found, offset, found[0], 1440.0, 500.0);
        assert_eq!(pointer.position(), (2.0, 400.0));
        // A small move back does not bounce straight across again.
        assert_eq!(pointer.move_by(-1.0, 0.0), None);
        assert_eq!(pointer.position(), (1.0, 400.0));
        // Nothing is above the Mac: the pointer slides along its top edge.
        assert_eq!(pointer.move_by(100.0, -1000.0), None);
        assert_eq!(pointer.position(), (101.0, 0.0));
        // Below the laptop (local y 950) the Mac's left edge leads nowhere.
        assert_eq!(pointer.move_by(0.0, 850.0), None);
        assert_eq!(pointer.move_by(-200.0, 0.0), None);
        assert_eq!(pointer.position(), (0.0, 850.0));
        // Within the portal it comes back two points inside the laptop.
        assert_eq!(pointer.move_by(0.0, -400.0), None);
        assert_eq!(pointer.move_by(-3.0, 0.0), Some((1438.0, 550.0)));
        // Released without crossing: just inside the edge it left through.
        assert_eq!(pointer.return_point(1440.0, 2000.0), (1432.0, 899.0));
    }

    #[test]
    fn three_devices_stand_in_a_row_whichever_has_the_keyboard() {
        // This laptop, a Mac to its right, and a PC dropped right of the Mac.
        let mac = [display(0.0, 0.0, 1512.0, 982.0)];
        let pc = [display(0.0, 0.0, 1920.0, 1080.0)];
        let mac_offset = Offset { x: 1440.0, y: 0.0 };
        let others = vec![placed(&mac, mac_offset)];
        let drop = Offset {
            x: 1440.0 + 1512.0 + 30.0,
            y: 20.0,
        };
        // Against the Mac, not pulled back next to the laptop.
        let chained = snap_in_arrangement(&[LAPTOP], &pc, drop, &others).expect("a spot");
        assert_eq!(chained.x, 1440.0 + 1512.0);
        assert!(is_valid_in_arrangement(&[LAPTOP], &pc, chained, &others));
        assert!(portals(&[LAPTOP], &pc, chained).is_empty());
        // Placements that only touch the laptop still work as before, and
        // the old rules (no chaining) still pull the PC back to the laptop.
        let alone = snap(&[LAPTOP], &pc, drop, &others[0]).expect("a spot");
        assert!(!portals(&[LAPTOP], &pc, alone).is_empty());
        // Peers that connect only to each other float: not anchors.
        let island = vec![
            placed(&mac, Offset { x: 5000.0, y: 0.0 }),
            placed(&pc, Offset { x: 6512.0, y: 0.0 }),
        ];
        assert_eq!(connected(&[LAPTOP], &island), vec![false, false]);
        assert!(!is_valid_in_arrangement(
            &[LAPTOP],
            &pc,
            Offset { x: 6512.0, y: 0.0 },
            &island[..1]
        ));
        assert_eq!(
            connected(&[LAPTOP], &[others[0].clone(), placed(&pc, chained)]),
            vec![true, true]
        );
    }

    #[test]
    fn the_pointer_moves_from_one_peer_straight_into_the_next() {
        let mac = [display(0.0, 0.0, 1512.0, 982.0)];
        let pc = [display(0.0, 0.0, 1920.0, 1080.0)];
        let mac_offset = Offset { x: 1440.0, y: 0.0 };
        let pc_offset = Offset {
            x: 1440.0 + 1512.0,
            y: 0.0,
        };
        let found = portals(&[LAPTOP], &mac, mac_offset);
        let mut pointer = RemotePointer::enter(&mac, &found, mac_offset, found[0], 1440.0, 400.0)
            .with_neighbors(vec![Neighbor {
                id: "pc".to_owned(),
                displays: placed(&pc, pc_offset),
            }]);
        // Across the Mac to its right edge, then one point further.
        assert_eq!(pointer.step(1508.0, 0.0), Motion::Stay);
        assert_eq!(
            pointer.step(3.0, 0.0),
            Motion::Neighbor {
                id: "pc".to_owned(),
                x: 1440.0 + 1513.0,
                y: 400.0
            }
        );
        // The pointer itself stopped at the edge (as if nothing was there).
        assert_eq!(pointer.position(), (1511.0, 400.0));
        // On the PC from that point; the way home is still the first portal.
        let next =
            RemotePointer::enter_at(&pc, &[], pc_offset, 1440.0 + 1513.0, 400.0, pointer.entry());
        assert_eq!(next.position(), (1.0, 400.0));
        assert_eq!(next.local_position(), (2953.0, 400.0));
        assert_eq!(next.return_point(1440.0, 400.0), (1432.0, 400.0));
        // By shortcut (no entry), giving control back returns where it was.
        let jumped = RemotePointer::enter_at(&pc, &[], pc_offset, 5000.0, 500.0, None);
        assert_eq!(jumped.position(), (1919.0, 500.0));
        assert_eq!(jumped.return_point(700.0, 450.0), (700.0, 450.0));
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
    fn an_l_shaped_peer_snaps_by_its_own_displays() {
        // A Mac mini: a wide display, and a portrait one on its left that
        // reaches 480 points lower. Its bounding box is not a display.
        let pc = [display(0.0, 0.0, 1440.0, 960.0)];
        let mac = [
            display(0.0, 0.0, 2560.0, 1440.0),
            display(-1080.0, 0.0, 1080.0, 1920.0),
        ];
        let snapped_to = |x: f64, y: f64| snap(&pc, &mac, Offset { x, y }, &[]).unwrap();

        // Dropped centred above the PC, a little high: the wide display sits
        // on the PC's top edge (centres aligned), the portrait one beside it.
        let above = snapped_to((1440.0 - 2560.0) / 2.0, -1500.0);
        assert_eq!(
            above,
            Offset {
                x: -560.0,
                y: -1440.0
            }
        );
        let found = portals(&pc, &mac, above);
        assert_eq!(
            (
                found[0].side,
                found[0].peer_display,
                found[0].from,
                found[0].to
            ),
            (Side::Top, 0, 0.0, 1440.0)
        );

        // Dropped above but too far right for the portrait display to clear
        // the PC: moved along the edge until it does.
        let shifted = snapped_to(300.0, -1490.0);
        assert_eq!(shifted, Offset { x: 0.0, y: -1440.0 });
        assert!(is_valid_placement(&pc, &mac, shifted, &[]));

        // To the right, the portrait display's left edge touches.
        let right = snapped_to(2600.0, 0.0);
        assert_eq!(
            right,
            Offset {
                x: 1440.0 + 1080.0,
                y: 0.0
            }
        );
        assert_eq!(portals(&pc, &mac, right)[0].peer_display, 1);

        // Below, both displays start at the same height and touch.
        let below = snapped_to(-560.0, 1000.0);
        assert_eq!(below.y, 960.0);
        assert!(is_valid_placement(&pc, &mac, below, &[]));
    }

    #[test]
    fn every_drop_lands_on_a_valid_spot_with_a_usable_edge() {
        let pc = [display(0.0, 0.0, 1440.0, 960.0)];
        let mac = [
            display(0.0, 0.0, 2560.0, 1440.0),
            display(-1080.0, 0.0, 1080.0, 1920.0),
        ];
        for x in (-6000..=4000).step_by(250) {
            for y in (-4000..=3000).step_by(250) {
                let proposed = Offset {
                    x: f64::from(x),
                    y: f64::from(y),
                };
                let snapped = snap(&pc, &mac, proposed, &[]).expect("a spot");
                assert!(is_valid_placement(&pc, &mac, snapped, &[]), "{proposed:?}");
                let longest = portals(&pc, &mac, snapped)
                    .iter()
                    .map(|portal| portal.to - portal.from)
                    .fold(0.0, f64::max);
                assert!(
                    longest >= MIN_OVERLAP,
                    "{proposed:?} -> {snapped:?}: {longest}"
                );
            }
        }
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
