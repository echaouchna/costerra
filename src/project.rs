//! Project discovery: finds Terraform root modules and Terragrunt units under
//! a path and evaluates them (in parallel) into resource instances.

use crate::hcl::value::{Map, Value};
use crate::tf::evaluate::{EvalEnv, EvalOutput, Providers, ResourceInstance, evaluate_module};
use crate::tf::parse::{ParsedModule, add_source, parse_module_dir, tf_files};
use crate::tf::plan::load_plan;
use crate::tf::source::{ModuleResolver, normalize};
use crate::tf::terragrunt::{TgEngine, find_repo_root, read_tfvars};
use anyhow::Result;
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    Terraform,
    Terragrunt,
    Plan,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub kind: ProjectKind,
    pub resources: Vec<ResourceInstance>,
    /// For plan inputs: the resources before the change.
    pub prior: Option<Vec<ResourceInstance>>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub var_files: Vec<PathBuf>,
    pub vars: Vec<(String, String)>,
    pub workspace: Option<String>,
    pub offline: bool,
    pub allow_run_cmd: bool,
    pub exclude: Vec<String>,
    pub cache_dir: PathBuf,
    /// Do not evaluate one project per `environments/*.tfvars` file.
    pub no_env_discovery: bool,
}

const SKIP_DIRS: &[&str] = &[".terraform", ".terragrunt-cache", ".git", "node_modules", ".terragrunt-stack"];

fn walk_files(root: &Path, exclude: &globset::GlobSet) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
                return false;
            }
            let rel = e.path().strip_prefix(root).unwrap_or(e.path());
            !exclude.is_match(rel)
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

fn rel_name(root: &Path, p: &Path) -> String {
    let r = p.strip_prefix(root).unwrap_or(p).to_string_lossy().to_string();
    if !r.is_empty() {
        return r;
    }
    // The scanned directory itself: name it relative to the repository root so
    // that names are stable across checkouts (e.g. git worktrees in diffs).
    match find_repo_root(p) {
        Some(repo) => {
            let rel = p.strip_prefix(&repo).unwrap_or(p).to_string_lossy().to_string();
            if rel.is_empty() { ".".into() } else { rel }
        }
        None => p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| ".".into()),
    }
}

/// Per-environment variable files of a root module, e.g. `environments/prod.tfvars`
/// (only used when no `--var-file` is given).
pub fn environment_tfvars(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for dir in ["environments", "envs", "env", "vars", "tfvars"] {
        let d = root.join(dir);
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with(".tfvars") || n.to_string_lossy().ends_with(".tfvars.json"))
            })
            .collect();
        files.sort();
        out.extend(files);
        if !out.is_empty() {
            break;
        }
    }
    out
}

