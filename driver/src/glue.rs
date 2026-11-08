//! Generate serde glue for workflow frames.
//!
//! For each frame `i`: `__ser_i(ptr) -> Value` and `__de_i(&Value, ptr)`.
//! - Coroutine: one JSON object per state,
//!   `{ "state": N, "sig": "...", "at": "file:line:col", <local>: <value>, ... }`.
//!   State 0 (unresumed) holds the upvars, state 1 (returned) nothing.
//! - Closure / `Structural` struct: `{ <field>: <value>, ... }`.
//! Values that are frames themselves recurse; the rest go through serde.
//! `sig` lists a state's live locals and types; deserialize requires an
//! exact match, so a snapshot only loads into a build with the same state.

use crate::{Ctx, Field, Frame, Shape, State};
use rustc_infer::infer::TyCtxtInferExt;
use rustc_middle::ty::print::{with_crate_prefix, with_no_trimmed_paths};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::def_id::DefId;
use rustc_trait_selection::infer::InferCtxtExt;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

static RT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Name the runtime crate the generated code calls into (`durable` or `durable_poc`).
pub fn set_runtime(krate: &str) {
    let _ = RT.set(format!("::{krate}::glue"));
}

fn rt() -> &'static str {
    RT.get().map(String::as_str).unwrap_or("::durable::glue")
}

/// Why a value of this type cannot go through serde, if it cannot.
fn unsupported(ty: Ty<'_>) -> Option<String> {
    for arg in ty.walk() {
        if arg.as_region().is_some() {
            return Some(format!("`{ty}` has lifetimes"));
        }
        let Some(t) = arg.as_type() else { continue };
        let why = match t.kind() {
            ty::Ref(..) => "holds a reference",
            ty::RawPtr(..) => "holds a raw pointer",
            ty::FnPtr(..) | ty::FnDef(..) => "holds a function",
            ty::Dynamic(..) => "holds a trait object",
            ty::Closure(..) | ty::CoroutineClosure(..) | ty::Coroutine(..) | ty::CoroutineWitness(..) => {
                "holds an unnameable type"
            }
            ty::Param(..) | ty::Alias(..) | ty::Foreign(..) | ty::Infer(..) | ty::Placeholder(..) | ty::Bound(..)
            | ty::Error(..) => "unsupported type",
            _ => continue,
        };
        return Some(format!("`{ty}` {why}"));
    }
    None
}

pub enum Step {
    Found,
    Skip,
    Descend,
}

/// Walk the types a value of `ty` owns: tuple and array elements, the type
/// arguments of std and trusted types (`Ctx::trusted`), the fields of other
/// ADTs (`PhantomData` owns nothing). Returns the first type `visit` reports as `Found`.
pub fn find_owned<'tcx>(cx: &Ctx<'tcx>, ty: Ty<'tcx>, visit: &mut impl FnMut(Ty<'tcx>) -> Step) -> Option<Ty<'tcx>> {
    fn go<'tcx>(
        cx: &Ctx<'tcx>,
        ty: Ty<'tcx>,
        visit: &mut impl FnMut(Ty<'tcx>) -> Step,
        seen: &mut HashSet<Ty<'tcx>>,
    ) -> Option<Ty<'tcx>> {
        if !seen.insert(ty) {
            return None;
        }
        match visit(ty) {
            Step::Found => return Some(ty),
            Step::Skip => return None,
            Step::Descend => {}
        }
        match ty.kind() {
            ty::Adt(def, _) if def.is_phantom_data() => None,
            ty::Adt(def, args) if cx.trusted(def.did()) => args.types().find_map(|t| go(cx, t, visit, seen)),
            ty::Adt(def, args) => {
                def.all_fields().find_map(|f| go(cx, cx.normalize(f.ty(cx.tcx, *args)), visit, seen))
            }
            ty::Tuple(ts) => ts.iter().find_map(|t| go(cx, t, visit, seen)),
            ty::Array(t, _) | ty::Slice(t) => go(cx, *t, visit, seen),
            _ => None,
        }
    }
    go(cx, ty, visit, &mut HashSet::new())
}

