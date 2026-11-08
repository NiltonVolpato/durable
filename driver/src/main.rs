//! durable-driver: rustc plus a pass over the frames of durable workflows.
//!
//! Use as `RUSTC_WORKSPACE_WRAPPER`. Only crates named in `DURABLE_CRATES`
//! (comma-separated) are processed. Roots are `async fn`s marked
//! `#[durable::workflow]`; from them the driver follows every frame they can
//! hold: coroutines (generic or from other crates, with concrete arguments),
//! closures, and structs implementing `durable::Structural`.
//!
//! Outputs (each optional):
//! - `DURABLE_DUMP=path`: human-readable dump of the frames.
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
mod lint;
mod mir;

use rustc_abi::Variants;
use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::{self, EarlyBinder, Ty, TyCtxt, TypeVisitableExt};
use rustc_span::def_id::{DefId, LOCAL_CRATE};
use rustc_span::{Span, Symbol};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;

/// One stored value: a live local, an upvar, or a struct field.
pub struct Field<'tcx> {
    pub name: String,
    pub ty: Ty<'tcx>,
    pub offset: u64,
    /// Where the value is declared.
    pub span: Span,
    /// For a value guarded by a drop flag: the flag's offset. The value is
    /// stored only while the flag says it is initialized.
    pub flag: Option<u64>,
}

pub struct State<'tcx> {
    pub index: usize,
    pub at: String,
    /// The await (or yield) that creates this state.
    pub span: Span,
    pub fields: Vec<Field<'tcx>>,
}

pub enum Shape<'tcx> {
    Coroutine {
        /// `async fn` bodies move their upvars into locals on first resume.
        is_fn_body: bool,
        upvars: Vec<Field<'tcx>>,
        /// (size in bytes, offset), Direct encoding only.
        tag: Option<(u64, u64)>,
        states: Vec<State<'tcx>>,
    },
    /// Closure upvars, or the fields of a `Structural` struct.
    Fields(Vec<Field<'tcx>>),
}

pub struct Frame<'tcx> {
    pub ty: Ty<'tcx>,
    pub type_name: String,
    pub size: u64,
    pub align: u64,
    /// A `#[durable::workflow]` root: gets an entry in the glue table.
    pub root: bool,
    pub shape: Shape<'tcx>,
}

pub struct Ctx<'tcx> {
    pub tcx: TyCtxt<'tcx>,
    /// The runtime crate in the crate graph: `durable`, or the PoC's `durable_poc`.
    pub rt: &'static str,
    structural: Option<rustc_span::def_id::DefId>,
}

fn is_fn_body(tcx: TyCtxt<'_>, def_id: rustc_span::def_id::DefId) -> bool {
    matches!(
        tcx.coroutine_kind(def_id),
        Some(rustc_hir::CoroutineKind::Desugared(_, rustc_hir::CoroutineSource::Fn))
    )
}

