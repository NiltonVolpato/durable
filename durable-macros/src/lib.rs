//! `#[durable::workflow]` and `durable::glue!()`.

#![feature(proc_macro_tracked_env, proc_macro_tracked_path)]

use proc_macro::{TokenStream, TokenTree, tracked};

fn error(msg: &str) -> TokenStream {
    format!("::core::compile_error!({msg:?});").parse().unwrap()
}

/// Marks a free, non-generic `async fn` as a durable workflow root.
///
/// The function is left unchanged. A hidden `const` names it, so that
/// `durable-driver` can find the roots without a tool attribute.
#[proc_macro_attribute]
pub fn workflow(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return error("`#[durable::workflow]` takes no arguments");
    }
    let toks: Vec<TokenTree> = item.clone().into_iter().collect();
    let ident = |t: &TokenTree, s: &str| matches!(t, TokenTree::Ident(i) if i.to_string() == s);
    let Some(fn_at) = toks.iter().position(|t| ident(t, "fn")) else {
        return error("`#[durable::workflow]` goes on an `async fn`");
    };
    if !toks[..fn_at].iter().any(|t| ident(t, "async")) {
        return error("`#[durable::workflow]` goes on an `async fn`");
    }
    let Some(TokenTree::Ident(name)) = toks.get(fn_at + 1) else {
        return error("`#[durable::workflow]`: expected a function name");
    };
    let generic = matches!(toks.get(fn_at + 2), Some(TokenTree::Punct(p)) if p.as_char() == '<')
        || matches!(toks.get(fn_at + 2), Some(TokenTree::Group(g))
            if g.stream().into_iter().any(|t| matches!(&t, TokenTree::Ident(i) if i.to_string() == "impl")));
    if generic {
        return error("durable workflows cannot be generic (alpha); wrap the generic code in a concrete workflow");
    }
    let marker: TokenStream =
        format!("#[doc(hidden)] const _: () = {{ let _ = ::durable::__private::Root({name}); }};").parse().unwrap();
    let mut out = item;
    out.extend(marker);
    out
}

/// Expands to `pub static GLUE: &[durable::glue::Entry]`: the serde glue that
/// `cargo durable` generated for this crate, or an empty table in a plain
/// `cargo build` (then `Runtime::register` reports the missing glue).
#[proc_macro]
pub fn glue(input: TokenStream) -> TokenStream {
    if !input.is_empty() {
        return error("`durable::glue!()` takes no arguments");
    }
    let pass = tracked::env_var("DURABLE_PASS").unwrap_or_default();
    let dir = tracked::env_var("DURABLE_GLUE_DIR");
    let krate = tracked::env_var("CARGO_CRATE_NAME").unwrap_or_default();
    if pass == "2"
        && let Ok(dir) = dir
    {
        let path = format!("{dir}/{krate}.rs");
        tracked::path(&path);
        if std::path::Path::new(&path).exists() {
            return format!("include!({path:?});").parse().unwrap();
        }
    }
    "pub static GLUE: &[::durable::glue::Entry] = &[];".parse().unwrap()
}