/// A type in `ty`, owned or behind owning pointers (`Box`, `Vec`, ...), with
/// interior mutability. `is_freeze` alone stops at those pointers.
fn mutable_inside<'tcx>(cx: &Ctx<'tcx>, ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
    let env = ty::TypingEnv::fully_monomorphized();
    find_owned(cx, ty, &mut |t| if t.is_freeze(cx.tcx, env) { Step::Descend } else { Step::Found })
}

/// Shared identity a snapshot loses. With serde's `rc` feature, `Rc` / `Arc`
/// serialize, but each resume gets a private copy: harmless if the pointee is
/// immutable, not if it can be mutated through the pointer. A deserialized
/// `Weak` is always empty.
fn lost_identity<'tcx>(cx: &Ctx<'tcx>, ty: Ty<'tcx>) -> Option<String> {
    use rustc_span::sym;
    let is = |did, items: &[rustc_span::Symbol]| items.iter().any(|s| cx.tcx.is_diagnostic_item(*s, did));
    let mut mutable = None;
    let found = find_owned(cx, ty, &mut |t| match t.kind() {
        ty::Adt(def, _) if is(def.did(), &[sym::RcWeak, sym::ArcWeak]) => Step::Found,
        ty::Adt(def, args) if is(def.did(), &[sym::Rc, sym::Arc]) => match mutable_inside(cx, args.type_at(0)) {
            Some(m) => {
                mutable = Some(m);
                Step::Found
            }
            None => Step::Descend,
        },
        _ => Step::Descend,
    })?;
    let what = if found == ty { format!("`{ty}`") } else { format!("`{ty}` holds `{found}`, which") };
    Some(match mutable {
        Some(m) => format!("{what} is shared and mutable through `{m}`; a stored copy would no longer be shared"),
        None => format!("{what} is a `Weak`; a stored one comes back empty"),
    })
}

fn path(ty: Ty<'_>) -> String {
    with_no_trimmed_paths!(with_crate_prefix!(ty.to_string()))
}

fn uint(bytes: u64) -> &'static str {
    match bytes {
        1 => "u8",
        2 => "u16",
        4 => "u32",
        _ => "u64",
    }
}

/// Find a trait by crate and item name anywhere in the crate graph.
pub fn find_trait(tcx: TyCtxt<'_>, crates: &[&str], name: &str) -> Option<DefId> {
    tcx.all_traits_including_private()
        .find(|did| crates.contains(&tcx.crate_name(did.krate).as_str()) && tcx.item_name(*did).as_str() == name)
}

/// Whether `ty: Trait<extra...>` holds (no generics in scope).
pub fn implements<'tcx>(tcx: TyCtxt<'tcx>, tr: DefId, ty: Ty<'tcx>, extra: &[ty::GenericArg<'tcx>]) -> bool {
    let infcx = tcx.infer_ctxt().build(ty::TypingMode::non_body_analysis());
    let args: Vec<ty::GenericArg<'tcx>> = std::iter::once(ty.into()).chain(extra.iter().copied()).collect();
    infcx.type_implements_trait(tr, args, ty::ParamEnv::empty()).must_apply_modulo_regions()
}

/// How one value is handled.
pub enum Kind {
    /// Another frame, by index.
    Nested(usize),
    Serde(String),
    /// `[T; N]`: serde only covers N <= 32, so go through a slice.
    Array(String, u64),
}

pub type Plan<'a, 'tcx> = Vec<(&'a Field<'tcx>, Kind)>;

/// Decides how (and whether) each stored value can be serialized.
pub struct Planner<'a, 'tcx> {
    cx: &'a Ctx<'tcx>,
    frames: &'a [Frame<'tcx>],
    ids: HashMap<Ty<'tcx>, usize>,
    serde: Option<(DefId, DefId)>,
}

