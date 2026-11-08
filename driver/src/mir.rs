//! Find which saved local each drop flag guards, from the coroutine's MIR.
//!
//! Pattern (after the coroutine transform):
//!   switchInt(copy ((*_n) as variant#V).F: bool) -> [0: bbA, otherwise: bbB]
//!   bbB: ...; [goto / switch on a discriminant ...;] drop(((*_n) as variant#W).G[.more])
//! where `_n` points at the coroutine. Then saved local (V, F) guards (W, G).
//! The drop may be of a field of G: a struct without a Drop impl is dropped
//! field by field ("open drop"), and an enum after a switch on its discriminant.

use rustc_middle::mir::{self, CoroutineLayout, CoroutineSavedLocal, Operand, Place, ProjectionElem, TerminatorKind};
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::def_id::DefId;

/// The saved local behind `((*_n) as variant#V).F`, if `_n` points at this coroutine.
fn saved_local<'tcx>(
    body: &mir::Body<'tcx>,
    def_id: DefId,
    cl: &CoroutineLayout<'tcx>,
    place: Place<'tcx>,
) -> Option<CoroutineSavedLocal> {
    let points_at_self = match body.local_decls[place.local].ty.kind() {
        ty::Ref(_, t, _) => matches!(t.kind(), ty::Coroutine(d, _) if *d == def_id),
        _ => false,
    };
    match place.projection.as_slice() {
        [ProjectionElem::Deref, ProjectionElem::Downcast(_, v), ProjectionElem::Field(f, _), ..] if points_at_self => {
            cl.variant_fields.get(*v)?.get(*f).copied()
        }
        _ => None,
    }
}

/// (flag, guarded value) pairs.
pub fn drop_flags<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    cl: &CoroutineLayout<'tcx>,
) -> Vec<(CoroutineSavedLocal, CoroutineSavedLocal)> {
    if !tcx.is_mir_available(def_id) {
        return vec![];
    }
    let body = tcx.optimized_mir(def_id);
    let mut out = vec![];
    for bb in body.basic_blocks.iter() {
        let TerminatorKind::SwitchInt { discr: Operand::Copy(p) | Operand::Move(p), targets } = &bb.terminator().kind
        else {
            continue;
        };
        let Some(flag) = saved_local(body, def_id, cl, *p) else { continue };
        // From the "initialized" branch, find the first drop of a saved local,
        // through gotos and discriminant switches.
        let mut queue = std::collections::VecDeque::from([targets.otherwise()]);
        let mut seen = std::collections::HashSet::new();
        while let Some(b) = queue.pop_front() {
            if !seen.insert(b) || seen.len() > 16 {
                continue;
            }
            let term = body.basic_blocks[b].terminator();
            if let TerminatorKind::Drop { place, .. } = &term.kind
                && let Some(val) = saved_local(body, def_id, cl, *place)
            {
                if val != flag && !out.contains(&(flag, val)) {
                    out.push((flag, val));
                }
                break;
            }
            if matches!(term.kind, TerminatorKind::Goto { .. } | TerminatorKind::SwitchInt { .. } | TerminatorKind::Drop { .. }) {
                queue.extend(term.successors());
            }
        }
    }
    out
}
