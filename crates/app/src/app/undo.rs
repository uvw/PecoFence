//! Ctrl+Z in a fence, as on the desktop: undoes the last fence move or resize, icons moved
//! between fences, or a fence deleted from its menu. File operations (a deleted file, a
//! file moved into a folder) stay with Windows' own undo. A short stack in memory, gone at
//! restart and when the whole layout is replaced (import, backup, snapshot restore).

use super::*;
use crate::state::DeletedFence;

/// Steps kept; the oldest go first.
const UNDO_KEPT: usize = 20;
/// Bounds changes this close together are one step: a group drag ends with one per fence.
const SAME_GESTURE: Duration = Duration::from_millis(400);

pub(super) enum UndoStep {
    /// Fences moved or resized: where each one was.
    Bounds {
        at: Instant,
        places: Vec<(FenceId, UndoPlace)>,
    },
    /// Desktop icons moved into `to`: where each one was.
    Items {
        to: FenceId,
        places: Vec<(FenceId, pecofence_core::ItemRef)>,
    },
    /// A fence deleted from its menu.
    Deleted(Box<DeletedFence>),
}

/// Where a fence rested before a move or resize: its saved rect (device px), the end of it a
/// rolled title bar sat at, and the full height that rect was cut from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct UndoPlace {
    pub(super) rect: RECT,
    pub(super) bottom_anchored: bool,
    pub(super) full_h: i32,
}

/// How `put_fence_at` puts a fence back at an [`UndoPlace`].
#[derive(Debug, PartialEq)]
struct UndoGeometry {
    /// The rect the window is restored to, and the height it opens to.
    rect: RECT,
    full_h: i32,
    /// The end of `rect` its rolled title bar goes to; None leaves the window's own.
    kept_end: Option<bool>,
    /// Recorded at that end, without choosing a side again (`record_bounds_kept`).
    kept: bool,
}

/// [`UndoGeometry`] for `place`, as `fence move` places a fence: a top title row with 向上展开
/// rolled up goes to the bottom of the rect; a rolled bottom title row goes back to the end it
/// was saved at and keeps the full height its saved rect was cut from.
fn undo_geometry(
    place: &UndoPlace,
    edge: pecofence_core::TitleEdge,
    rolled: bool,
    expand_upward: bool,
) -> UndoGeometry {
    let h = place.rect.bottom - place.rect.top;
    if rolled && edge != pecofence_core::TitleEdge::Top {
        return UndoGeometry {
            rect: place.rect,
            full_h: place.full_h.max(h),
            kept_end: Some(place.bottom_anchored),
            kept: true,
        };
    }
    UndoGeometry {
        rect: place.rect,
        full_h: h,
        kept_end: (rolled && expand_upward).then_some(true),
        kept: false,
    }
}

impl UndoStep {
    fn kind(&self) -> &'static str {
        match self {
            Self::Bounds { .. } => "bounds",
            Self::Items { .. } => "items",
            Self::Deleted(_) => "deleted fence",
        }
    }
}

impl App {
    fn push_undo(&mut self, step: UndoStep) {
        tracing::info!(
            kind = step.kind(),
            depth = self.undo.len() + 1,
            "undo step noted"
        );
        self.undo.push(step);
        if self.undo.len() > UNDO_KEPT {
            self.undo.remove(0);
        }
    }

    /// The whole layout was replaced: no step applies to it any more.
    pub(super) fn clear_undo(&mut self, why: &'static str) {
        if !self.undo.is_empty() {
            tracing::info!(steps = self.undo.len(), why, "undo steps dropped");
            self.undo.clear();
        }
        self.undo_notice = None;
    }

    /// Where `fence` rests now, for Ctrl+Z after a move or resize.
    pub(super) fn undo_place(&self, fence: FenceId) -> Option<UndoPlace> {
        let f = self.state.fence(fence)?;
        let r = self.state.fence_px_rect(f);
        Some(UndoPlace {
            rect: RECT {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            },
            bottom_anchored: f.bottom_anchored,
            full_h: self.state.fence_expanded_px(f),
        })
    }

