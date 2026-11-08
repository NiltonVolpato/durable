//! durable-driver: rustc plus a pass over coroutine layouts.
//!
//! Use as `RUSTC_WORKSPACE_WRAPPER`. Only crates named in `DURABLE_CRATES`
//! (comma-separated) are processed. Outputs (each optional):
//! - `DURABLE_DUMP=path`: human-readable layout dump.
//! - `DURABLE_GLUE_OUT=path`: generated serde glue (pass 1).
//! - `DURABLE_GLUE_CHECK=path`: fail the build if regenerated glue differs
//!   from this file (pass 2, which includes it).

#![feature(rustc_private)]

extern crate rustc_abi;
extern crate rustc_const_eval;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_infer;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_trait_selection;

mod glue;

use rustc_abi::Variants;
use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::{self, Ty, TyCtxt, TypeVisitableExt};
use rustc_span::def_id::{DefId, LOCAL_CRATE};
use std::fmt::Write as _;

/// One live local in one suspension state.
pub struct Field<'tcx> {
    pub name: String,
    pub ty: Ty<'tcx>,
    pub offset: u64,
    pub local: usize,
}

pub struct State<'tcx> {
    pub index: usize,
    pub at: String,
    pub fields: Vec<Field<'tcx>>,
}

pub struct Coroutine<'tcx> {
    pub def_id: DefId,
    pub ty: Ty<'tcx>,
    pub type_name: String,
    pub size: u64,
    pub align: u64,
    /// True for `async fn` bodies: upvars are moved into locals on first resume.
    pub is_fn_body: bool,
    pub upvars: Vec<Field<'tcx>>,
    /// (size in bytes, offset), Direct encoding only.
    pub tag: Option<(u64, u64)>,
    pub states: Vec<State<'tcx>>,
}

fn collect<'tcx>(tcx: TyCtxt<'tcx>) -> Vec<Coroutine<'tcx>> {
    let mut out = vec![];
    for def_id in tcx.hir_body_owners() {
        let def_id = def_id.to_def_id();
        if !tcx.is_coroutine(def_id) {
            continue;
        }
        let ty = tcx.type_of(def_id).instantiate_identity().skip_normalization();
        let ty::Coroutine(_, args) = ty.kind() else { continue };
        if ty.has_param() {
            continue; // generic coroutines: not handled yet
        }
        let Ok(cl) = tcx.coroutine_layout(def_id, *args) else { continue };
        let Ok(layout) = tcx.layout_of(ty::TypingEnv::fully_monomorphized().as_query_input(ty)) else {
            continue;
        };
        let is_fn_body = matches!(
            tcx.coroutine_kind(def_id),
            Some(rustc_hir::CoroutineKind::Desugared(_, rustc_hir::CoroutineSource::Fn))
        );
        let upvars = args
            .as_coroutine()
            .upvar_tys()
            .iter()
            .enumerate()
            .map(|(i, t)| Field { name: format!("upvar{i}"), ty: t, offset: layout.fields.offset(i).bytes(), local: i })
            .collect();
        let mut co = Coroutine {
            def_id,
            ty,
            type_name: rustc_const_eval::util::type_name(tcx, ty),
            size: layout.size.bytes(),
            align: layout.align.bytes(),
            is_fn_body,
            upvars,
            tag: None,
            states: vec![],
        };
        if let Variants::Multiple { tag, tag_encoding: rustc_abi::TagEncoding::Direct, tag_field, variants } =
            &layout.variants
        {
            co.tag = Some((tag.size(&tcx).bytes(), layout.fields.offset(tag_field.as_usize()).bytes()));
            for (v, locals) in cl.variant_fields.iter_enumerated() {
                let span = cl.variant_source_info[v].span;
                let at = tcx.sess.source_map().span_to_diagnostic_string(span);
                let offsets = &variants[v].field_offsets;
                let fields = locals
                    .iter_enumerated()
                    .map(|(f, local)| {
                        let saved = &cl.field_tys[*local];
                        Field {
                            name: saved
                                .debuginfo_name
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("_l{}", local.as_usize())),
                            ty: saved.ty,
                            offset: offsets[f].bytes(),
                            local: local.as_usize(),
                        }
                    })
                    .collect();
                co.states.push(State { index: v.as_usize(), at, fields });
            }
        }
        out.push(co);
    }
    out
}

fn dump(cos: &[Coroutine<'_>]) -> String {
    let mut out = String::new();
    for co in cos {
        let _ = writeln!(out, "coroutine {} size={} align={}", co.ty, co.size, co.align);
        for u in &co.upvars {
            let _ = writeln!(out, "  upvar {}: {} @ {}", u.local, u.ty, u.offset);
        }
        if let Some((size, off)) = co.tag {
            let _ = writeln!(out, "  tag: {size} B @ {off}");
        }
        for s in &co.states {
            let _ = writeln!(out, "  variant {} ({:?})", s.index, s.at);
            for f in &s.fields {
                let _ = writeln!(out, "    {}: {} @ {} (local {})", f.name, f.ty, f.offset, f.local);
            }
        }
    }
    out
}

struct Driver;

impl Callbacks for Driver {
    fn after_analysis<'tcx>(&mut self, _: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let krate = tcx.crate_name(LOCAL_CRATE).to_string();
        let wanted = std::env::var("DURABLE_CRATES").unwrap_or_default();
        if !wanted.split(',').any(|c| c == krate) {
            return Compilation::Continue;
        }
        let cos = collect(tcx);
        if cos.is_empty() {
            return Compilation::Continue; // e.g. the lib target of a lib+bin package
        }
        if let Ok(path) = std::env::var("DURABLE_DUMP") {
            std::fs::write(path, dump(&cos)).unwrap();
        }
        let code = glue::generate(tcx, &cos);
        if let Ok(path) = std::env::var("DURABLE_GLUE_OUT") {
            std::fs::write(path, &code).unwrap();
        }
        if let Ok(path) = std::env::var("DURABLE_GLUE_CHECK") {
            let included = std::fs::read_to_string(&path).unwrap_or_default();
            if included != code {
                tcx.dcx().fatal(format!(
                    "durable glue in {path} does not match this build's coroutine layouts; rerun pass 1"
                ));
            }
        }
        Compilation::Continue
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    // As RUSTC_WORKSPACE_WRAPPER, argv[1] is the real rustc path.
    if args.get(1).is_some_and(|a| a.ends_with("rustc")) {
        args.remove(1);
    }
    rustc_driver::install_ice_hook("durable-driver", |_| ());
    let code = rustc_driver::catch_with_exit_code(|| rustc_driver::compiler_entrypoint(&args, &mut Driver));
    std::process::exit(if code == std::process::ExitCode::SUCCESS { 0 } else { 1 });
}
