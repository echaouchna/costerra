//! Static evaluation of Terragrunt configurations.

use crate::hcl::eval::{EvalResult, Evaluator, FuncExt, err};
use crate::hcl::value::{Map, Value};
use crate::tf::parse::{attr_expr, block_label};
use hcl::{Body, Structure};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct TgConfig {
    pub file: PathBuf,
    pub source: Option<String>,
    /// Directory the `source` is relative to.
    pub source_base: PathBuf,
    pub inputs: Map,
    pub locals: Map,
    pub generate: Vec<(String, String)>,
    pub dependencies: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl TgConfig {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("locals".into(), Value::Map(self.locals.clone()));
        m.insert("inputs".into(), Value::Map(self.inputs.clone()));
        let mut tf = Map::new();
        tf.insert("source".into(), self.source.clone().map(Value::String).unwrap_or(Value::Null));
        m.insert("terraform".into(), Value::Map(tf));
        Value::Map(m)
    }
}

/// Callback used to obtain outputs of a dependency unit.
pub type DepOutputs<'a> = &'a (dyn Fn(&Path) -> Option<Map> + Sync);

pub struct TgEngine {
    pub allow_run_cmd: bool,
    read_cache: Mutex<HashMap<(PathBuf, PathBuf), Arc<TgConfig>>>,
}

#[derive(Clone)]
struct Ctx<'a> {
    engine: &'a TgEngine,
    /// Directory of the unit being evaluated (get_terragrunt_dir()).
    unit_dir: PathBuf,
    /// Directory of the file currently being parsed.
    file_dir: PathBuf,
    /// When evaluating an included parent config: the parent file path.
    include_parent: Option<PathBuf>,
    deps: Option<DepOutputs<'a>>,
    depth: usize,
}

impl Default for TgEngine {
    fn default() -> Self {
        Self::new(false)
    }
}

pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

fn rel_path(from: &Path, to: &Path) -> String {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); f.len() - common];
    parts.extend(t[common..].iter().map(|c| c.as_os_str().to_string_lossy().to_string()));
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

