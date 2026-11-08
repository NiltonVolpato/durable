//! Generate serde glue for coroutine frames.
//!
//! For each coroutine `i`: `__ser_i(frame) -> Value` and `__de_i(&Value, frame)`.
//! Each suspension state becomes a JSON object
//! `{ "state": N, "sig": "...", "at": "file:line:col", <local>: <value>, ... }`.
//! Nested coroutine frames (the `__awaitee` of an `async fn` call) recurse.
//! `sig` lists the state's live locals and types; deserialize requires an
//! exact match, so a snapshot only loads into a build with the same state.

use crate::{Coroutine, Field, State};
use rustc_infer::infer::TyCtxtInferExt;
use rustc_middle::ty::print::{with_crate_prefix, with_no_trimmed_paths};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::def_id::DefId;
use rustc_trait_selection::infer::InferCtxtExt;
use std::collections::HashMap;
use std::fmt::Write as _;

const RT: &str = "::durable_poc::glue";

/// Why a value of this type cannot go through the glue, if it cannot.
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

/// How one field is handled.
pub enum Kind {
    Nested(usize),
    Serde(String),
    /// `[T; N]`: serde only covers N <= 32, so go through a slice.
    Array(String, u64),
}

/// Decides how (and whether) each live local can be serialized.
pub struct Planner<'tcx> {
    tcx: TyCtxt<'tcx>,
    ids: HashMap<DefId, usize>,
    serde: Option<(DefId, DefId)>,
}

impl<'tcx> Planner<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>, cos: &[Coroutine<'tcx>]) -> Self {
        let ids = cos.iter().enumerate().map(|(i, c)| (c.def_id, i)).collect();
        let ser = find_trait(tcx, &["serde", "serde_core"], "Serialize");
        let de = find_trait(tcx, &["serde", "serde_core"], "Deserialize");
        Planner { tcx, ids, serde: ser.zip(de) }
    }

    fn is_serde(&self, ty: Ty<'tcx>) -> bool {
        let Some((ser, de)) = self.serde else { return false };
        implements(self.tcx, ser, ty, &[]) && implements(self.tcx, de, ty, &[self.tcx.lifetimes.re_static.into()])
    }

    fn kind(&self, ty: Ty<'tcx>) -> Result<Kind, String> {
        if let ty::Coroutine(d, _) = ty.kind() {
            return match self.ids.get(d) {
                Some(i) => Ok(Kind::Nested(*i)),
                None => Err(format!("no glue for nested `{ty}`")),
            };
        }
        if let Some(why) = unsupported(ty) {
            return Err(why);
        }
        if self.serde.is_none() {
            return Err("serde is not in the crate graph".into());
        }
        if let ty::Array(elem, len) = ty.kind()
            && let Some(n) = len.try_to_target_usize(self.tcx)
            && self.is_serde(*elem)
        {
            return Ok(Kind::Array(path(*elem), n));
        }
        if !self.is_serde(ty) {
            return Err(format!("`{ty}` does not implement Serialize + DeserializeOwned"));
        }
        Ok(Kind::Serde(path(ty)))
    }

    /// Live values of a state: its saved locals, plus upvars for non-fn coroutines.
    pub fn live<'a>(&self, co: &'a Coroutine<'tcx>, st: &'a State<'tcx>) -> Vec<&'a Field<'tcx>> {
        let mut fields: Vec<_> = st.fields.iter().collect();
        if !co.is_fn_body {
            fields.extend(co.upvars.iter());
        }
        fields
    }

    /// How to handle every live value, or the first one that cannot be handled.
    pub fn plan<'a>(
        &self,
        co: &'a Coroutine<'tcx>,
        st: &'a State<'tcx>,
    ) -> Result<Vec<(&'a Field<'tcx>, Kind)>, (&'a Field<'tcx>, String)> {
        self.live(co, st)
            .into_iter()
            .map(|f| {
                if f.name.starts_with("_l") && f.ty.is_bool() {
                    return Err((f, "looks like a drop flag".to_string()));
                }
                self.kind(f.ty).map(|k| (f, k)).map_err(|e| (f, e))
            })
            .collect()
    }
}