impl<'tcx> Ctx<'tcx> {
    fn normalize(&self, ty: ty::Unnormalized<'tcx, Ty<'tcx>>) -> Ty<'tcx> {
        self.tcx.normalize_erasing_regions(ty::TypingEnv::fully_monomorphized(), ty)
    }

    /// Types the driver handles field by field instead of through serde.
    pub fn is_frame(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            ty::Coroutine(..) | ty::Closure(..) => !ty.has_param() && !ty.has_escaping_bound_vars(),
            ty::Adt(def, _) => {
                def.is_struct()
                    && !ty.has_param()
                    && !ty.has_erased_regions()
                    && !ty.has_escaping_bound_vars()
                    && self.structural.is_some_and(|s| glue::implements(self.tcx, s, ty, &[]))
            }
            _ => false,
        }
    }

    fn frame(&self, ty: Ty<'tcx>, root: bool) -> Option<Frame<'tcx>> {
        let tcx = self.tcx;
        let layout = tcx.layout_of(ty::TypingEnv::fully_monomorphized().as_query_input(ty)).ok()?;
        let offset = |i: usize| layout.fields.offset(i).bytes();
        let shape = match ty.kind() {
            ty::Coroutine(def_id, args) => {
                let cl = tcx.coroutine_layout(*def_id, *args).ok()?;
                let span = tcx.def_span(*def_id);
                let upvars = (args.as_coroutine().upvar_tys().iter().enumerate())
                    .map(|(i, t)| Field { name: format!("upvar{i}"), ty: t, offset: offset(i), span, flag: None })
                    .collect();
                let mut tag = None;
                let mut states = vec![];
                if let Variants::Multiple {
                    tag: t,
                    tag_encoding: rustc_abi::TagEncoding::Direct,
                    tag_field,
                    variants,
                } = &layout.variants
                {
                    tag = Some((t.size(&tcx).bytes(), offset(tag_field.as_usize())));
                    let guards = mir::drop_flags(tcx, *def_id, cl);
                    for (v, locals) in cl.variant_fields.iter_enumerated() {
                        let span = cl.variant_source_info[v].span;
                        let name = |local: rustc_middle::mir::CoroutineSavedLocal| {
                            cl.field_tys[local]
                                .debuginfo_name
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("_l{}", local.as_usize()))
                        };
                        let off = |local| {
                            locals.iter_enumerated().find(|(_, l)| **l == local).map(|(f, _)| variants[v].field_offsets[f].bytes())
                        };
                        let fields = locals
                            .iter_enumerated()
                            .map(|(f, local)| {
                                let saved = &cl.field_tys[*local];
                                // A drop flag is named after the value it guards.
                                let guarded = guards.iter().find(|(flag, _)| *flag == *local).map(|(_, val)| *val);
                                Field {
                                    name: match guarded {
                                        Some(val) => format!("{}.init", name(val)),
                                        None => name(*local),
                                    },
                                    ty: self.normalize(EarlyBinder::bind(tcx, saved.ty).instantiate(tcx, *args)),
                                    offset: variants[v].field_offsets[f].bytes(),
                                    span: saved.source_info.span,
                                    flag: guards.iter().find(|(_, val)| *val == *local).and_then(|(flag, _)| off(*flag)),
                                }
                            })
                            .collect();
                        let at = tcx.sess.source_map().span_to_diagnostic_string(span);
                        states.push(State { index: v.as_usize(), at, span, fields });
                    }
                }
                Shape::Coroutine { is_fn_body: is_fn_body(tcx, *def_id), upvars, tag, states }
            }
            ty::Closure(def_id, args) => {
                let span = tcx.def_span(*def_id);
                Shape::Fields(
                    (args.as_closure().upvar_tys().iter().enumerate())
                        .map(|(i, t)| Field { name: format!("c{i}"), ty: t, offset: offset(i), span, flag: None })
                        .collect(),
                )
            }
            ty::Adt(def, args) => Shape::Fields(
                (def.non_enum_variant().fields.iter_enumerated())
                    .map(|(i, f)| Field {
                        name: f.name.to_string(),
                        ty: self.normalize(f.ty(tcx, *args)),
                        offset: offset(i.as_usize()),
                        span: tcx.def_span(f.did),
                        flag: None,
                    })
                    .collect(),
            ),
            _ => return None,
        };
        Some(Frame {
            ty,
            type_name: rustc_const_eval::util::type_name(tcx, ty),
            size: layout.size.bytes(),
            align: layout.align.bytes(),
            root,
            shape,
        })
    }
}

impl<'tcx> Frame<'tcx> {
    /// Every stored value, in any state.
    fn all_fields(&self) -> Vec<&Field<'tcx>> {
        match &self.shape {
            Shape::Coroutine { upvars, states, .. } => {
                upvars.iter().chain(states.iter().flat_map(|s| s.fields.iter())).collect()
            }
            Shape::Fields(f) => f.iter().collect(),
        }
    }
}

/// Functions marked by `#[durable::workflow]`: the macro emits
/// `const _: () = { let _ = ::durable::__private::Root(name); };`.
fn marked_roots(tcx: TyCtxt<'_>) -> HashSet<DefId> {
    use rustc_hir::def::{DefKind, Res};
    use rustc_hir::intravisit::{self, Visitor};
    use rustc_hir::{Expr, ExprKind, QPath};
    struct Find<'tcx> {
        tcx: TyCtxt<'tcx>,
        out: HashSet<DefId>,
    }
    impl<'tcx> Visitor<'tcx> for Find<'tcx> {
        fn visit_expr(&mut self, e: &'tcx Expr<'tcx>) {
            if let ExprKind::Call(callee, [arg]) = e.kind
                && let ExprKind::Path(QPath::Resolved(_, p)) = callee.kind
                && let Res::Def(DefKind::Ctor(..), ctor) = p.res
                && self.tcx.def_path_str(ctor).ends_with("__private::Root")
                && let ExprKind::Path(QPath::Resolved(_, ap)) = arg.kind
                && let Res::Def(DefKind::Fn, f) = ap.res
            {
                self.out.insert(f);
            }
            intravisit::walk_expr(self, e);
        }
    }
    let mut find = Find { tcx, out: HashSet::new() };
    for owner in tcx.hir_body_owners() {
        if tcx.def_kind(owner) == rustc_hir::def::DefKind::Const {
            find.visit_expr(tcx.hir_body_owned_by(owner).value);
        }
    }
    find.out
}

/// A root: marked by `#[durable::workflow]`, or (PoC) by the tool attribute.
fn is_workflow(tcx: TyCtxt<'_>, marked: &HashSet<DefId>, def_id: DefId) -> bool {
    let root = tcx.typeck_root_def_id(def_id);
    let path = [Symbol::intern("durable"), Symbol::intern("workflow")];
    marked.contains(&root) || tcx.tool_attrs(root, &path).next().is_some()
}

