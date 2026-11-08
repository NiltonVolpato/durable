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
use std::collections::HashMap;
use std::fmt::Write as _;

const RT: &str = "::durable_poc::glue";

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
        if self.serde.is_none() {
            return Err("serde is not in the crate graph".into());
        }
        if let ty::Array(elem, len) = ty.kind()
            && let Some(n) = len.try_to_target_usize(self.cx.tcx)
            && self.is_serde(*elem)
        {
            return Ok(Kind::Array(path(*elem), n));
        }
        if !self.is_serde(ty) {
            return Err(format!("`{ty}` does not implement Serialize + DeserializeOwned"));
        }
        Ok(Kind::Serde(path(ty)))
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
    for (f, k) in plan {
        let (n, o) = (&f.name, f.offset);
        let (put, take) = match k {
            Kind::Nested(j) => (format!("__ser_{j}(p.add({o}))?"), format!("__de_{j}({RT}::field(v, {n:?})?, p.add({o}))?")),
            Kind::Array(t, len) => (
                format!("{RT}::ser_array::<{t}, {len}>(p.add({o}))?"),
                format!("{RT}::de_array::<{t}, {len}>(v, {n:?}, p.add({o}))?"),
            ),
            Kind::Serde(t) => {
                (format!("{RT}::ser_field::<{t}>(p.add({o}))?"), format!("{RT}::de_field::<{t}>(v, {n:?}, p.add({o}))?"))
            }
        };
        match f.flag {
            // Drop-flag guarded: present only while initialized.
            Some(fo) => {
                let _ = writeln!(ser, "{indent}if ::core::ptr::read(p.add({fo}) as *const bool) {{ m.insert({n:?}.into(), {put}); }}");
                let _ = writeln!(de, "{indent}if {RT}::field(v, {:?})?.as_bool() == Some(true) {{ {take}; }}", format!("{n}.init"));
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
    let name = fr.ty.to_string();
    let Shape::Coroutine { tag: Some((tsize, toff)), states, .. } = &fr.shape else {
        let _ = writeln!(s, "unsafe fn __ser_{i}(_: *const u8) -> {RT}::Result<{RT}::Value> {{ Err({:?}.into()) }}", format!("{name}: no tag"));
        let _ = writeln!(s, "unsafe fn __de_{i}(_: &{RT}::Value, _: *mut u8) -> {RT}::Result<()> {{ Err({:?}.into()) }}\n", format!("{name}: no tag"));
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
        let _ = writeln!(ser, "        {} => {{\n            let mut m = {RT}::Map::new();", st.index);
        let _ = writeln!(ser, "            m.insert(\"state\".into(), {}u64.into());", st.index);
        let _ = writeln!(ser, "            m.insert(\"sig\".into(), {sig:?}.into());");
        let _ = writeln!(ser, "            m.insert(\"at\".into(), {:?}.into());", st.at);
        let _ = writeln!(de, "        {} => {{\n            {RT}::check_sig(v, {sig:?})?;", st.index);
        put_take(&plan, &mut ser, &mut de, "            ");
        let _ = writeln!(ser, "            Ok(m.into())\n        }}");
        let _ = writeln!(de, "            ::core::ptr::write(p.add({toff}) as *mut {}, {});\n            Ok(())\n        }}", uint(*tsize), st.index);
    }
    let _ = writeln!(s, "// {name}\n{ALLOW}");
    let _ = writeln!(s, "unsafe fn __ser_{i}(p: *const u8) -> {RT}::Result<{RT}::Value> {{ unsafe {{");
    let _ = writeln!(s, "    match ::core::ptr::read(p.add({toff}) as *const {}) {{", uint(*tsize));
    s.push_str(&ser);
    let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} cannot be serialized\", {name:?}, t)),\n    }}\n}}}}");
    let _ = writeln!(s, "{ALLOW}\nunsafe fn __de_{i}(v: &{RT}::Value, p: *mut u8) -> {RT}::Result<()> {{ unsafe {{");
    let _ = writeln!(s, "    match {RT}::state(v)? {{");
    s.push_str(&de);
    let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} cannot be deserialized\", {name:?}, t)),\n    }}\n}}}}\n");
}

fn fields_glue<'tcx>(i: usize, fr: &Frame<'tcx>, planner: &Planner<'_, 'tcx>, s: &mut String) {
    let Shape::Fields(fields) = &fr.shape else { return };
    let name = fr.ty.to_string();
    let _ = writeln!(s, "// {name}\n{ALLOW}");
    match planner.plan(fields.iter().collect()) {
        Err((f, why)) => {
            let msg = format!("{name}: {}: {why}", f.name);
            let _ = writeln!(s, "unsafe fn __ser_{i}(_: *const u8) -> {RT}::Result<{RT}::Value> {{ Err({msg:?}.into()) }}\n{ALLOW}");
            let _ = writeln!(s, "unsafe fn __de_{i}(_: &{RT}::Value, _: *mut u8) -> {RT}::Result<()> {{ Err({msg:?}.into()) }}\n");
        }
        Ok(plan) => {
            let (mut ser, mut de) = (String::new(), String::new());
            put_take(&plan, &mut ser, &mut de, "    ");
            let _ = writeln!(s, "unsafe fn __ser_{i}(p: *const u8) -> {RT}::Result<{RT}::Value> {{ unsafe {{\n    let mut m = {RT}::Map::new();");
            let _ = writeln!(s, "{ser}    Ok(m.into())\n}}}}\n{ALLOW}");
            let _ = writeln!(s, "unsafe fn __de_{i}(v: &{RT}::Value, p: *mut u8) -> {RT}::Result<()> {{ unsafe {{\n{de}    Ok(())\n}}}}\n");
        }
    }
}

pub fn generate<'tcx>(frames: &[Frame<'tcx>], planner: &Planner<'_, 'tcx>) -> String {
    let mut s = String::from("// @generated by durable-driver from this crate's workflow frames. Do not edit.\n\n");
    for (i, fr) in frames.iter().enumerate() {
        match fr.shape {
            Shape::Coroutine { .. } => coroutine_glue(i, fr, planner, &mut s),
            Shape::Fields(_) => fields_glue(i, fr, planner, &mut s),
        }
    }
    let _ = writeln!(s, "pub static GLUE: &[{RT}::Entry] = &[");
    for (i, fr) in frames.iter().enumerate().filter(|(_, f)| f.root) {
        let _ = writeln!(
            s,
            "    {RT}::Entry {{ type_name: {:?}, size: {}, align: {}, ser: __ser_{i}, de: __de_{i} }},",
            fr.type_name, fr.size, fr.align
        );
    }
    s.push_str("];\n");
    s
}