impl<'a, 'tcx> Planner<'a, 'tcx> {
    pub fn new(cx: &'a Ctx<'tcx>, frames: &'a [Frame<'tcx>]) -> Self {
        let ids = frames.iter().enumerate().map(|(i, f)| (f.ty, i)).collect();
        let ser = find_trait(cx.tcx, &["serde", "serde_core"], "Serialize");
        let de = find_trait(cx.tcx, &["serde", "serde_core"], "Deserialize");
        Planner { cx, frames, ids, serde: ser.zip(de) }
    }

    pub fn id(&self, ty: Ty<'tcx>) -> Option<usize> {
        self.ids.get(&ty).copied()
    }

    fn is_serde(&self, ty: Ty<'tcx>) -> bool {
        let tcx = self.cx.tcx;
        let Some((ser, de)) = self.serde else { return false };
        implements(tcx, ser, ty, &[]) && implements(tcx, de, ty, &[tcx.lifetimes.re_static.into()])
    }

    fn kind(&self, ty: Ty<'tcx>) -> Result<Kind, String> {
        if let Some(i) = self.id(ty) {
            return Ok(Kind::Nested(i));
        }
        if self.cx.is_frame(ty) {
            return Err(format!("no layout for `{ty}`"));
        }
        if let Some(why) = unsupported(ty) {
            return Err(why);
        }
        // The glue is included at the crate root and names types by path.
        let tcx = self.cx.tcx;
        for t in ty.walk().filter_map(|a| a.as_type()) {
            if let ty::Adt(def, _) = t.kind()
                && !tcx.visibility(def.did()).is_accessible_from(rustc_span::def_id::CRATE_DEF_ID, tcx)
            {
                return Err(format!(
                    "`{}` is private to its module; the generated glue (at the crate root) cannot name it: make it `pub(crate)`",
                    tcx.def_path_str(def.did())
                ));
            }
        }
        if self.serde.is_none() {
            return Err("serde is not in the crate graph".into());
        }
        let kind = match ty.kind() {
            ty::Array(elem, len) if self.is_serde(*elem) && len.try_to_target_usize(self.cx.tcx).is_some() => {
                Kind::Array(path(*elem), len.try_to_target_usize(self.cx.tcx).unwrap())
            }
            _ if self.is_serde(ty) => Kind::Serde(path(ty)),
            _ => return Err(format!("`{ty}` does not implement Serialize + DeserializeOwned")),
        };
        // After serde: a type without serde impls gets that error instead.
        if let Some(why) = lost_identity(self.cx, ty) {
            return Err(why);
        }
        if let Some(h) = find_owned(self.cx, ty, &mut |t| if self.cx.process_local(t) { Step::Found } else { Step::Descend }) {
            let what = if h == ty { format!("`{ty}`") } else { format!("`{ty}` holds `{h}`, which") };
            return Err(format!("{what} is a handle valid only in the process that created it"));
        }
        Ok(kind)
    }

    /// Values stored in a coroutine state; None for the poisoned state.
    pub fn state_values<'b>(&self, fr: &'b Frame<'tcx>, st: &'b State<'tcx>) -> Option<Vec<&'b Field<'tcx>>> {
        let Shape::Coroutine { is_fn_body, upvars, .. } = &fr.shape else { return None };
        match st.index {
            0 => Some(upvars.iter().collect()),
            1 => Some(vec![]),
            2 => None,
            _ => {
                let mut v: Vec<_> = st.fields.iter().collect();
                if !is_fn_body {
                    v.extend(upvars.iter());
                }
                Some(v)
            }
        }
    }

    pub fn plan<'b>(&self, values: Vec<&'b Field<'tcx>>) -> Result<Plan<'b, 'tcx>, (&'b Field<'tcx>, String)> {
        values
            .into_iter()
            .map(|f| {
                if f.name.starts_with("_l") && f.ty.is_bool() && !f.name.ends_with(".init") {
                    return Err((f, "looks like a drop flag".to_string()));
                }
                self.kind(f.ty).map(|k| (f, k)).map_err(|e| (f, e))
            })
            .collect()
    }

    /// For closures and structs: every field, recursively, must be serializable.
    /// Coroutines are checked per state elsewhere. Returns the failing path.
    pub fn deep_check(&self, i: usize, depth: usize) -> Result<(), String> {
        let Shape::Fields(fields) = &self.frames[i].shape else { return Ok(()) };
        if depth > 16 {
            return Ok(());
        }
        let plan = self.plan(fields.iter().collect()).map_err(|(f, why)| format!("{}: {why}", f.name))?;
        for (f, k) in plan {
            if let Kind::Nested(j) = k {
                self.deep_check(j, depth + 1).map_err(|e| format!("{}.{e}", f.name))?;
            }
        }
        Ok(())
    }
}