pub fn load(path: &Path, opts: &LoadOptions) -> Result<Vec<Project>> {
    let path = std::path::absolute(path)?;
    if !path.exists() {
        anyhow::bail!("path {} does not exist", path.display());
    }
    if path.is_file() {
        let fname = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if fname.ends_with(".json") {
            let pr = load_plan(&path)?;
            return Ok(vec![Project {
                name: fname,
                path: path.clone(),
                kind: ProjectKind::Plan,
                resources: pr.planned,
                prior: pr.prior,
                warnings: vec![],
            }]);
        }
        if fname.ends_with(".hcl") {
            let root = path.parent().unwrap().to_path_buf();
            let runner = TgRunner::new(opts);
            return Ok(runner.run(&path, &root, &[]).project.clone().into_iter().collect());
        }
        anyhow::bail!("unsupported input file: {}", path.display());
    }

    let mut gb = globset::GlobSetBuilder::new();
    for e in &opts.exclude {
        gb.add(globset::Glob::new(e)?);
    }
    let exclude = gb.build()?;
    let files = walk_files(&path, &exclude);

    let tg_files: Vec<PathBuf> = files.iter().filter(|f| f.file_name().is_some_and(|n| n == "terragrunt.hcl")).cloned().collect();

    // Terragrunt units. A terragrunt.hcl that is an ancestor of other units and
    // has no include block is a root configuration, not a unit.
    let tg_dirs: Vec<PathBuf> = tg_files.iter().map(|f| f.parent().unwrap().to_path_buf()).collect();
    let units: Vec<&PathBuf> = tg_files
        .iter()
        .filter(|f| {
            let d = f.parent().unwrap();
            let is_ancestor = tg_dirs.iter().any(|o| o != d && o.starts_with(d));
            !is_ancestor || std::fs::read_to_string(f).map(|s| s.contains("include")).unwrap_or(false)
        })
        .collect();
    let runner = TgRunner::new(opts);
    let tg_projects: Vec<Project> = units.par_iter().filter_map(|f| runner.run(f, &path, &[]).project.clone()).collect();
    let tg_sources = runner.module_dirs.lock().clone();

    // Plain Terraform root modules: dirs with .tf files that are neither
    // Terragrunt units, nor their source modules, nor referenced as local
    // module sources by other dirs.
    let mut tf_dirs: Vec<PathBuf> = files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "tf"))
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    tf_dirs.sort();
    let referenced: HashSet<PathBuf> = tf_dirs
        .par_iter()
        .flat_map_iter(|d| {
            let m = parse_module_dir(d);
            m.module_calls
                .iter()
                .filter(|c| c.source.starts_with("./") || c.source.starts_with("../"))
                .map(|c| normalize(&d.join(&c.source)))
                .collect::<Vec<_>>()
        })
        .collect();
    let roots: Vec<&PathBuf> =
        tf_dirs.iter().filter(|d| !referenced.contains(*d) && !tg_sources.contains(*d) && !tg_dirs.contains(d)).collect();

    let resolver = ModuleResolver::new(opts.cache_dir.join("modules"), opts.offline);
    // One evaluation per (root, environment tfvars) pair.
    let runs: Vec<(&PathBuf, Option<PathBuf>)> = roots
        .iter()
        .flat_map(|d| {
            let envs = if opts.var_files.is_empty() && !opts.no_env_discovery { environment_tfvars(d) } else { vec![] };
            if envs.is_empty() { vec![(*d, None)] } else { envs.into_iter().map(|e| (*d, Some(e))).collect() }
        })
        .collect();
    let mut projects: Vec<Project> = runs
        .par_iter()
        .map(|(d, env_file)| {
            let d = *d;
            let mut inputs = Map::new();
            let mut auto: Vec<PathBuf> = std::fs::read_dir(d)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy();
                    n == "terraform.tfvars"
                        || n == "terraform.tfvars.json"
                        || n.ends_with(".auto.tfvars")
                        || n.ends_with(".auto.tfvars.json")
                })
                .collect();
            auto.sort_by_key(|p| p.file_name().map(|n| n != "terraform.tfvars" && n != "terraform.tfvars.json"));
            for f in auto.iter().chain(opts.var_files.iter()).chain(env_file.iter()) {
                inputs.extend(read_tfvars(f));
            }
            for (k, v) in &opts.vars {
                let val = hcl::parse(&format!("x = {v}"))
                    .ok()
                    .and_then(|b| b.attributes().next().map(|a| a.expr.clone()))
                    .map(|e| crate::hcl::eval::Evaluator::new(&Map::new(), d).eval_lenient(&e))
                    .unwrap_or_else(|| Value::String(v.clone()));
                inputs.insert(k.clone(), val);
            }
            let manifest_root = d.join(".terraform/modules/modules.json").exists().then(|| d.to_path_buf());
            let env = EvalEnv {
                resolver: &resolver,
                manifest_root,
                root_dir: d.to_path_buf(),
                workspace: opts.workspace.clone().unwrap_or_else(|| "default".into()),
            };
            let parsed = parse_module_dir(d);
            let mut out = EvalOutput::default();
            evaluate_module(&env, &parsed, &inputs, Providers::new(), "", "", 0, &mut out);
            let base = rel_name(&path, d);
            let name = match env_file.as_ref().and_then(|f| f.file_stem()) {
                Some(stem) => format!("{base} ({})", stem.to_string_lossy().trim_end_matches(".tfvars")),
                None => base,
            };
            Project {
                name,
                path: d.to_path_buf(),
                kind: ProjectKind::Terraform,
                resources: out.resources,
                prior: None,
                warnings: out.warnings,
            }
        })
        .collect();
    projects.extend(tg_projects);
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(projects)
}

