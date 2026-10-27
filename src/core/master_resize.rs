//! Layout-local physical-axis resizing. Every plan starts from native layout
//! rectangles, never from the previous arranged result. A single stable client
//! owns the intent; neighbours reflow through the same monotone coordinate map.

use super::types::Rect;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeIntent<K> {
    pub client: K,
    /// Content size as a fraction of the current usable output, not a pixel
    /// rectangle. Output moves/resizes therefore cannot strand the anchor.
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeFrame<K> {
    pub client: K,
    pub rect: Rect,
    pub border: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResizeContext<K> {
    pub topology: Vec<K>,
    pub groups: Vec<Vec<K>>,
    pub intent: Option<ResizeIntent<K>>,
    pub area: Rect,
    pub gap: i32,
    pub frames: Vec<ResizeFrame<K>>,
    /// A track-only resize grab owns this snapshot until release/cancel.
    pub drag_snapshot: Option<Option<ResizeIntent<K>>>,
    /// Only explicit resize commands may prime native slots without an intent.
    pub preparing: bool,
}

impl<K> Default for ResizeContext<K> {
    fn default() -> Self {
        Self {
            topology: Vec::new(),
            groups: Vec::new(),
            intent: None,
            area: Rect::default(),
            gap: 0,
            frames: Vec::new(),
            drag_snapshot: None,
            preparing: false,
        }
    }
}

impl<K: Copy + Eq> ResizeContext<K> {
    fn cancel_drag(&mut self) {
        if let Some(original) = self.drag_snapshot.take() {
            self.intent = original;
        }
    }

    pub fn sync_groups(&mut self, groups: Vec<Vec<K>>) {
        if self.groups != groups {
            // A new grouping invalidates a grab's old coordinate system, but
            // the same live anchor still owns its normalized size intent.
            self.cancel_drag();
        }
        self.groups = groups;
    }

    pub fn sync(&mut self, topology: Vec<K>, area: Rect, gap: i32, frames: Vec<ResizeFrame<K>>) {
        if self.topology != topology {
            self.cancel_drag();
        }
        // When the last neighbour leaves, the survivor takes its native
        // singleton geometry instead of retaining a master/stack-sized box.
        // Explicit resizing of an existing singleton still survives arrange.
        // Other topology changes retain a live anchor's manual size.
        if (self.topology.len() > 1 && topology.len() == 1)
            || self
                .intent
                .is_some_and(|intent| !topology.contains(&intent.client))
        {
            self.intent = None;
        }
        self.topology = topology;
        self.area = area;
        self.gap = gap.max(0);
        self.frames = frames;
    }
}

pub fn usable_content(area: Rect, gap: i32, border: i32) -> (f64, f64) {
    let gap = gap.clamp(0, area.w.max(0).min(area.h.max(0)) / 2);
    let padding = 2.0 * f64::from(gap) + 2.0 * f64::from(border.max(0));
    (
        (f64::from(area.w) - padding).max(1.0),
        (f64::from(area.h) - padding).max(1.0),
    )
}

pub fn intent_from_rect<K: Copy>(
    client: K,
    rect: Rect,
    area: Rect,
    gap: i32,
    border: i32,
) -> ResizeIntent<K> {
    let (w, h) = usable_content(area, gap, border);
    ResizeIntent {
        client,
        width: (f64::from(rect.w.max(1)) / w).clamp(0.0, 1.0),
        height: (f64::from(rect.h.max(1)) / h).clamp(0.0, 1.0),
    }
}

/// Physical delta, independent of layout orientation and existing mfact/cfact
/// keyboard meanings. Zero is a no-op, not an implicit reset.
pub fn adjust_intent<K: Copy>(
    mut intent: ResizeIntent<K>,
    horizontal: bool,
    delta: f64,
) -> Option<ResizeIntent<K>> {
    if !delta.is_finite() {
        return None;
    }
    let value = if horizontal {
        &mut intent.width
    } else {
        &mut intent.height
    };
    *value = (*value + delta).clamp(0.0, 1.0);
    Some(intent)
}

#[derive(Clone, Copy)]
struct AxisMap {
    lo: f64,
    hi: f64,
    start: f64,
    end: f64,
    new_start: f64,
    new_end: f64,
    full: bool,
}

impl AxisMap {
    fn new(lo: f64, hi: f64, start: f64, end: f64, desired: f64) -> Self {
        let extent = (hi - lo).max(1.0);
        let desired = desired.clamp(1.0, extent);
        let before = (start - lo).max(0.0);
        let after = (hi - end).max(0.0);
        let full = before + after < 0.001;
        let new_start = if full {
            lo + (extent - desired) / 2.0
        } else {
            lo + (extent - desired) * before / (before + after)
        };
        Self {
            lo,
            hi,
            start,
            end,
            new_start,
            new_end: new_start + desired,
            full,
        }
    }

    fn map(self, value: f64) -> f64 {
        if self.full {
            return value;
        }
        if value < self.start && self.start > self.lo {
            self.lo + (value - self.lo) * (self.new_start - self.lo) / (self.start - self.lo)
        } else if value > self.end && self.end < self.hi {
            self.new_end + (value - self.end) * (self.hi - self.new_end) / (self.hi - self.end)
        } else {
            self.new_start
                + (value - self.start) * (self.new_end - self.new_start)
                    / (self.end - self.start).max(1.0)
        }
    }

    fn interval(self, lo: f64, hi: f64, anchor: bool) -> (f64, f64) {
        if self.full && anchor {
            (self.new_start, self.new_end)
        } else {
            (self.map(lo), self.map(hi))
        }
    }
}

/// Exact plan or None. `fit` is the same size-hint policy used by the shell.
/// Hints may shorten the offered box, but may never enlarge it into a neighbour.
/// This check happens before applying any geometry, atomically for all clients.
pub fn exact_plan<K: Copy + Eq>(
    area: Rect,
    gap: i32,
    frames: &[ResizeFrame<K>],
    intent: ResizeIntent<K>,
    fit: &impl Fn(K, i32, i32) -> (i32, i32),
) -> Option<Vec<ResizeFrame<K>>> {
    if area.w <= 0
        || area.h <= 0
        || !intent.width.is_finite()
        || !intent.height.is_finite()
        || !(0.0..=1.0).contains(&intent.width)
        || !(0.0..=1.0).contains(&intent.height)
    {
        return None;
    }
    let anchor = frames.iter().find(|f| f.client == intent.client)?;
    let gap = gap.clamp(0, area.w.min(area.h) / 2);
    let half = f64::from(gap) / 2.0;
    let allocation = |f: &ResizeFrame<K>| {
        let border = 2.0 * f64::from(f.border.max(0));
        (
            f64::from(f.rect.x) - half,
            f64::from(f.rect.y) - half,
            f64::from(f.rect.x) + f64::from(f.rect.w) + border + half,
            f64::from(f.rect.y) + f64::from(f.rect.h) + border + half,
        )
    };
    let (mut ax, mut ay, mut bx, mut by) = allocation(anchor);
    let (mut left, mut top, mut right, mut bottom) = (
        f64::from(area.x) + half,
        f64::from(area.y) + half,
        f64::from(area.x) + f64::from(area.w) - half,
        f64::from(area.y) + f64::from(area.h) - half,
    );
    // Scrolling has deliberately off-screen columns. Its allocation domain is
    // the strip; content-size targets are still bounded by the visible output.
    if frames.len() > 1 {
        for f in frames {
            let (x1, y1, x2, y2) = allocation(f);
            left = left.min(x1);
            top = top.min(y1);
            right = right.max(x2);
            bottom = bottom.max(y2);
        }
    } else {
        // A singleton owns the whole available output, including in vstack.
        ax = left;
        ay = top;
        bx = right;
        by = bottom;
    }
    let (cw, ch) = usable_content(area, gap, anchor.border);
    let border_gap = 2.0 * f64::from(anchor.border.max(0)) + f64::from(gap);
    let xmap = AxisMap::new(left, right, ax, bx, cw * intent.width + border_gap);
    let ymap = AxisMap::new(top, bottom, ay, by, ch * intent.height + border_gap);
    let mut out = Vec::with_capacity(frames.len());
    for f in frames {
        let is_anchor = f.client == intent.client;
        let (x1, y1, x2, y2) = allocation(f);
        let (x1, x2) = xmap.interval(x1, x2, is_anchor);
        let (y1, y2) = ymap.interval(y1, y2, is_anchor);
        let x = (x1 + half).round();
        let y = (y1 + half).round();
        let border = 2.0 * f64::from(f.border.max(0));
        let w = (x2 - half).round() - x - border;
        let h = (y2 - half).round() - y - border;
        if w < 1.0
            || h < 1.0
            || w > f64::from(u16::MAX)
            || h > f64::from(u16::MAX)
            || x < f64::from(i32::MIN)
            || x > f64::from(i32::MAX)
            || y < f64::from(i32::MIN)
            || y > f64::from(i32::MAX)
        {
            return None;
        }
        let (w, h) = (w as i32, h as i32);
        let (fw, fh) = fit(f.client, w, h);
        if fw < 1 || fh < 1 || fw > w || fh > h {
            return None;
        }
        out.push(ResizeFrame {
            client: f.client,
            rect: Rect::new(
                (x as i32).saturating_add((w - fw) / 2),
                (y as i32).saturating_add((h - fh) / 2),
                fw,
                fh,
            ),
            border: f.border,
        });
    }
    // A configured gap can exceed the effective inner gap of a tiny layout.
    // Expanding allocations by that gap then mapping is not itself sufficient
    // to preserve separation. Validate final outer frames after hints too;
    // intentional pre-existing overlaps remain part of the original layout.
    for i in 0..frames.len() {
        for j in i + 1..frames.len() {
            if !outer_overlap(frames[i], frames[j]) && outer_overlap(out[i], out[j]) {
                return None;
            }
        }
    }
    Some(out)
}

fn outer_overlap<K>(a: ResizeFrame<K>, b: ResizeFrame<K>) -> bool {
    let (ax, ay, bx, by) = (
        i64::from(a.rect.x),
        i64::from(a.rect.y),
        i64::from(b.rect.x),
        i64::from(b.rect.y),
    );
    let aw = i64::from(a.rect.w) + 2 * i64::from(a.border.max(0));
    let ah = i64::from(a.rect.h) + 2 * i64::from(a.border.max(0));
    let bw = i64::from(b.rect.w) + 2 * i64::from(b.border.max(0));
    let bh = i64::from(b.rect.h) + 2 * i64::from(b.border.max(0));
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// Clamp a request along the segment from a known feasible intent. The caller
/// passes the current intent for interactive edits, the native one after an
/// output/constraint change. An infeasible starting point never authorizes a
/// partial plan. Finite bisection cannot accumulate rounding drift.
pub fn bounded_plan<K: Copy + Eq>(
    area: Rect,
    gap: i32,
    frames: &[ResizeFrame<K>],
    start: ResizeIntent<K>,
    wanted: ResizeIntent<K>,
    fit: &impl Fn(K, i32, i32) -> (i32, i32),
) -> Option<(ResizeIntent<K>, Vec<ResizeFrame<K>>)> {
    if start.client != wanted.client || !wanted.width.is_finite() || !wanted.height.is_finite() {
        return None;
    }
    if let Some(plan) = exact_plan(area, gap, frames, wanted, fit) {
        return Some((wanted, plan));
    }
    let mut best = (start, exact_plan(area, gap, frames, start, fit)?);
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..32 {
        let t = (low + high) / 2.0;
        let intent = ResizeIntent {
            client: wanted.client,
            width: start.width + (wanted.width - start.width) * t,
            height: start.height + (wanted.height - start.height) * t,
        };
        if let Some(plan) = exact_plan(area, gap, frames, intent, fit) {
            low = t;
            best = (intent, plan);
        } else {
            high = t;
        }
    }
    Some(best)
}

/// Center a scrolling column after resizing its strip. Every rectangle receives
/// the same translation, preserving pair relationships and gaps. The fit policy
/// must first cap column content to the visible work area.
pub fn center_group<K: Copy + Eq>(
    area: Rect,
    gap: i32,
    frames: &mut [ResizeFrame<K>],
    group: &[K],
) -> bool {
    let mut left = i64::MAX;
    let mut right = i64::MIN;
    for f in frames.iter().filter(|f| group.contains(&f.client)) {
        left = left.min(i64::from(f.rect.x));
        right =
            right.max(i64::from(f.rect.x) + i64::from(f.rect.w) + 2 * i64::from(f.border.max(0)));
    }
    if left == i64::MAX {
        return false;
    }
    let gap = i64::from(gap.clamp(0, area.w.max(0).min(area.h.max(0)) / 2));
    let available = i64::from(area.w) - 2 * gap;
    if right - left > available || available < 1 {
        return false;
    }
    let target = i64::from(area.x) + gap + (available - (right - left)) / 2;
    let delta = target - left;
    if frames
        .iter()
        .any(|f| i32::try_from(i64::from(f.rect.x) + delta).is_err())
    {
        return false;
    }
    for f in frames {
        f.rect.x = (i64::from(f.rect.x) + delta) as i32;
    }
    true
}

/// Convert pointer travel to a content-size delta using the boundary's actual
/// motion rate. A full-axis/singleton box is centered (half-speed edges), while
/// a master pinned at the left has a full-speed right edge. A pinned edge has
/// no outward travel; dragging it still adjusts size through the opposite edge.
pub fn pointer_resize_gains<K: Copy + Eq>(
    area: Rect,
    gap: i32,
    frames: &[ResizeFrame<K>],
    key: K,
    left_edge: bool,
    top_edge: bool,
) -> (f64, f64) {
    if frames.len() <= 1 {
        return (2.0, 2.0);
    }
    let Some(anchor) = frames.iter().find(|f| f.client == key) else {
        return (1.0, 1.0);
    };
    let half = f64::from(gap.clamp(0, area.w.max(0).min(area.h.max(0)) / 2)) / 2.0;
    let gain = |horizontal: bool, low_edge: bool| {
        let mut lo = if horizontal {
            f64::from(area.x)
        } else {
            f64::from(area.y)
        } + half;
        let mut hi = if horizontal {
            f64::from(area.x) + f64::from(area.w)
        } else {
            f64::from(area.y) + f64::from(area.h)
        } - half;
        let interval = |f: &ResizeFrame<K>| {
            let start = if horizontal {
                f64::from(f.rect.x)
            } else {
                f64::from(f.rect.y)
            };
            let size = if horizontal {
                f64::from(f.rect.w)
            } else {
                f64::from(f.rect.h)
            };
            (
                start - half,
                start + size + 2.0 * f64::from(f.border.max(0)) + half,
            )
        };
        for f in frames {
            let (a, b) = interval(f);
            lo = lo.min(a);
            hi = hi.max(b);
        }
        let (a, b) = interval(anchor);
        let before = (a - lo).max(0.0);
        let after = (hi - b).max(0.0);
        if before + after < 0.001 {
            return 2.0;
        }
        let rate = if low_edge {
            before / (before + after)
        } else {
            after / (before + after)
        };
        if rate < 0.001 { 1.0 } else { 1.0 / rate }
    };
    (gain(true, left_edge), gain(false, top_edge))
}

/// Edge signs shared by pointer events and pure tests. A left/top drag has the
/// inverse sign. Geometry-dependent edge gains are applied separately.
pub fn pointer_resize_delta(
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
    dx: f64,
    dy: f64,
) -> (f64, f64) {
    (
        (if left {
            -dx
        } else if right {
            dx
        } else {
            0.0
        }),
        (if top {
            -dy
        } else if bottom {
            dy
        } else {
            0.0
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(client: u32, x: i32, y: i32, w: i32, h: i32) -> ResizeFrame<u32> {
        ResizeFrame {
            client,
            rect: Rect::new(x, y, w, h),
            border: 0,
        }
    }
    fn fit(_: u32, w: i32, h: i32) -> (i32, i32) {
        (w, h)
    }
    #[test]
    fn singleton_uses_whole_area_and_centers_even_a_small_native_card() {
        let area = Rect::new(-1600, 40, 1600, 900);
        let f = [frame(1, -1200, 490, 800, 450)];
        let intent = ResizeIntent {
            client: 1,
            width: 0.8,
            height: 0.8,
        };
        let out = exact_plan(area, 0, &f, intent, &fit).unwrap();
        assert_eq!(out[0].rect, Rect::new(-1440, 130, 1280, 720));
        for _ in 0..100 {
            assert_eq!(out, exact_plan(area, 0, &f, intent, &fit).unwrap());
        }
    }
    #[test]
    fn both_boundary_anchors_and_centered_anchors_reflow_without_overlap() {
        let area = Rect::new(0, 0, 1000, 800);
        let f = [frame(1, 10, 10, 480, 780), frame(2, 500, 10, 490, 780)];
        for client in [1, 2] {
            let out = exact_plan(
                area,
                10,
                &f,
                ResizeIntent {
                    client,
                    width: 0.65,
                    height: 1.0,
                },
                &fit,
            )
            .unwrap();
            assert_eq!(out[1].rect.x - (out[0].rect.x + out[0].rect.w), 10);
            assert_eq!(out[0].rect.x, 10);
            assert_eq!(out[1].rect.x + out[1].rect.w, 990);
        }
        let f = [
            frame(1, 10, 10, 240, 780),
            frame(2, 260, 10, 480, 780),
            frame(3, 750, 10, 240, 780),
        ];
        let out = exact_plan(
            area,
            10,
            &f,
            ResizeIntent {
                client: 2,
                width: 0.7,
                height: 0.7,
            },
            &fit,
        )
        .unwrap();
        assert_eq!(out[1].rect.y, 127);
        assert_eq!(out[0].rect.y, 10);
        assert_eq!(out[1].rect.x - out[0].rect.x - out[0].rect.w, 10);
        assert_eq!(out[2].rect.x - out[1].rect.x - out[1].rect.w, 10);
    }
    #[test]
    fn neighbour_minimum_clamps_atomically_and_no_hint_can_grow_after_projection() {
        let area = Rect::new(0, 0, 1000, 800);
        let f = [frame(1, 10, 10, 480, 780), frame(2, 500, 10, 490, 780)];
        let hints = |_: u32, w: i32, h: i32| (w.max(200), h.max(100));
        let start = intent_from_rect(1, f[0].rect, area, 10, 0);
        let wanted = ResizeIntent {
            client: 1,
            width: 1.0,
            height: 1.0,
        };
        let (_, out) = bounded_plan(area, 10, &f, start, wanted, &hints).unwrap();
        assert!(out[1].rect.w >= 200);
        assert_eq!(out[1].rect.x - out[0].rect.x - out[0].rect.w, 10);
        assert!(exact_plan(area, 10, &f, wanted, &hints).is_none());
    }
    #[test]
    fn full_axis_shrink_is_target_local_while_shared_partition_can_reflow() {
        let area = Rect::new(0, 0, 1000, 800);
        let f = [
            frame(1, 0, 0, 500, 400),
            frame(2, 0, 400, 500, 400),
            frame(3, 500, 0, 500, 800),
        ];
        let out = exact_plan(
            area,
            0,
            &f,
            ResizeIntent {
                client: 2,
                width: 0.7,
                height: 0.3,
            },
            &fit,
        )
        .unwrap();
        assert_eq!(out[0].rect.w, 700);
        assert_eq!(out[1].rect.w, 700);
        assert_eq!(out[0].rect.h, 560);
        assert_eq!(out[1].rect.h, 240);
        let out = exact_plan(
            area,
            0,
            &f,
            ResizeIntent {
                client: 3,
                width: 0.5,
                height: 0.5,
            },
            &fit,
        )
        .unwrap();
        assert_eq!(out[2].rect, Rect::new(500, 200, 500, 400));
        assert_eq!(out[0].rect, f[0].rect);
        assert_eq!(out[1].rect, f[1].rect);
    }
    #[test]
    fn context_retains_anchor_across_membership_and_output_changes() {
        let f = vec![frame(1, 0, 0, 100, 100)];
        let mut c = ResizeContext::default();
        c.sync(vec![1], Rect::new(0, 0, 100, 100), 0, f.clone());
        let i = ResizeIntent {
            client: 1,
            width: 0.7,
            height: 0.6,
        };
        c.intent = Some(i);
        c.sync(vec![1], Rect::new(-200, 100, 200, 300), 0, f.clone());
        assert_eq!(c.intent, Some(i));
        c.drag_snapshot = Some(Some(i));
        c.sync(vec![2, 1], c.area, 0, f.clone());
        assert_eq!(c.intent, Some(i));
        assert_eq!(c.drag_snapshot, None);
        for topology in [vec![1, 2], vec![1, 3, 2], vec![1, 3]] {
            c.drag_snapshot = Some(Some(i));
            c.sync(topology, c.area, 0, f.clone());
            assert_eq!(c.intent, Some(i));
            assert_eq!(c.drag_snapshot, None);
        }
        c.sync(vec![2], c.area, 0, vec![frame(2, 0, 0, 100, 100)]);
        assert_eq!(c.intent, None);
        c.sync(vec![1, 2], c.area, 0, f);
        assert_eq!(c.intent, None, "a removed anchor must not resurrect");
    }
    #[test]
    fn singleton_transition_discards_multiwindow_size_and_pending_drag() {
        let area = Rect::new(0, 0, 1000, 800);
        let original = ResizeIntent {
            client: 1,
            width: 0.55,
            height: 1.0,
        };
        let provisional = ResizeIntent {
            width: 0.4,
            ..original
        };
        let mut c = ResizeContext {
            topology: vec![1, 2],
            intent: Some(provisional),
            drag_snapshot: Some(Some(original)),
            ..ResizeContext::default()
        };
        c.sync(vec![1], area, 0, vec![frame(1, 0, 0, 1000, 800)]);
        assert_eq!(c.intent, None);
        assert_eq!(
            c.drag_snapshot, None,
            "late drag completion must not revive the old size"
        );
        c.sync(vec![1, 2], area, 0, vec![]);
        assert_eq!(
            c.intent, None,
            "returning neighbours cannot revive the old size"
        );
        c.sync(vec![1], area, 0, vec![]);
        c.intent = Some(original);
        c.sync(vec![1], area, 0, vec![]);
        assert_eq!(
            c.intent,
            Some(original),
            "an explicit singleton resize still persists"
        );
    }

    #[test]
    fn grouping_changes_retain_intent_but_cancel_drag() {
        let mut c = ResizeContext::default();
        c.sync(vec![1, 2, 3], Rect::new(0, 0, 1000, 800), 0, vec![]);
        let intent = ResizeIntent {
            client: 2,
            width: 0.6,
            height: 0.4,
        };
        c.intent = Some(intent);
        c.groups = vec![vec![1, 2], vec![3]];
        c.drag_snapshot = Some(None);
        c.sync_groups(c.groups.clone());
        assert_eq!(c.drag_snapshot, Some(None));
        for groups in [vec![vec![1], vec![2, 3]], vec![vec![3, 2], vec![1]], vec![]] {
            c.drag_snapshot = Some(Some(intent));
            c.sync_groups(groups);
            assert_eq!(c.intent, Some(intent));
            assert_eq!(c.drag_snapshot, None);
        }
    }

    #[test]
    fn topology_or_group_change_rolls_back_uncommitted_drag() {
        let original = ResizeIntent {
            client: 1,
            width: 0.6,
            height: 0.4,
        };
        let draft = ResizeIntent {
            client: 2,
            width: 0.8,
            height: 0.9,
        };
        for grouped in [false, true] {
            for saved in [None, Some(original)] {
                let mut c = ResizeContext::default();
                c.sync(vec![1, 2], Rect::new(0, 0, 1000, 800), 0, vec![]);
                c.groups = vec![vec![1, 2]];
                c.intent = Some(draft);
                c.drag_snapshot = Some(saved);
                if grouped {
                    c.sync_groups(vec![vec![1], vec![2]]);
                } else {
                    c.sync(vec![2, 1, 3], c.area, 0, vec![]);
                }
                assert_eq!(c.intent, saved);
                assert_eq!(c.drag_snapshot, None);
            }
        }
        let mut c = ResizeContext {
            topology: vec![1, 2],
            intent: Some(draft),
            drag_snapshot: Some(Some(original)),
            ..ResizeContext::default()
        };
        c.sync(vec![2, 3], Rect::new(0, 0, 1000, 800), 0, vec![]);
        assert_eq!(c.intent, None, "removed original anchor cannot be restored");
        assert_eq!(c.drag_snapshot, None);
    }

    #[test]
    fn retained_anchor_replans_from_fresh_frames_without_drift() {
        let area = Rect::new(-200, 50, 1000, 800);
        let mut c = ResizeContext::default();
        let intent = ResizeIntent {
            client: 1,
            width: 0.6,
            height: 0.4,
        };
        c.sync(vec![1], area, 0, vec![frame(1, -200, 50, 1000, 800)]);
        c.intent = Some(intent);
        let fresh = vec![frame(2, -200, 50, 500, 800), frame(1, 300, 50, 500, 800)];
        let expected = exact_plan(area, 0, &fresh, intent, &fit).unwrap();
        assert_eq!(expected[1].rect.w, 600);
        assert_eq!(expected[1].rect.h, 320);
        for _ in 0..20 {
            c.sync(vec![2, 1], area, 0, fresh.clone());
            assert_eq!(c.intent, Some(intent));
            assert_eq!(
                exact_plan(c.area, c.gap, &c.frames, c.intent.unwrap(), &fit).unwrap(),
                expected
            );
            assert_eq!(c.frames, fresh);
        }
    }

    #[test]
    fn nonfinite_extreme_hints_and_degenerate_areas_are_rejected() {
        let f = [frame(1, 0, 0, 100, 100)];
        let area = Rect::new(0, 0, 100, 100);
        for width in [f64::NAN, f64::INFINITY, -1.0, 2.0] {
            assert!(
                exact_plan(
                    area,
                    0,
                    &f,
                    ResizeIntent {
                        client: 1,
                        width,
                        height: 0.5
                    },
                    &fit
                )
                .is_none()
            );
        }
        assert!(
            exact_plan(
                Rect::new(0, 0, 0, 100),
                0,
                &f,
                ResizeIntent {
                    client: 1,
                    width: 0.5,
                    height: 0.5
                },
                &fit
            )
            .is_none()
        );
        assert!(
            exact_plan(
                area,
                0,
                &f,
                ResizeIntent {
                    client: 1,
                    width: 0.5,
                    height: 0.5
                },
                &|_, _, _| (i32::MAX, 1)
            )
            .is_none()
        );
    }
    #[test]
    fn pointer_edges_map_to_physical_axes_and_do_not_change_the_other_axis() {
        assert_eq!(
            pointer_resize_delta(true, false, false, false, 10.0, 40.0),
            (-10.0, 0.0)
        );
        assert_eq!(
            pointer_resize_delta(false, true, true, false, 10.0, 40.0),
            (10.0, -40.0)
        );
        assert_eq!(
            pointer_resize_delta(false, false, false, true, 10.0, 40.0),
            (0.0, 40.0)
        );
    }
}
