//! Find which saved local each drop flag guards, from the coroutine's MIR.
//!
//! Pattern (after the coroutine transform):
//!   switchInt(copy ((*_n) as variant#V).F: bool) -> [0: bbA, otherwise: bbB]
//!   bbB: ...; [goto ...;] drop(((*_n) as variant#W).G)
//! where `_n` points at the coroutine. Then saved local (V, F) guards (W, G).

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
        [ProjectionElem::Deref, ProjectionElem::Downcast(_, v), ProjectionElem::Field(f, _)] if points_at_self => {
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
        // Follow gotos from the "initialized" branch to the drop.
        let mut next = targets.otherwise();
        for _ in 0..4 {
            match &body.basic_blocks[next].terminator().kind {
                TerminatorKind::Goto { target, .. } => next = *target,
                TerminatorKind::Drop { place, .. } => {
                    if let Some(val) = saved_local(body, def_id, cl, *place)
                        && !out.contains(&(flag, val))
                    {
                        out.push((flag, val));
                    }
                    break;
                }
                _ => break,
            }
        }
    }
    out
}
