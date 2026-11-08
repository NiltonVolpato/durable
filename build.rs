// Provide $OUT_DIR/durable_glue.rs: the driver's generated glue when
// DURABLE_GLUE_SRC is set (pass 2), otherwise an empty table (pass 1).
fn main() {
    // EVOLVE=1|2|3 selects a version of the `evolve` workflow (default 1).
    println!("cargo::rerun-if-env-changed=EVOLVE");
    println!("cargo::rustc-check-cfg=cfg(evolve, values(\"1\", \"2\", \"3\"))");
    let v = std::env::var("EVOLVE").unwrap_or_else(|_| "1".into());
    println!("cargo::rustc-cfg=evolve=\"{v}\"");

    // Same for tests/serde_roundtrip.rs with DURABLE_TEST_GLUE_SRC.
    println!("cargo:rerun-if-env-changed=DURABLE_TEST_GLUE_SRC");
    let out_test = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("durable_test_glue.rs");
    match std::env::var("DURABLE_TEST_GLUE_SRC") {
        Ok(src) => {
            println!("cargo:rerun-if-changed={src}");
            std::fs::copy(&src, &out_test).unwrap();
        }
        Err(_) => {
            std::fs::write(&out_test, "pub static GLUE: &[::durable_poc::glue::Entry] = &[];\n").unwrap();
        }
    }

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
