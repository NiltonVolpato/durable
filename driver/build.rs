// Let the driver find librustc_driver at run time.
fn main() {
    let out = std::process::Command::new(std::env::var("RUSTC").unwrap())
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    let sysroot = String::from_utf8(out.stdout).unwrap();
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}/lib", sysroot.trim());
}
