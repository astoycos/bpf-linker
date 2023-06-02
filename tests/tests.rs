extern crate compiletest_rs as compiletest;

use std::{env, path::PathBuf};
use which::which;

fn run_mode(mode: &'static str) {
    let mut config = compiletest::Config::default();
    config.target = "bpfel-unknown-none".to_string();
    config.target_rustcflags = Some(format!("-C linker={}", env!("CARGO_BIN_EXE_bpf-linker")));
    if let Ok(filecheck) = which("FileCheck") {
        config.llvm_filecheck = Some(filecheck)
    } else if let Ok(filecheck) = which("FileCheck-16") {
        config.llvm_filecheck = Some(filecheck)
    } else {
        panic!("no FileCheck binary found");
    };
    config.mode = mode.parse().expect("Invalid mode");
    config.src_base = PathBuf::from(format!("tests/{}", mode));
    config.link_deps(); // Populate config.target_rustcflags with dependencies on the path
                        //config.clean_rmeta(); // If your tests import the parent crate, this helps with E0464

    compiletest::run_tests(&config);
}

#[test]
fn compile_test() {
    run_mode("assembly");
}