/// The values and their types, sorted. Nested frames carry their own.
fn signature<'tcx>(plan: &Plan<'_, 'tcx>) -> String {
    let mut parts: Vec<String> = plan
        .iter()
        .map(|(f, k)| match k {
            Kind::Nested(_) => format!("{}:<frame>", f.name),
            Kind::Serde(t) => format!("{}:{t}", f.name),
            Kind::Array(t, n) => format!("{}:[{t}; {n}]", f.name),
        })
        .collect();
    parts.sort();
    parts.join(",")
}

/// Statements that put / take each value of a plan (`m` is the map, `v` the value).
fn put_take<'tcx>(plan: &Plan<'_, 'tcx>, ser: &mut String, de: &mut String, indent: &str) {
    let rt = rt();
    for (f, k) in plan {
        let (n, o) = (&f.name, f.offset);
        let (put, take) = match k {
            Kind::Nested(j) => (format!("__ser_{j}(p.add({o}))?"), format!("__de_{j}({rt}::field(v, {n:?})?, p.add({o}))?")),
            Kind::Array(t, len) => (
                format!("{rt}::ser_array::<{t}, {len}>(p.add({o}))?"),
                format!("{rt}::de_array::<{t}, {len}>(v, {n:?}, p.add({o}))?"),
            ),
            Kind::Serde(t) => {
                (format!("{rt}::ser_field::<{t}>(p.add({o}))?"), format!("{rt}::de_field::<{t}>(v, {n:?}, p.add({o}))?"))
            }
        };
        match f.flag {
            // Drop-flag guarded: present only while initialized.
            Some(fo) => {
                let _ = writeln!(ser, "{indent}if ::core::ptr::read(p.add({fo}) as *const bool) {{ m.insert({n:?}.into(), {put}); }}");
                let _ = writeln!(de, "{indent}if {rt}::field(v, {:?})?.as_bool() == Some(true) {{ {take}; }}", format!("{n}.init"));
            }
            None => {
                let _ = writeln!(ser, "{indent}m.insert({n:?}.into(), {put});");
                let _ = writeln!(de, "{indent}{take};");
            }
        }
    }
}

const ALLOW: &str = "#[allow(unused_mut, unused_unsafe, unused_variables, unreachable_code)]";

fn coroutine_glue<'tcx>(i: usize, fr: &Frame<'tcx>, planner: &Planner<'_, 'tcx>, s: &mut String) {
    let rt = rt();
    let name = fr.ty.to_string();
    let Shape::Coroutine { tag: Some((tsize, toff)), states, .. } = &fr.shape else {
        let _ = writeln!(s, "unsafe fn __ser_{i}(_: *const u8) -> {rt}::Result<{rt}::Value> {{ Err({:?}.into()) }}", format!("{name}: no tag"));
        let _ = writeln!(s, "unsafe fn __de_{i}(_: &{rt}::Value, _: *mut u8) -> {rt}::Result<()> {{ Err({:?}.into()) }}\n", format!("{name}: no tag"));
        return;
    };
    let (mut ser, mut de) = (String::new(), String::new());
    for st in states {
        let Some(values) = planner.state_values(fr, st) else { continue };
        let plan = match planner.plan(values) {
            Ok(p) => p,
            Err((f, why)) => {
                let msg = format!("{name}: state {} at {} is not serializable: {}: {why}", st.index, st.at, f.name);
                let _ = writeln!(ser, "        {} => Err({msg:?}.into()),", st.index);
                let _ = writeln!(de, "        {} => Err({msg:?}.into()),", st.index);
                continue;
            }
        };
        let sig = signature(&plan);
        let _ = writeln!(ser, "        {} => {{\n            let mut m = {rt}::Map::new();", st.index);
        let _ = writeln!(ser, "            m.insert(\"state\".into(), {}u64.into());", st.index);
        let _ = writeln!(ser, "            m.insert(\"sig\".into(), {sig:?}.into());");
        let _ = writeln!(ser, "            m.insert(\"at\".into(), {:?}.into());", st.at);
        let _ = writeln!(de, "        {} => {{\n            {rt}::check_sig(v, {sig:?})?;", st.index);
        put_take(&plan, &mut ser, &mut de, "            ");
        let _ = writeln!(ser, "            Ok(m.into())\n        }}");
        let _ = writeln!(de, "            ::core::ptr::write(p.add({toff}) as *mut {}, {});\n            Ok(())\n        }}", uint(*tsize), st.index);
    }
    let _ = writeln!(s, "// {name}\n{ALLOW}");
    let _ = writeln!(s, "unsafe fn __ser_{i}(p: *const u8) -> {rt}::Result<{rt}::Value> {{ unsafe {{");
    let _ = writeln!(s, "    match ::core::ptr::read(p.add({toff}) as *const {}) {{", uint(*tsize));
    s.push_str(&ser);
    let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} cannot be serialized\", {name:?}, t)),\n    }}\n}}}}");
    let _ = writeln!(s, "{ALLOW}\nunsafe fn __de_{i}(v: &{rt}::Value, p: *mut u8) -> {rt}::Result<()> {{ unsafe {{");
    let _ = writeln!(s, "    match {rt}::state(v)? {{");
    s.push_str(&de);
    let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} cannot be deserialized\", {name:?}, t)),\n    }}\n}}}}\n");
}

