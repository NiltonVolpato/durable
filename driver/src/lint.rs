//! Compile-time check: at every slow await reachable from a workflow root,
//! every live value must be serializable by the glue.
//!
//! A frame can park if it awaits a type implementing `durable::SlowLeaf`,
//! or another frame that can park (a coroutine, or a `Structural` combinator
//! with such a field). Computed as a fixpoint over the collected frames.
//! A type the driver cannot see into counts as slow if any type nested in it
//! is a slow leaf or a frame that can park (`Timeout<Request<_>>`,
//! `tokio::join!`'s `PollFn<{closure}>`, `Ephemeral<{async block}>`).
//! Type-erased futures (`Pin<Box<dyn Future>>`) count as fast: a slow leaf
//! hidden there is only caught at run time, when serialization fails.

use crate::glue::{self, Kind, Planner};
use crate::{Ctx, Field, Frame, Shape, State};
use rustc_middle::ty::{self, Ty, TypeVisitableExt};
use rustc_span::def_id::DefId;

fn awaitee<'a, 'tcx>(st: &'a State<'tcx>) -> Option<&'a Field<'tcx>> {
    st.fields.iter().find(|f| f.name == "__awaitee")
}

struct Slow<'a, 'tcx> {
    cx: &'a Ctx<'tcx>,
    planner: &'a Planner<'a, 'tcx>,
    leaf: DefId,
    slow: Vec<bool>,
}

impl<'tcx> Slow<'_, 'tcx> {
    fn can_park(&self, ty: Ty<'tcx>) -> bool {
        if let Some(i) = self.planner.id(ty) {
            return self.slow[i];
        }
        // A type the driver cannot see into (`Timeout<F>`, `PollFn<{closure}>`,
        // `Ephemeral<F>`, a tuple behind `&mut`): it can park if anything
        // nested in it is a slow leaf or a frame that can park.
        ty.walk().filter_map(|a| a.as_type()).any(|t| match self.planner.id(t) {
            Some(i) => self.slow[i],
            None => {
                matches!(t.kind(), ty::Adt(..))
                    && !t.has_erased_regions()
                    && !t.has_escaping_bound_vars()
                    && glue::implements(self.cx.tcx, self.leaf, t, &[])
            }
        })
    }

    fn parks_at(&self, st: &State<'tcx>) -> bool {
        st.index >= 3 && awaitee(st).is_some_and(|f| self.can_park(f.ty))
    }

    fn frame_parks(&self, fr: &Frame<'tcx>) -> bool {
        match &fr.shape {
            Shape::Coroutine { states, .. } => states.iter().any(|st| self.parks_at(st)),
            Shape::Fields(fields) => fields.iter().any(|f| self.can_park(f.ty)),
        }
    }
}

/// Emit an error for each slow await that holds something unserializable.
/// Returns the number of errors.
pub fn check<'tcx>(cx: &Ctx<'tcx>, frames: &[Frame<'tcx>], planner: &Planner<'_, 'tcx>) -> usize {
    let Some(leaf) = glue::find_trait(cx.tcx, &[cx.rt], "SlowLeaf") else { return 0 };
    let mut s = Slow { cx, planner, leaf, slow: vec![false; frames.len()] };
    loop {
        let mut changed = false;
        for (i, fr) in frames.iter().enumerate() {
            if !s.slow[i] && s.frame_parks(fr) {
                s.slow[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut errors = 0;
    let mut report = |st: &State<'tcx>, f: &Field<'tcx>, what: String, why: String| {
        let mut d = cx
            .tcx
            .dcx()
            .struct_span_err(st.span, format!("`{what}` cannot be serialized at this slow await"))
            .with_span_label(st.span, "the task can park here")
            .with_span_note(f.span, format!("`{}` is live across the await: {why}", f.name))
            .with_help("make it serializable, or end it before the await");
        if f.ty.needs_drop(cx.tcx, ty::TypingEnv::fully_monomorphized()) {
            d = d.with_help(format!(
                "values with a destructor stay live until the end of their scope; \
                 if `{}` is not used after this point, `drop` it or close its block before the await",
                f.name
            ));
        }
        d.emit();
        errors += 1;
    };
    for fr in frames {
        let Shape::Coroutine { states, .. } = &fr.shape else { continue };
        for st in states.iter().filter(|st| s.parks_at(st)) {
            let Some(values) = planner.state_values(fr, st) else { continue };
            match planner.plan(values) {
                Err((f, why)) => report(st, f, f.name.clone(), why),
                Ok(plan) => {
                    for (f, k) in &plan {
                        if let Kind::Nested(j) = k
                            && let Err(e) = planner.deep_check(*j, 0)
                        {
                            report(st, f, format!("{}.{e}", f.name).split(':').next().unwrap().to_string(), e);
                        }
                    }
                }
            }
        }
    }
    errors
}