    /// A user move or resize of `fence` that left `before` (where it rested until then).
    pub(super) fn note_bounds_for_undo(&mut self, fence: FenceId, before: UndoPlace) {
        let now = Instant::now();
        if let Some(UndoStep::Bounds { at, places }) = self.undo.last_mut()
            && now.duration_since(*at) < SAME_GESTURE
            && !places.iter().any(|(f, _)| *f == fence)
        {
            places.push((fence, before));
            *at = now;
            return;
        }
        self.push_undo(UndoStep::Bounds {
            at: now,
            places: vec![(fence, before)],
        });
    }

    /// Desktop icons the user moved into `to` from `places` (see `AppState::item_places`).
    pub(super) fn note_items_for_undo(
        &mut self,
        to: FenceId,
        places: Vec<(FenceId, pecofence_core::ItemRef)>,
    ) {
        let places: Vec<_> = places.into_iter().filter(|(f, _)| *f != to).collect();
        if !places.is_empty() {
            self.push_undo(UndoStep::Items { to, places });
        }
    }

    pub(super) fn note_deleted_for_undo(&mut self, deleted: DeletedFence) {
        self.push_undo(UndoStep::Deleted(Box::new(deleted)));
    }

    /// Ctrl+Z: undoes the newest step.
    pub(super) fn undo_last(&mut self, from: &'static str) {
        let Some(step) = self.undo.pop() else {
            tracing::info!(from, "nothing to undo");
            return;
        };
        self.apply_undo(step, from);
    }

    /// The 撤销 of the 已删除栅栏 notice: puts that fence back, whatever came after it.
    pub(super) fn undo_delete_of(&mut self, fence: FenceId) {
        let Some(i) = self
            .undo
            .iter()
            .rposition(|s| matches!(s, UndoStep::Deleted(d) if d.fence.id == fence))
        else {
            tracing::info!(%fence, "deleted fence no longer undoable");
            return;
        };
        let step = self.undo.remove(i);
        self.apply_undo(step, "notice");
    }

    fn apply_undo(&mut self, step: UndoStep, from: &'static str) {
        let kind = step.kind();
        let done = match step {
            UndoStep::Bounds { places, .. } => {
                let mut n = 0;
                for (fence, place) in places {
                    n += usize::from(self.put_fence_at(fence, &place));
                }
                n
            }
            UndoStep::Items { to, places } => {
                let moved = self.state.put_items_back(&places, to);
                if moved > 0 {
                    let mut fences: Vec<FenceId> = places.iter().map(|(f, _)| *f).collect();
                    fences.push(to);
                    fences.dedup();
                    for f in fences {
                        self.refresh_fence(f);
                    }
                }
                moved
            }
            UndoStep::Deleted(d) => {
                let id = d.fence.id;
                let portal = d.fence.kind == FenceKind::FolderPortal;
                let back = self.state.undelete_fence(*d);
                if back {
                    self.resync_windows();
                    if portal {
                        self.refresh_portals();
                    }
                    self.refresh_all();
                    self.push_settings_state();
                }
                if self.undo_notice.as_ref().is_some_and(|(_, f)| *f == id) {
                    self.undo_notice = None;
                }
                usize::from(back)
            }
        };
        tracing::info!(kind, from, done, left = self.undo.len(), "undone");
        if done > 0 {
            self.schedule_save();
        }
    }

    /// Moves `fence` back to `place` the way `fence move` does. False when it is gone, a tab,
    /// or placed by 像桌面一样排列.
    fn put_fence_at(&mut self, fence: FenceId, place: &UndoPlace) -> bool {
        if self.state.fence(fence).is_none_or(|f| f.tab_host.is_some()) {
            return false;
        }
        let Some(w) = self.fences.get(&fence) else {
            return false;
        };
        if w.is_desktop_arranged() {
            return false;
        }
        let rolled = self
            .state
            .fence(fence)
            .map_or_else(|| w.is_rolled(), |f| f.rolled_up || w.is_rolled());
        self.ctx.behavior.release_push(w.hwnd());
        let (edge, upward) = self
            .state
            .fence(fence)
            .map_or((pecofence_core::TitleEdge::Top, false), |f| {
                (f.title_edge(), f.expand_upward)
            });
        let g = undo_geometry(place, edge, rolled, upward);
        if let Some(end) = g.kept_end {
            w.set_row_at_bottom(end);
        }
        w.restore_geometry(g.rect, rolled, g.full_h);
        if g.kept {
            let bar = w.rect();
            self.record_bounds_kept(fence, bar, true, g.full_h, "undo");
            self.apply_auto_height_kept(fence);
        } else {
            self.record_bounds(fence, g.rect, rolled, g.full_h, "undo");
            self.apply_auto_height(fence);
        }
        true
    }