/// The state's live locals and their types, sorted. Nested frames carry their own.
fn signature(plan: &[(&Field<'_>, Kind)]) -> String {
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

pub fn generate<'tcx>(tcx: TyCtxt<'tcx>, cos: &[Coroutine<'tcx>]) -> String {
    let planner = Planner::new(tcx, cos);
    let mut s = String::new();
    s.push_str("// @generated by durable-driver from this crate's coroutine layouts. Do not edit.\n\n");
    for (i, co) in cos.iter().enumerate() {
        let name = co.ty.to_string();
        let Some((tsize, toff)) = co.tag else {
            let _ = writeln!(s, "unsafe fn __ser_{i}(_: *const u8) -> {RT}::Result<{RT}::Value> {{ Err(\"{name}: no tag\".into()) }}");
            let _ = writeln!(s, "unsafe fn __de_{i}(_: &{RT}::Value, _: *mut u8) -> {RT}::Result<()> {{ Err(\"{name}: no tag\".into()) }}\n");
            continue;
        };
        let mut ser = String::new();
        let mut de = String::new();
        for st in co.states.iter().filter(|st| st.index >= 3) {
            let plan = match planner.plan(co, st) {
                Ok(p) => p,
                Err((f, why)) => {
                    let msg =
                        format!("{name}: state {} at {} is not serializable: {}: {why}", st.index, st.at, f.name);
                    let _ = writeln!(ser, "        {} => Err({msg:?}.into()),", st.index);
                    let _ = writeln!(de, "        {} => Err({msg:?}.into()),", st.index);
                    continue;
                }
            };
            let sig = signature(&plan);
            let _ = writeln!(ser, "        {} => {{", st.index);
            let _ = writeln!(ser, "            let mut m = {RT}::Map::new();");
            let _ = writeln!(ser, "            m.insert(\"state\".into(), {}u64.into());", st.index);
            let _ = writeln!(ser, "            m.insert(\"sig\".into(), {sig:?}.into());");
            let _ = writeln!(ser, "            m.insert(\"at\".into(), {:?}.into());", st.at);
            let _ = writeln!(de, "        {} => {{", st.index);
            let _ = writeln!(de, "            {RT}::check_sig(v, {sig:?})?;");
            for (f, k) in &plan {
                let (n, o) = (&f.name, f.offset);
                match k {
                    Kind::Nested(j) => {
                        let _ = writeln!(ser, "            m.insert({n:?}.into(), __ser_{j}(p.add({o}))?);");
                        let _ = writeln!(de, "            __de_{j}({RT}::field(v, {n:?})?, p.add({o}))?;");
                    }
                    Kind::Array(t, len) => {
                        let _ = writeln!(ser, "            m.insert({n:?}.into(), {RT}::ser_array::<{t}, {len}>(p.add({o}))?);");
                        let _ = writeln!(de, "            {RT}::de_array::<{t}, {len}>(v, {n:?}, p.add({o}))?;");
                    }
                    Kind::Serde(t) => {
                        let _ = writeln!(ser, "            m.insert({n:?}.into(), {RT}::ser_field::<{t}>(p.add({o}))?);");
                        let _ = writeln!(de, "            {RT}::de_field::<{t}>(v, {n:?}, p.add({o}))?;");
                    }
                }
            }
            let _ = writeln!(ser, "            Ok(m.into())\n        }}");
            let _ = writeln!(
                de,
                "            ::core::ptr::write(p.add({toff}) as *mut {}, {});\n            Ok(())\n        }}",
                uint(tsize),
                st.index
            );
        }
        let allow = "#[allow(unused_mut, unused_unsafe, unused_variables, unreachable_code)]";
        let _ = writeln!(s, "// {name}\n{allow}");
        let _ = writeln!(s, "unsafe fn __ser_{i}(p: *const u8) -> {RT}::Result<{RT}::Value> {{ unsafe {{");
        let _ = writeln!(s, "    match ::core::ptr::read(p.add({toff}) as *const {}) {{", uint(tsize));
        s.push_str(&ser);
        let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} is not a suspension point\", {name:?}, t)),");
        let _ = writeln!(s, "    }}\n}}}}\n{allow}");
        let _ = writeln!(s, "unsafe fn __de_{i}(v: &{RT}::Value, p: *mut u8) -> {RT}::Result<()> {{ unsafe {{");
        let _ = writeln!(s, "    match {RT}::state(v)? {{");
        s.push_str(&de);
        let _ = writeln!(s, "        t => Err(format!(\"{{}}: state {{}} is not a suspension point\", {name:?}, t)),");
        let _ = writeln!(s, "    }}\n}}}}\n");
    }
    let _ = writeln!(s, "pub static GLUE: &[{RT}::Entry] = &[");
    for (i, co) in cos.iter().enumerate() {
        let _ = writeln!(
            s,
            "    {RT}::Entry {{ type_name: {:?}, size: {}, align: {}, ser: __ser_{i}, de: __de_{i} }},",
            co.type_name, co.size, co.align
        );
    }
    s.push_str("];\n");
    s
}
