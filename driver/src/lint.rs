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
//!
//! Parking drops the in-memory frame, so a live value's destructor runs at
//! park and again on the rebuilt copy. A live value whose type has a user
//! `Drop` impl anywhere inside is rejected unless that type implements
//! `durable::DropOnPark`. std types and types trusted in the config
//! (`config.rs`) are not flagged, but their type arguments are checked.

use crate::glue::{self, Kind, Planner, Step};
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

/// Finds user `Drop` impls that would run when a task parks.
struct Drops<'a, 'tcx> {
    cx: &'a Ctx<'tcx>,
    frames: &'a [Frame<'tcx>],
    planner: &'a Planner<'a, 'tcx>,
    marker: DefId,
}

impl<'tcx> Drops<'_, 'tcx> {
    fn opted_in(&self, ty: Ty<'tcx>) -> bool {
        !ty.has_erased_regions() && !ty.has_escaping_bound_vars() && glue::implements(self.cx.tcx, self.marker, ty, &[])
    }

    /// The first type in `ty` with a user `Drop` impl and no opt-in.
    fn in_type(&self, ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
        let tcx = self.cx.tcx;
        glue::find_owned(self.cx, ty, &mut |t| match t.kind() {
            _ if !t.needs_drop(tcx, ty::TypingEnv::fully_monomorphized()) => Step::Skip,
            ty::Adt(def, _) if def.is_manually_drop() || self.opted_in(t) => Step::Skip,
            ty::Adt(def, _) if !self.cx.trusted(def.did()) && tcx.adt_destructor(def.did()).is_some() => Step::Found,
            _ => Step::Descend,
        })
    }

    /// The first value (by path) under `f` with a user `Drop` impl and no opt-in.
    /// A nested coroutine that was polled (under `__awaitee`) is checked at its
    /// own slow states; one that was not holds only its upvars.
    fn in_value(&self, f: &Field<'tcx>, polled: bool, depth: usize) -> Option<(String, Ty<'tcx>)> {
        let Some(j) = self.planner.id(f.ty) else {
            return self.in_type(f.ty).map(|t| (f.name.clone(), t));
        };
        if depth > 16 {
            return None;
        }
        let polled = polled || f.name == "__awaitee";
        let fields: Vec<&Field<'tcx>> = match &self.frames[j].shape {
            Shape::Coroutine { .. } if polled => return None,
            Shape::Coroutine { upvars, .. } => upvars.iter().collect(),
            Shape::Fields(fields) => {
                // A `Structural` struct with its own destructor.
                if let ty::Adt(def, _) = f.ty.kind()
                    && self.cx.tcx.adt_destructor(def.did()).is_some()
                    && !self.opted_in(f.ty)
                {
                    return Some((f.name.clone(), f.ty));
                }
                fields.iter().collect()
            }
        };
        fields.into_iter().find_map(|g| self.in_value(g, polled, depth + 1)).map(|(p, t)| (format!("{}.{p}", f.name), t))
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
    let drops = glue::find_trait(cx.tcx, &[cx.rt], "DropOnPark").map(|marker| Drops { cx, frames, planner, marker });
    let errors = std::cell::Cell::new(0);
    let drop_err = |st: &State<'tcx>, f: &Field<'tcx>, what: String, ty: Ty<'tcx>| {
        cx.tcx
            .dcx()
            .struct_span_err(st.span, format!("`{what}` has a destructor that would run when the task parks"))
            .with_span_label(st.span, "the task can park here")
            .with_span_note(
                f.span,
                format!(
                    "`{}` is live across the await; `{ty}` implements `Drop`, which runs when the parked task \
                     is dropped from memory and again when the resumed copy goes out of scope",
                    f.name
                ),
            )
            .with_help("end it before the await, or implement `durable::DropOnPark` for it if running its destructor at park is harmless")
            .emit();
        errors.set(errors.get() + 1);
    };
    let report = |st: &State<'tcx>, f: &Field<'tcx>, what: String, why: String| {
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
        errors.set(errors.get() + 1);
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
                        } else if let Some((what, ty)) = drops.as_ref().and_then(|d| d.in_value(f, false, 0)) {
                            drop_err(st, f, what, ty);
                        }
                    }
                }
            }
        }
    }
    errors.get()
}
