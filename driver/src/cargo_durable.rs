//! `cargo durable build|run|test [cargo args]`: build with durable-driver and
//! the generated serde glue, in one command.
//!
//! 1. Build with the existing glue (`DURABLE_PASS=2`). If it is current, done.
//! 2. Otherwise build with empty glue (`DURABLE_PASS=1`), which writes it.
//! 3. Build again with the new glue (`DURABLE_PASS=2`), which checks it.
//!
//! Glue goes to `<target dir>/durable/<crate>.rs`.

use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

fn target_dir() -> PathBuf {
    let out = cargo().args(["metadata", "-q", "--format-version", "1", "--no-deps"]).output().expect("cargo metadata");
    let s = String::from_utf8_lossy(&out.stdout);
    let key = "\"target_directory\":\"";
    let start = s.find(key).expect("target_directory in cargo metadata") + key.len();
    PathBuf::from(&s[start..start + s[start..].find('"').unwrap()])
}

struct Env {
    driver: PathBuf,
    glue_dir: PathBuf,
}

impl Env {
    fn cargo(&self, sub: &str, args: &[String], pass: &str) -> Command {
        let mut c = cargo();
        c.arg(sub)
            .args(args)
            .env("RUSTC_WORKSPACE_WRAPPER", &self.driver)
            .env("RUSTC_WRAPPER", "") // e.g. sccache: does not know the driver
            .env("DURABLE_GLUE_DIR", &self.glue_dir)
            .env("DURABLE_PASS", pass);
        c
    }

    fn build(&self, args: &[String]) -> bool {
        // Quiet first attempt: only shown if the glue was already current.
        let out = self.cargo("build", args, "2").stderr(Stdio::piped()).output().expect("cargo build");
        if out.status.success() {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            return true;
        }
        self.cargo("build", args, "1").status().expect("cargo build").success()
            && self.cargo("build", args, "2").status().expect("cargo build").success()
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("durable") {
        args.remove(0); // invoked as `cargo durable ...`
    }
    let usage = "usage: cargo durable build|run|test [cargo args] [-- program args]";
    if args.is_empty() {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    }
    let cmd = args.remove(0);
    let driver = std::env::current_exe().unwrap().with_file_name("durable-driver");
    if !driver.exists() {
        eprintln!("durable-driver not found next to cargo-durable ({})", driver.display());
        return ExitCode::FAILURE;
    }
    let env = Env { driver, glue_dir: target_dir().join("durable") };
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (build_args, rest) = args.split_at(split);
    let mut to_build = build_args.to_vec();
    if cmd == "test" {
        to_build.push("--tests".into());
    }
    if !env.build(&to_build) {
        return ExitCode::FAILURE;
    }
    match cmd.as_str() {
        "build" => ExitCode::SUCCESS,
        "run" | "test" => {
            let mut run_args = build_args.to_vec();
            run_args.extend(rest.iter().cloned());
            let ok = env.cargo(&cmd, &run_args, "2").status().expect("cargo run").success();
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        _ => {
            eprintln!("{usage}");
            ExitCode::FAILURE
        }
    }
}