    /// After a delete from the fence menu: 「已删除栅栏“X”」 with 撤销, unless another notice
    /// is up (Ctrl+Z still undoes it then).
    pub(super) fn offer_undo_delete(&mut self, fence: FenceId, title: &str) {
        if self.notice_showing() {
            tracing::info!("undo notice not shown: another notice is up");
            return;
        }
        self.undo_notice = None;
        let title = pecofence_core::i18n::format("已删除栅栏“{0}”", &[title.to_string()]);
        match crate::notice::Notice::show(
            &self.ctx,
            &title,
            pecofence_core::i18n::text("点「撤销」，或在栅栏上按 Ctrl+Z，就能把它找回来。"),
            pecofence_core::i18n::text("撤销"),
            crate::notice::locale(),
            &self.notice_avoid_rects(),
        ) {
            Ok(notice) => {
                tracing::info!(%fence, "undo notice shown");
                self.undo_notice = Some((notice, fence));
            }
            Err(error) => tracing::warn!(%error, "undo notice not shown"),
        }
    }

    /// The undo notice was clicked, closed or timed out.
    pub(super) fn on_undo_notice(&mut self, fence: FenceId, event: crate::notice::NoticeEvent) {
        tracing::info!(%fence, ?event, "undo notice answered");
        if event == crate::notice::NoticeEvent::Action {
            self.undo_delete_of(fence);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pecofence_core::TitleEdge;

    const BAR: i32 = 36;

    fn rect(top: i32, bottom: i32) -> RECT {
        RECT {
            left: 100,
            top,
            right: 400,
            bottom,
        }
    }

    /// Top of the rolled title bar the window shows at `g`: at the end `kept_end` names.
    fn bar_top(g: &UndoGeometry) -> i32 {
        let anchor = if g.kept_end == Some(true) { 1.0 } else { 0.0 };
        crate::fence_window::anchored_start(g.rect.top, g.rect.bottom - g.rect.top, BAR, anchor)
    }

    /// A rolled bottom row resting at 1000 (its saved rect 700..1036 opens upward) is dragged
    /// to 50, where it opens downward. Ctrl+Z puts its row back at 1000, not at 700.
    #[test]
    fn undo_puts_a_rolled_bottom_row_back_at_its_end() {
        let place = UndoPlace {
            rect: rect(700, 1036),
            bottom_anchored: true,
            full_h: 336,
        };
        let g = undo_geometry(&place, TitleEdge::Bottom, true, true);
        assert_eq!((g.kept_end, g.kept), (Some(true), true));
        assert_eq!(bar_top(&g), 1000);
        // Saved opening downward, it goes back to the top of its rect.
        let down = UndoPlace {
            bottom_anchored: false,
            ..place
        };
        assert_eq!(
            bar_top(&undo_geometry(&down, TitleEdge::Bottom, true, false)),
            700
        );
    }

    /// A saved rect cut to the room keeps the full height it opens to.
    #[test]
    fn undo_keeps_the_full_height_of_a_cut_rect() {
        let place = UndoPlace {
            rect: rect(700, 1036),
            bottom_anchored: true,
            full_h: 600,
        };
        assert_eq!(
            undo_geometry(&place, TitleEdge::Bottom, true, true).full_h,
            600
        );
    }

    /// A top title row goes back as `fence move` puts it: the rect's height, at the bottom of
    /// the rect only when rolled with 向上展开.
    #[test]
    fn undo_of_a_top_row_follows_fence_move() {
        let place = UndoPlace {
            rect: rect(700, 1036),
            bottom_anchored: true,
            full_h: 600,
        };
        let g = undo_geometry(&place, TitleEdge::Top, true, false);
        assert_eq!((g.full_h, g.kept_end, g.kept), (336, None, false));
        let g = undo_geometry(&place, TitleEdge::Top, true, true);
        assert_eq!((g.kept_end, bar_top(&g)), (Some(true), 1000));
        let g = undo_geometry(&place, TitleEdge::Top, false, true);
        assert_eq!((g.rect, g.full_h, g.kept_end), (place.rect, 336, None));
    }
}