impl FuncExt for Ctx<'_> {
    fn call(&self, name: &str, args: &[Value], _ev: &Evaluator) -> Option<EvalResult> {
        let sarg = |i: usize| args.get(i).and_then(|v| v.as_str().map(str::to_string));
        Some(match name {
            "find_in_parent_folders" => {
                if args.iter().any(Value::is_unknown) {
                    return Some(Ok(Value::Unknown));
                }
                let target = sarg(0).unwrap_or_else(|| "terragrunt.hcl".into());
                let mut cur = self.unit_dir.parent();
                while let Some(d) = cur {
                    let p = d.join(&target);
                    if p.exists() {
                        return Some(Ok(Value::String(p.to_string_lossy().to_string())));
                    }
                    cur = d.parent();
                }
                match args.get(1) {
                    Some(fallback) => Ok(fallback.clone()),
                    None => err(format!("find_in_parent_folders: '{target}' not found")),
                }
            }
            "path_relative_to_include" => Ok(Value::String(match &self.include_parent {
                Some(p) => rel_path(p.parent().unwrap_or(Path::new("/")), &self.unit_dir),
                None => ".".into(),
            })),
            "path_relative_from_include" => Ok(Value::String(match &self.include_parent {
                Some(p) => rel_path(&self.unit_dir, p.parent().unwrap_or(Path::new("/"))),
                None => ".".into(),
            })),
            "get_terragrunt_dir" | "get_original_terragrunt_dir" => Ok(Value::String(self.unit_dir.to_string_lossy().to_string())),
            "get_parent_terragrunt_dir" => Ok(Value::String(
                self.include_parent.as_ref().and_then(|p| p.parent()).unwrap_or(&self.unit_dir).to_string_lossy().to_string(),
            )),
            "get_repo_root" => {
                Ok(find_repo_root(&self.unit_dir).map(|p| Value::String(p.to_string_lossy().to_string())).unwrap_or(Value::Unknown))
            }
            "get_path_from_repo_root" => {
                Ok(find_repo_root(&self.unit_dir).map(|r| Value::String(rel_path(&r, &self.unit_dir))).unwrap_or(Value::Unknown))
            }
            "get_path_to_repo_root" => {
                Ok(find_repo_root(&self.unit_dir).map(|r| Value::String(rel_path(&self.unit_dir, &r))).unwrap_or(Value::Unknown))
            }
            "get_env" => {
                let Some(n) = sarg(0) else { return Some(Ok(Value::Unknown)) };
                Ok(match std::env::var(&n) {
                    Ok(v) => Value::String(v),
                    Err(_) => args.get(1).cloned().unwrap_or(Value::String(String::new())),
                })
            }
            "get_platform" => Ok(Value::String(std::env::consts::OS.into())),
            "get_terraform_command" => Ok(Value::String("plan".into())),
            "get_terraform_cli_args" => Ok(Value::List(vec![])),
            "get_terraform_commands_that_need_vars"
            | "get_terraform_commands_that_need_input"
            | "get_terraform_commands_that_need_locking"
            | "get_terraform_commands_that_need_parallelism" => Ok(Value::List(
                ["apply", "console", "destroy", "import", "plan", "push", "refresh"].iter().map(|s| Value::from(*s)).collect(),
            )),
            "get_terragrunt_source_cli_flag" => Ok(Value::String(String::new())),
            "mark_as_read" => Ok(args.first().cloned().unwrap_or(Value::Null)),
            "get_aws_account_id"
            | "get_aws_account_alias"
            | "get_aws_caller_identity_arn"
            | "get_aws_caller_identity_user_id"
            | "sops_decrypt_file"
            | "get_default_retryable_errors" => Ok(Value::Unknown),
            "run_cmd" => {
                if !self.engine.allow_run_cmd || args.iter().any(Value::is_unknown) {
                    return Some(Ok(Value::Unknown));
                }
                let mut parts: Vec<String> = args.iter().filter_map(Value::to_interp_string).collect();
                parts.retain(|p| p != "--terragrunt-quiet" && p != "--terragrunt-global-cache");
                if parts.is_empty() {
                    return Some(Ok(Value::Unknown));
                }
                match std::process::Command::new(&parts[0]).args(&parts[1..]).current_dir(&self.unit_dir).output() {
                    Ok(o) if o.status.success() => Ok(Value::String(String::from_utf8_lossy(&o.stdout).trim_end().to_string())),
                    _ => Ok(Value::Unknown),
                }
            }
            "read_terragrunt_config" => {
                let Some(p) = sarg(0) else { return Some(Ok(Value::Unknown)) };
                let path = self.resolve(&p);
                if !path.exists() {
                    return Some(match args.get(1) {
                        Some(d) => Ok(d.clone()),
                        None => err(format!("read_terragrunt_config: {} not found", path.display())),
                    });
                }
                if self.depth > 16 {
                    return Some(Ok(Value::Unknown));
                }
                let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
                let key = (path.clone(), self.unit_dir.clone());
                if let Some(c) = self.engine.read_cache.lock().get(&key) {
                    return Some(Ok(c.to_value()));
                }
                let ctx = Ctx {
                    engine: self.engine,
                    unit_dir: self.unit_dir.clone(),
                    file_dir: dir,
                    include_parent: None,
                    deps: self.deps,
                    depth: self.depth + 1,
                };
                let cfg = Arc::new(ctx.eval_file(&path));
                let v = cfg.to_value();
                self.engine.read_cache.lock().insert(key, cfg);
                Ok(v)
            }
            "read_tfvars_file" => {
                let Some(p) = sarg(0) else { return Some(Ok(Value::Unknown)) };
                let path = self.resolve(&p);
                let vals = read_tfvars(&path);
                Ok(Value::String(Value::Map(vals).to_json().to_string()))
            }
            _ => return None,
        })
    }
}

/// Read a `.tfvars` (HCL) or `.tfvars.json` file into a map of literal values.
pub fn read_tfvars(path: &Path) -> Map {
    let Ok(src) = std::fs::read_to_string(path) else { return Map::new() };
    if path.extension().is_some_and(|e| e == "json") {
        return serde_json::from_str::<serde_json::Value>(&src)
            .ok()
            .and_then(|j| Value::from_json(&j).as_map().cloned())
            .unwrap_or_default();
    }
    let Ok(body) = hcl::parse(&src) else { return Map::new() };
    let empty = Map::new();
    let dir = path.parent().unwrap_or(Path::new("."));
    let ev = Evaluator::new(&empty, dir);
    body.attributes().map(|a| (a.key.to_string(), ev.eval_lenient(&a.expr))).collect()
}