pub struct UnitResult {
    pub project: Option<Project>,
    pub outputs: Map,
}

pub struct TgRunner {
    engine: TgEngine,
    resolver: ModuleResolver,
    workspace: String,
    results: Mutex<HashMap<PathBuf, Arc<UnitResult>>>,
    /// Local module directories used as Terragrunt sources.
    pub module_dirs: Mutex<HashSet<PathBuf>>,
}

impl TgRunner {
    pub fn new(opts: &LoadOptions) -> Self {
        TgRunner {
            engine: TgEngine::new(opts.allow_run_cmd),
            resolver: ModuleResolver::new(opts.cache_dir.join("modules"), opts.offline),
            workspace: opts.workspace.clone().unwrap_or_else(|| "default".into()),
            results: Mutex::new(HashMap::new()),
            module_dirs: Mutex::new(HashSet::new()),
        }
    }

    pub fn run(&self, file: &Path, scan_root: &Path, stack: &[PathBuf]) -> Arc<UnitResult> {
        let dir = file.parent().unwrap().to_path_buf();
        if let Some(r) = self.results.lock().get(&dir) {
            return r.clone();
        }
        let r = Arc::new(self.run_uncached(file, &dir, scan_root, stack));
        self.results.lock().entry(dir).or_insert(r).clone()
    }

    fn run_uncached(&self, file: &Path, dir: &Path, scan_root: &Path, stack: &[PathBuf]) -> UnitResult {
        let mut next_stack = stack.to_vec();
        next_stack.push(dir.to_path_buf());
        let deps = |dep_dir: &Path| -> Option<Map> {
            if next_stack.iter().any(|p| p == dep_dir) {
                return None;
            }
            let f = dep_dir.join("terragrunt.hcl");
            if !f.exists() {
                return None;
            }
            Some(self.run(&f, scan_root, &next_stack).outputs.clone())
        };
        let cfg = self.engine.eval_unit(file, Some(&deps));
        let mut warnings = cfg.warnings.clone();

        let module_dir: Option<PathBuf> = match &cfg.source {
            Some(src) if src.starts_with('.') || src.starts_with('/') => {
                let p = normalize(&cfg.source_base.join(src.replace("//", "/")));
                Some(p)
            }
            Some(src) => match self.resolver.fetch_remote(src, None) {
                Ok(p) => Some(p),
                Err(e) => {
                    warnings.push(format!("cannot load terraform source '{src}': {e}"));
                    None
                }
            },
            None if !tf_files(dir).is_empty() => Some(dir.to_path_buf()),
            None => {
                if cfg.source.is_none() {
                    return UnitResult { project: None, outputs: Map::new() };
                }
                None
            }
        };
        let Some(module_dir) = module_dir else {
            return UnitResult {
                project: Some(Project {
                    name: rel_name(scan_root, dir),
                    path: dir.to_path_buf(),
                    kind: ProjectKind::Terragrunt,
                    resources: vec![],
                    prior: None,
                    warnings,
                }),
                outputs: Map::new(),
            };
        };

        self.module_dirs.lock().insert(module_dir.clone());
        let base = parse_module_dir(&module_dir);
        let parsed: Arc<ParsedModule> = if cfg.generate.is_empty() {
            base
        } else {
            let mut m = (*base).clone();
            for (name, contents) in &cfg.generate {
                add_source(&mut m, contents, &format!("generate:{name}"));
            }
            Arc::new(m)
        };
        let env = EvalEnv {
            resolver: &self.resolver,
            manifest_root: None,
            root_dir: find_repo_root(dir).unwrap_or_else(|| dir.to_path_buf()),
            workspace: self.workspace.clone(),
        };
        let mut out = EvalOutput::default();
        let outputs = evaluate_module(&env, &parsed, &cfg.inputs, Providers::new(), "", "", 0, &mut out);
        warnings.extend(out.warnings);
        UnitResult {
            project: Some(Project {
                name: rel_name(scan_root, dir),
                path: dir.to_path_buf(),
                kind: ProjectKind::Terragrunt,
                resources: out.resources,
                prior: None,
                warnings,
            }),
            outputs,
        }
    }
}