/// Workflow roots, then every frame reachable from them.
fn collect<'tcx>(cx: &Ctx<'tcx>) -> Vec<Frame<'tcx>> {
    let tcx = cx.tcx;
    let marked = marked_roots(tcx);
    for f in &marked {
        if tcx.generics_of(*f).count() > 0 {
            tcx.dcx().span_err(tcx.def_span(*f), "durable workflows cannot be generic (alpha); wrap the generic code in a concrete workflow");
        }
    }
    let mut queue = VecDeque::new();
    for def_id in tcx.hir_body_owners() {
        let def_id = def_id.to_def_id();
        if tcx.is_coroutine(def_id) && is_fn_body(tcx, def_id) && is_workflow(tcx, &marked, def_id) {
            let ty = tcx.type_of(def_id).instantiate_identity().skip_normalization();
            if !ty.has_param() {
                queue.push_back((ty, true));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut frames = vec![];
    while let Some((ty, root)) = queue.pop_front() {
        if !seen.insert(ty) {
            continue;
        }
        let Some(frame) = cx.frame(ty, root) else { continue };
        // Frames anywhere inside a stored type, e.g. the block in `Ephemeral<{async block}>`:
        // the lint needs them to know whether an opaque wrapper can park.
        for f in frame.all_fields() {
            for t in f.ty.walk().filter_map(|a| a.as_type()) {
                if cx.is_frame(t) {
                    queue.push_back((t, false));
                }
            }
        }
        frames.push(frame);
    }
    frames
}

fn dump(frames: &[Frame<'_>]) -> String {
    let mut out = String::new();
    for fr in frames {
        let kind = match fr.shape {
            Shape::Coroutine { .. } => "coroutine",
            Shape::Fields(_) => "fields",
        };
        let root = if fr.root { " (root)" } else { "" };
        let _ = writeln!(out, "{kind} {} size={} align={}{root}", fr.ty, fr.size, fr.align);
        match &fr.shape {
            Shape::Coroutine { upvars, tag, states, .. } => {
                for (i, u) in upvars.iter().enumerate() {
                    let _ = writeln!(out, "  upvar {i}: {} @ {}", u.ty, u.offset);
                }
                if let Some((size, off)) = tag {
                    let _ = writeln!(out, "  tag: {size} B @ {off}");
                }
                for s in states {
                    let _ = writeln!(out, "  variant {} ({:?})", s.index, s.at);
                    for f in &s.fields {
                        let _ = writeln!(out, "    {}: {} @ {}", f.name, f.ty, f.offset);
                    }
                }
            }
            Shape::Fields(fields) => {
                for f in fields {
                    let _ = writeln!(out, "  {}: {} @ {}", f.name, f.ty, f.offset);
                }
            }
        }
    }
    out
}

struct Driver;

impl Callbacks for Driver {
    fn after_analysis<'tcx>(&mut self, _: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let krate = tcx.crate_name(LOCAL_CRATE).to_string();
        if let Ok(wanted) = std::env::var("DURABLE_CRATES")
            && !wanted.split(',').any(|c| c == krate)
        {
            return Compilation::Continue;
        }
        // The runtime crate this crate uses, if any.
        let rt = ["durable", "durable_poc"]
            .into_iter()
            .find(|c| glue::find_trait(tcx, &[c], "SlowLeaf").is_some());
        let Some(rt) = rt else { return Compilation::Continue };
        glue::set_runtime(rt);
        let cx = Ctx { tcx, rt, structural: glue::find_trait(tcx, &[rt], "Structural") };
        let frames = collect(&cx);
        if frames.is_empty() {
            return Compilation::Continue; // e.g. the lib target of a lib+bin package
        }
        if let Ok(path) = std::env::var("DURABLE_DUMP") {
            std::fs::write(path, dump(&frames)).unwrap();
        }
        let planner = glue::Planner::new(&cx, &frames);
        if lint::check(&cx, &frames, &planner) > 0 || tcx.dcx().has_errors().is_some() {
            return Compilation::Stop;
        }
        let code = glue::generate(&frames, &planner);
        if let Ok(path) = std::env::var("DURABLE_GLUE_OUT") {
            std::fs::write(path, &code).unwrap();
        }
        // cargo durable: pass 1 writes `<dir>/<crate>.rs`; pass 2 (which
        // included it via `durable::glue!()`) checks it, and rewrites it if stale.
        if let Ok(dir) = std::env::var("DURABLE_GLUE_DIR") {
            let path = format!("{dir}/{krate}.rs");
            let pass2 = std::env::var("DURABLE_PASS").as_deref() == Ok("2");
            let included = std::fs::read_to_string(&path).unwrap_or_default();
            if included != code {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &code).unwrap();
                if pass2 {
                    tcx.dcx().fatal(format!(
                        "durable glue for `{krate}` was out of date and has been regenerated; build again"
                    ));
                }
            }
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
    // `cfg(durable)`: this crate is built by cargo durable, with its glue
    // (e.g. tests that need the glue use `#![cfg(durable)]`).
    if args.iter().any(|a| a == "--crate-name") {
        args.extend(["--cfg".into(), "durable".into()]);
    }
    rustc_driver::install_ice_hook("durable-driver", |_| ());
    let code = rustc_driver::catch_with_exit_code(|| rustc_driver::compiler_entrypoint(&args, &mut Driver));
    std::process::exit(if code == std::process::ExitCode::SUCCESS { 0 } else { 1 });
}
