//! Parsing of Terraform module directories into a structured, cached form.

use hcl::expr::Expression;
use hcl::{Block, Body, Structure};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Managed,
    Data,
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub default: Option<Expression>,
    pub ty: Option<Expression>,
}

#[derive(Debug, Clone)]
pub struct ResourceBlock {
    pub mode: Mode,
    pub rtype: String,
    pub name: String,
    pub body: Body,
    pub count: Option<Expression>,
    pub for_each: Option<Expression>,
    pub provider: Option<String>,
    pub file: Arc<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ModuleCall {
    pub name: String,
    pub source: String,
    pub version: Option<String>,
    pub body: Body,
    pub count: Option<Expression>,
    pub for_each: Option<Expression>,
    /// `providers = { google = google.west }` mapping (child name -> parent name)
    pub providers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct ProviderBlock {
    pub name: String,
    pub alias: Option<String>,
    pub body: Body,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedModule {
    pub dir: PathBuf,
    pub variables: Vec<Variable>,
    pub locals: Vec<(String, Expression)>,
    pub outputs: Vec<(String, Expression)>,
    pub resources: Vec<ResourceBlock>,
    pub module_calls: Vec<ModuleCall>,
    pub providers: Vec<ProviderBlock>,
    pub has_backend: bool,
    pub errors: Vec<String>,
}

static CACHE: LazyLock<RwLock<HashMap<PathBuf, Arc<ParsedModule>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Parse (or fetch from cache) all `*.tf` files in a directory.
pub fn parse_module_dir(dir: &Path) -> Arc<ParsedModule> {
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if let Some(m) = CACHE.read().get(&key) {
        return m.clone();
    }
    let parsed = Arc::new(parse_uncached(&key));
    CACHE.write().insert(key, parsed.clone());
    parsed
}

/// Forget the parsed module of a directory (after its files were edited).
pub fn invalidate(dir: &Path) {
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    CACHE.write().remove(&key);
}

pub fn tf_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "tf"))
        .collect();
    files.sort();
    files
}

fn parse_uncached(dir: &Path) -> ParsedModule {
    let mut m = ParsedModule { dir: dir.to_path_buf(), ..Default::default() };
    for file in tf_files(dir) {
        let src = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => {
                m.errors.push(format!("{}: {e}", file.display()));
                continue;
            }
        };
        match hcl::parse(&src) {
            Ok(body) => add_body(&mut m, body, Arc::new(file)),
            Err(e) => m.errors.push(format!("{}: {e}", file.display())),
        }
    }
    m
}

/// Parse extra HCL source (e.g. Terragrunt `generate` blocks) into a module.
pub fn add_source(m: &mut ParsedModule, src: &str, name: &str) {
    match hcl::parse(src) {
        Ok(body) => add_body(m, body, Arc::new(PathBuf::from(name))),
        Err(e) => m.errors.push(format!("{name}: {e}")),
    }
}

pub fn block_label(b: &Block, i: usize) -> Option<&str> {
    b.labels.get(i).map(|l| l.as_str())
}

pub fn attr_expr<'a>(body: &'a Body, name: &str) -> Option<&'a Expression> {
    body.attributes().find(|a| a.key.as_str() == name).map(|a| &a.expr)
}

pub fn literal_str(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.clone()),
        Expression::TemplateExpr(t) => match t.as_ref() {
            hcl::expr::TemplateExpr::QuotedString(s) if !s.contains("${") => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Render a traversal like `google.west` to a string.
fn expr_path(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Variable(v) => Some(v.to_string()),
        Expression::Traversal(t) => {
            let mut s = expr_path(&t.expr)?;
            for op in &t.operators {
                if let hcl::expr::TraversalOperator::GetAttr(id) = op {
                    s.push('.');
                    s.push_str(id.as_str());
                }
            }
            Some(s)
        }
        _ => literal_str(expr),
    }
}

fn add_body(m: &mut ParsedModule, body: Body, file: Arc<PathBuf>) {
    for st in body.into_inner() {
        let Structure::Block(b) = st else { continue };
        match b.identifier.as_str() {
            "variable" => {
                if let Some(name) = block_label(&b, 0) {
                    m.variables.push(Variable {
                        name: name.to_string(),
                        default: attr_expr(&b.body, "default").cloned(),
                        ty: attr_expr(&b.body, "type").cloned(),
                    });
                }
            }
            "locals" => {
                for a in b.body.attributes() {
                    m.locals.push((a.key.to_string(), a.expr.clone()));
                }
            }
            "output" => {
                if let (Some(name), Some(v)) = (block_label(&b, 0), attr_expr(&b.body, "value")) {
                    m.outputs.push((name.to_string(), v.clone()));
                }
            }
            "resource" | "data" => {
                let (Some(t), Some(n)) = (block_label(&b, 0), block_label(&b, 1)) else { continue };
                let count = attr_expr(&b.body, "count").cloned();
                let for_each = attr_expr(&b.body, "for_each").cloned();
                let provider = attr_expr(&b.body, "provider").and_then(expr_path);
                m.resources.push(ResourceBlock {
                    mode: if b.identifier.as_str() == "resource" { Mode::Managed } else { Mode::Data },
                    rtype: t.to_string(),
                    name: n.to_string(),
                    count,
                    for_each,
                    provider,
                    body: b.body,
                    file: file.clone(),
                });
            }
            "module" => {
                let Some(name) = block_label(&b, 0) else { continue };
                let Some(source) = attr_expr(&b.body, "source").and_then(literal_str) else {
                    m.errors.push(format!("module {name}: non-literal source"));
                    continue;
                };
                let version = attr_expr(&b.body, "version").and_then(literal_str);
                let mut providers = vec![];
                if let Some(Expression::Object(o)) = attr_expr(&b.body, "providers") {
                    for (k, v) in o.iter() {
                        let key = match k {
                            hcl::expr::ObjectKey::Identifier(i) => Some(i.to_string()),
                            hcl::expr::ObjectKey::Expression(e) => expr_path(e),
                            _ => None,
                        };
                        if let (Some(k), Some(v)) = (key, expr_path(v)) {
                            providers.push((k, v));
                        }
                    }
                }
                m.module_calls.push(ModuleCall {
                    name: name.to_string(),
                    source,
                    version,
                    count: attr_expr(&b.body, "count").cloned(),
                    for_each: attr_expr(&b.body, "for_each").cloned(),
                    providers,
                    body: b.body,
                });
            }
            "provider" => {
                if let Some(name) = block_label(&b, 0) {
                    let alias = attr_expr(&b.body, "alias").and_then(literal_str);
                    m.providers.push(ProviderBlock { name: name.to_string(), alias, body: b.body });
                }
            }
            "terraform" if b.body.blocks().any(|x| x.identifier.as_str() == "backend" || x.identifier.as_str() == "cloud") => {
                m.has_backend = true;
            }
            _ => {}
        }
    }
}