fn fields_glue<'tcx>(i: usize, fr: &Frame<'tcx>, planner: &Planner<'_, 'tcx>, s: &mut String) {
    let rt = rt();
    let Shape::Fields(fields) = &fr.shape else { return };
    let name = fr.ty.to_string();
    let _ = writeln!(s, "// {name}\n{ALLOW}");
    match planner.plan(fields.iter().collect()) {
        Err((f, why)) => {
            let msg = format!("{name}: {}: {why}", f.name);
            let _ = writeln!(s, "unsafe fn __ser_{i}(_: *const u8) -> {rt}::Result<{rt}::Value> {{ Err({msg:?}.into()) }}\n{ALLOW}");
            let _ = writeln!(s, "unsafe fn __de_{i}(_: &{rt}::Value, _: *mut u8) -> {rt}::Result<()> {{ Err({msg:?}.into()) }}\n");
        }
        Ok(plan) => {
            let (mut ser, mut de) = (String::new(), String::new());
            put_take(&plan, &mut ser, &mut de, "    ");
            let _ = writeln!(s, "unsafe fn __ser_{i}(p: *const u8) -> {rt}::Result<{rt}::Value> {{ unsafe {{\n    let mut m = {rt}::Map::new();");
            let _ = writeln!(s, "{ser}    Ok(m.into())\n}}}}\n{ALLOW}");
            let _ = writeln!(s, "unsafe fn __de_{i}(v: &{rt}::Value, p: *mut u8) -> {rt}::Result<()> {{ unsafe {{\n{de}    Ok(())\n}}}}\n");
        }
    }
}

pub fn generate<'tcx>(frames: &[Frame<'tcx>], planner: &Planner<'_, 'tcx>) -> String {
    let rt = rt();
    let mut s = String::from("// @generated by durable-driver from this crate's workflow frames. Do not edit.\n\n");
    for (i, fr) in frames.iter().enumerate() {
        match fr.shape {
            Shape::Coroutine { .. } => coroutine_glue(i, fr, planner, &mut s),
            Shape::Fields(_) => fields_glue(i, fr, planner, &mut s),
        }
    }
    let _ = writeln!(s, "pub static GLUE: &[{rt}::Entry] = &[");
    for (i, fr) in frames.iter().enumerate().filter(|(_, f)| f.root) {
        let _ = writeln!(
            s,
            "    {rt}::Entry {{ type_name: {:?}, size: {}, align: {}, ser: __ser_{i}, de: __de_{i} }},",
            fr.type_name, fr.size, fr.align
        );
    }
    s.push_str("];\n");
    s
}
