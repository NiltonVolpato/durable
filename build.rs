// Provide $OUT_DIR/durable_glue.rs: the driver's generated glue when
// DURABLE_GLUE_SRC is set (pass 2), otherwise an empty table (pass 1).
fn main() {
    println!("cargo:rerun-if-env-changed=DURABLE_GLUE_SRC");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("durable_glue.rs");
    match std::env::var("DURABLE_GLUE_SRC") {
        Ok(src) => {
            println!("cargo:rerun-if-changed={src}");
            std::fs::copy(&src, &out).unwrap();
        }
        Err(_) => {
            std::fs::write(&out, "pub static GLUE: &[::durable_poc::glue::Entry] = &[];\n").unwrap();
        }
    }
}
