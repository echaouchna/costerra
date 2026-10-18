//! A `.tf` file read into a module (resources, module calls, variables...):
//! costerra reads the Terraform code of the repositories it analyses.

#![no_main]

use costerra::tf::parse::{ParsedModule, add_source};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|src: &str| {
    let mut m = ParsedModule::default();
    add_source(&mut m, src, "main.tf");
});
