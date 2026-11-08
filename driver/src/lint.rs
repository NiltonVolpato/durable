//! Compile-time check: at every slow await, every live value must be
//! serializable by the glue. A state is slow if its awaited future can reach
//! a type implementing `durable_poc::SlowLeaf` (directly, as a generic
//! argument, or through nested coroutines). Workflows marked
//! `#[durable::raw]` use the raw-copy path and are skipped.

use crate::glue::{self, Planner};
use crate::{Coroutine, State};
use rustc_middle::ty::{self, TyCtxt, TypeVisitableExt};
use rustc_span::Symbol;
use rustc_span::def_id::DefId;
use std::collections::HashSet;

fn is_raw(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    let root = tcx.typeck_root_def_id(def_id);
    let path = [Symbol::intern("durable"), Symbol::intern("raw")];
    tcx.tool_attrs(root, &path).next().is_some()
}

fn awaits_slow<'tcx>(tcx: TyCtxt<'tcx>, leaf: DefId, slow: &HashSet<DefId>, st: &State<'tcx>) -> bool {
    st.fields.iter().filter(|f| f.name == "__awaitee").any(|f| {
        f.ty.walk().filter_map(|a| a.as_type()).any(|t| match t.kind() {
            ty::Coroutine(d, _) => slow.contains(d),
            ty::Adt(..) if !t.has_erased_regions() => glue::implements(tcx, leaf, t, &[]),
            _ => false,
        })
    })
}

/// Coroutines that can park on a slow leaf (fixpoint over nesting).
fn slow_coroutines<'tcx>(tcx: TyCtxt<'tcx>, leaf: DefId, cos: &[Coroutine<'tcx>]) -> HashSet<DefId> {
    let mut slow = HashSet::new();
    loop {
        let before = slow.len();
        for co in cos {
            if !slow.contains(&co.def_id) && co.states.iter().any(|st| awaits_slow(tcx, leaf, &slow, st)) {
                slow.insert(co.def_id);
            }
        }
        if slow.len() == before {
            return slow;
        }
    }
}

/// Emit an error for each slow await that holds something unserializable.
/// Returns the number of errors.
pub fn check<'tcx>(tcx: TyCtxt<'tcx>, cos: &[Coroutine<'tcx>], planner: &Planner<'tcx>) -> usize {
    let Some(leaf) = glue::find_trait(tcx, &["durable_poc"], "SlowLeaf") else { return 0 };
    let slow = slow_coroutines(tcx, leaf, cos);
    let mut errors = 0;
    for co in cos.iter().filter(|co| !is_raw(tcx, co.def_id)) {
        for st in co.states.iter().filter(|st| st.index >= 3 && awaits_slow(tcx, leaf, &slow, st)) {
            if let Err((f, why)) = planner.plan(co, st) {
                tcx.dcx()
                    .struct_span_err(st.span, format!("`{}` cannot be serialized at this slow await", f.name))
                    .with_span_label(st.span, "the task can park here")
                    .with_span_note(f.span, format!("`{}` is live across the await: {why}", f.name))
                    .with_help(
                        "make it serializable, end it before the await, or mark the workflow `#[durable::raw]`",
                    )
                    .emit();
                errors += 1;
            }
        }
    }
    errors
}