fn deep_merge(base: &mut Map, over: Map) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(Value::Map(b)), Value::Map(o)) => deep_merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Fill unknown values in `known` with values from `mock`.
fn fill_unknown(known: Value, mock: &Value) -> Value {
    match (known, mock) {
        (Value::Unknown, m) => m.clone(),
        (Value::Map(mut k), Value::Map(m)) => {
            for (key, mv) in m {
                let cur = k.shift_remove(key).unwrap_or(Value::Unknown);
                k.insert(key.clone(), fill_unknown(cur, mv));
            }
            Value::Map(k)
        }
        (k, _) => k,
    }
}

impl Ctx<'_> {
    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() { path.to_path_buf() } else { crate::tf::source::normalize(&self.file_dir.join(path)) }
    }

    fn eval_file(&self, file: &Path) -> TgConfig {
        let mut cfg = TgConfig { file: file.to_path_buf(), source_base: self.file_dir.clone(), ..Default::default() };
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                cfg.warnings.push(format!("{}: {e}", file.display()));
                return cfg;
            }
        };
        let body: Body = match hcl::parse(&src) {
            Ok(b) => b,
            Err(e) => {
                cfg.warnings.push(format!("{}: {e}", file.display()));
                return cfg;
            }
        };
        self.eval_body(&body, cfg)
    }

    fn eval_body(&self, body: &Body, mut cfg: TgConfig) -> TgConfig {
        let mut root = Map::new();
        let base = self.file_dir.clone();

        // 1. includes
        let mut parents: Vec<(Arc<TgConfig>, String, bool)> = vec![];
        let mut include_vals = Map::new();
        for b in body.blocks().filter(|b| b.identifier.as_str() == "include") {
            let name = block_label(b, 0).unwrap_or("").to_string();
            let path = {
                let ev = Evaluator::new(&root, &base).with_ext(self);
                attr_expr(&b.body, "path").map(|e| ev.eval_lenient(e))
            };
            let Some(Value::String(p)) = path else {
                cfg.warnings.push(format!("include '{name}': path could not be evaluated"));
                continue;
            };
            let ppath = self.resolve(&p);
            if self.depth > 16 {
                continue;
            }
            let pctx = Ctx {
                engine: self.engine,
                unit_dir: self.unit_dir.clone(),
                file_dir: ppath.parent().unwrap_or(Path::new("/")).to_path_buf(),
                include_parent: Some(ppath.clone()),
                deps: self.deps,
                depth: self.depth + 1,
            };
            let pcfg = Arc::new(pctx.eval_file(&ppath));
            let deep = attr_expr(&b.body, "merge_strategy")
                .map(|e| Evaluator::new(&root, &base).eval_lenient(e))
                .and_then(|v| v.as_str().map(|s| s == "deep"))
                .unwrap_or(false);
            include_vals.insert(name.clone(), pcfg.to_value());
            parents.push((pcfg, name, deep));
        }
        // Unlabeled include is addressable as include.<nothing> in old syntax;
        // expose the first include also under "include" directly for compat.
        let include_value = Value::Map(include_vals);
        root.insert("include".into(), include_value);

        // 2. locals (iterate to a fixpoint)
        let local_exprs: Vec<(String, hcl::Expression)> = body
            .blocks()
            .filter(|b| b.identifier.as_str() == "locals")
            .flat_map(|b| b.body.attributes().map(|a| (a.key.to_string(), a.expr.clone())).collect::<Vec<_>>())
            .collect();
        let mut locals = Map::new();
        for _ in 0..=local_exprs.len().min(12) {
            root.insert("local".into(), Value::Map(locals.clone()));
            let ev = Evaluator::new(&root, &base).with_ext(self);
            let next: Map = local_exprs.iter().map(|(k, e)| (k.clone(), ev.eval_lenient(e))).collect();
            let done = next == locals;
            locals = next;
            if done {
                break;
            }
        }
        root.insert("local".into(), Value::Map(locals.clone()));
        cfg.locals = locals;

        // 3. dependencies
        let mut deps = Map::new();
        for b in body.blocks().filter(|b| b.identifier.as_str() == "dependency") {
            let Some(name) = block_label(b, 0) else { continue };
            let ev = Evaluator::new(&root, &base).with_ext(self);
            let mock = attr_expr(&b.body, "mock_outputs").map(|e| ev.eval_lenient(e)).unwrap_or(Value::Null);
            let cp = attr_expr(&b.body, "config_path").map(|e| ev.eval_lenient(e));
            let mut outputs = Value::Unknown;
            if let Some(Value::String(p)) = cp {
                let dep_dir = self.resolve(&p);
                let dep_dir = if dep_dir.is_file() { dep_dir.parent().unwrap().to_path_buf() } else { dep_dir };
                cfg.dependencies.push(dep_dir.clone());
                if let Some(f) = self.deps
                    && let Some(o) = f(&dep_dir)
                {
                    outputs = Value::Map(o);
                }
            }
            let outputs = fill_unknown(outputs, &mock);
            let mut d = Map::new();
            d.insert("outputs".into(), outputs);
            deps.insert(name.to_string(), Value::Map(d));
        }
        root.insert("dependency".into(), Value::Map(deps));

        // 4. terraform / inputs / generate
        let ev = Evaluator::new(&root, &base).with_ext(self);
        for st in body.iter() {
            let Structure::Block(b) = st else {
                continue;
            };
            match b.identifier.as_str() {
                "terraform" => {
                    if let Some(e) = attr_expr(&b.body, "source") {
                        match ev.eval_lenient(e) {
                            Value::String(s) => {
                                cfg.source = Some(s);
                                cfg.source_base = self.unit_dir.clone();
                            }
                            _ => cfg.warnings.push("terraform.source could not be evaluated".into()),
                        }
                    }
                }
                "generate" => {
                    let name = block_label(b, 0).unwrap_or("generated").to_string();
                    if let Some(Value::String(c)) = attr_expr(&b.body, "contents").map(|e| ev.eval_lenient(e)) {
                        cfg.generate.push((name, c));
                    }
                }
                _ => {}
            }
        }
        if let Some(e) = attr_expr(body, "inputs")
            && let Value::Map(m) = ev.eval_lenient(e)
        {
            cfg.inputs = m;
        }

        // 5. merge parents (child overrides)
        for (p, _name, deep) in parents.into_iter().rev() {
            let mut inputs = p.inputs.clone();
            if deep {
                deep_merge(&mut inputs, std::mem::take(&mut cfg.inputs));
            } else {
                for (k, v) in std::mem::take(&mut cfg.inputs) {
                    inputs.insert(k, v);
                }
            }
            cfg.inputs = inputs;
            if cfg.source.is_none() && p.source.is_some() {
                cfg.source = p.source.clone();
                cfg.source_base = self.unit_dir.clone();
            }
            for (n, c) in &p.generate {
                if !cfg.generate.iter().any(|(x, _)| x == n) {
                    cfg.generate.push((n.clone(), c.clone()));
                }
            }
            cfg.dependencies.extend(p.dependencies.iter().cloned());
            cfg.warnings.extend(p.warnings.iter().cloned());
        }
        cfg
    }
}

impl TgEngine {
    pub fn new(allow_run_cmd: bool) -> Self {
        TgEngine { allow_run_cmd, read_cache: Mutex::new(HashMap::new()) }
    }

    /// Evaluate the `terragrunt.hcl` of a unit directory.
    pub fn eval_unit(&self, unit_file: &Path, deps: Option<DepOutputs>) -> TgConfig {
        let dir = unit_file.parent().unwrap_or(Path::new("/")).to_path_buf();
        let ctx = Ctx { engine: self, unit_dir: dir.clone(), file_dir: dir, include_parent: None, deps, depth: 0 };
        ctx.eval_file(unit_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpaths() {
        assert_eq!(rel_path(Path::new("/a/b"), Path::new("/a/b/c/d")), "c/d");
        assert_eq!(rel_path(Path::new("/a/b/c"), Path::new("/a/b")), "..");
        assert_eq!(rel_path(Path::new("/a"), Path::new("/a")), ".");
    }
}
