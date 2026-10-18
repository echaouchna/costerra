//! A string template (`"${upper(var.x)}-%{ if a }b%{ endif }"`) evaluated with
//! costerra's expression evaluator and built-in functions.

#![no_main]

use costerra::hcl::eval::Evaluator;
use costerra::hcl::value::Map;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|src: &str| {
    let root = Map::new();
    // An empty directory: file functions (file(), templatefile()) find nothing.
    let dir = std::env::temp_dir().join("costerra-fuzz-empty");
    let _ = std::fs::create_dir_all(&dir);
    let ev = Evaluator::new(&root, &dir);
    let _ = ev.eval_template_str(src, &Map::new());
});
