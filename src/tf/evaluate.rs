//! Static evaluation of a Terraform module tree.

use super::own_state::Key;
use super::parse::{Mode, ParsedModule, ResourceBlock, attr_expr, parse_module_dir};
use super::source::ModuleResolver;
use crate::hcl::eval::{Evaluator, Scope, collect_refs};
use crate::hcl::types::TypeSpec;
use crate::hcl::value::{Map, Value};
use hcl::Body;
use hcl::expr::Expression;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProviderCfg {
    pub project: Option<String>,
    pub region: Option<String>,
    pub zone: Option<String>,
}

pub type Providers = HashMap<String, ProviderCfg>;

#[derive(Debug, Clone)]
pub struct ResourceInstance {
    pub address: String,
    pub mode: Mode,
    pub rtype: String,
    pub name: String,
    pub module_path: String,
    pub attrs: Value,
    pub provider: ProviderCfg,
    pub file: Option<Arc<PathBuf>>,
    pub warnings: Vec<String>,
    /// Other resources referenced from this resource's configuration
    /// (`type.name`, without module prefix).
    pub refs: Vec<String>,
}

#[derive(Default)]
pub struct EvalOutput {
    pub resources: Vec<ResourceInstance>,
    /// Resources of other configurations, read from their states, that
    /// resources of this one refer to (for lookups only, never priced).
    pub external: Vec<ResourceInstance>,
    pub warnings: Vec<String>,
}

pub struct EvalEnv<'a> {
    pub resolver: &'a ModuleResolver,
    /// Root directory holding `.terraform/modules/modules.json`, if any.
    pub manifest_root: Option<PathBuf>,
    pub root_dir: PathBuf,
    pub workspace: String,
    /// Read `terraform_remote_state` outputs from the state buckets.
    pub remote_state: bool,
    /// The configuration's own state, for the values of its data sources.
    pub own_state: Option<&'a super::own_state::OwnState>,
}

const MAX_DEPTH: usize = 24;
const SKIP_BLOCKS: &[&str] = &["lifecycle", "provisioner", "connection", "precondition", "postcondition", "timeouts"];
const META_ATTRS: &[&str] = &["count", "for_each", "provider", "depends_on"];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Node {
    Provider(usize),
    Local(usize),
    Module(usize),
    Resource(usize),
}

enum Instances {
    Single,
    Count(usize),
    ForEach(Vec<(String, Value)>),
}

fn body_refs(body: &Body, out: &mut Vec<(String, Option<String>)>) {
    for a in body.attributes() {
        collect_refs(&a.expr, out);
    }
    for b in body.blocks() {
        body_refs(&b.body, out);
    }
}

fn map_mut<'m>(root: &'m mut Map, key: &str) -> &'m mut Map {
    let entry = root.entry(key.to_string()).or_insert_with(|| Value::Map(Map::new()));
    if !matches!(entry, Value::Map(_)) {
        *entry = Value::Map(Map::new());
    }
    match entry {
        Value::Map(m) => m,
        _ => unreachable!(),
    }
}

/// The instances of a resource or module, or `None` when `count` /
/// `for_each` is not known statically.
fn expand_known(ev: &Evaluator, count: &Option<Expression>, for_each: &Option<Expression>) -> Option<Instances> {
    if let Some(c) = count {
        return match ev.eval_lenient(c) {
            Value::Unknown => None,
            v => Some(Instances::Count(v.as_f64().unwrap_or(1.0).max(0.0) as usize)),
        };
    }
    if let Some(fe) = for_each {
        return match ev.eval_lenient(fe) {
            Value::Map(m) => Some(Instances::ForEach(m.into_iter().collect())),
            Value::List(l) => {
                Some(Instances::ForEach(l.into_iter().map(|v| (v.to_interp_string().unwrap_or_else(|| "?".into()), v)).collect()))
            }
            Value::Null => Some(Instances::ForEach(vec![])),
            _ => None,
        };
    }
    Some(Instances::Single)
}

fn expand(ev: &Evaluator, count: &Option<Expression>, for_each: &Option<Expression>, warn: &mut Vec<String>, what: &str) -> Instances {
    expand_known(ev, count, for_each).unwrap_or_else(|| {
        if count.is_some() {
            warn.push(format!("{what}: count is not known statically, assuming 1"));
            Instances::Count(1)
        } else {
            warn.push(format!("{what}: for_each is not known statically, assuming 1 instance"));
            Instances::ForEach(vec![("<unknown>".into(), Value::Unknown)])
        }
    })
}

/// Evaluate a body (attributes + nested blocks) into a map value.
/// Nested blocks become lists of maps, like in Terraform plan JSON.
pub fn eval_body(ev: &Evaluator, body: &Body, scope: Option<&Scope>, skip_meta: bool) -> Map {
    let mut out = Map::new();
    for a in body.attributes() {
        let k = a.key.as_str();
        if skip_meta && META_ATTRS.contains(&k) {
            continue;
        }
        let v = ev.eval_in(&a.expr, scope).unwrap_or(Value::Unknown);
        out.insert(k.to_string(), v);
    }
    for b in body.blocks() {
        let id = b.identifier.as_str();
        if SKIP_BLOCKS.contains(&id) {
            continue;
        }
        if id == "dynamic" {
            let Some(label) = b.labels.first().map(|l| l.as_str().to_string()) else { continue };
            let iter_name = attr_expr(&b.body, "iterator")
                .and_then(|e| match e {
                    Expression::Variable(v) => Some(v.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| label.clone());
            let Some(content) = b.body.blocks().find(|x| x.identifier.as_str() == "content") else { continue };
            let items: Vec<(Value, Value)> = match attr_expr(&b.body, "for_each").map(|e| ev.eval_in(e, scope)) {
                Some(Ok(Value::List(l))) => l.into_iter().enumerate().map(|(i, v)| (Value::Number(i as f64), v)).collect(),
                Some(Ok(Value::Map(m))) => m.into_iter().map(|(k, v)| (Value::String(k), v)).collect(),
                Some(Ok(Value::Null)) => vec![],
                _ => vec![(Value::Unknown, Value::Unknown)],
            };
            let list = map_list(&mut out, &label);
            for (k, v) in items {
                let mut it = Map::new();
                it.insert("key".into(), k);
                it.insert("value".into(), v);
                let s = Scope::new(&iter_name, Value::Map(it), scope);
                list.push(Value::Map(eval_body(ev, &content.body, Some(&s), false)));
            }
            continue;
        }
        let v = Value::Map(eval_body(ev, &b.body, scope, false));
        map_list(&mut out, id).push(v);
    }
    out
}

fn map_list<'m>(m: &'m mut Map, key: &str) -> &'m mut Vec<Value> {
    let e = m.entry(key.to_string()).or_insert_with(|| Value::List(vec![]));
    if !matches!(e, Value::List(_)) {
        *e = Value::List(vec![]);
    }
    match e {
        Value::List(l) => l,
        _ => unreachable!(),
    }
}

fn provider_from_body(ev: &Evaluator, body: &Body) -> ProviderCfg {
    let get = |k: &str| attr_expr(body, k).and_then(|e| ev.eval_lenient(e).as_str().map(str::to_string));
    ProviderCfg { project: get("project"), region: get("region"), zone: get("zone") }
}

fn fmt_key(key: &str) -> String {
    format!("[{}]", serde_json::Value::String(key.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_module(
    env: &EvalEnv,
    parsed: &ParsedModule,
    inputs: &Map,
    mut providers: Providers,
    addr_prefix: &str,
    manifest_key: &str,
    depth: usize,
    out: &mut EvalOutput,
) -> Map {
    let dir = parsed.dir.as_path();
    for e in &parsed.errors {
        out.warnings.push(format!("parse error: {e}"));
    }

    let mut root = Map::new();
    {
        let empty = Map::new();
        let ev0 = Evaluator::new(&empty, dir);
        let mut vars = Map::new();
        for v in &parsed.variables {
            let raw = match inputs.get(&v.name) {
                Some(x) => x.clone(),
                None => v.default.as_ref().map(|d| ev0.eval_lenient(d)).unwrap_or(Value::Unknown),
            };
            let val = match &v.ty {
                Some(t) => TypeSpec::parse(t).convert(raw),
                None => raw,
            };
            vars.insert(v.name.clone(), val);
        }
        root.insert("var".into(), Value::Map(vars));
    }
    let mut path = Map::new();
    path.insert("module".into(), Value::String(dir.to_string_lossy().to_string()));
    path.insert("root".into(), Value::String(env.root_dir.to_string_lossy().to_string()));
    path.insert("cwd".into(), Value::String(env.root_dir.to_string_lossy().to_string()));
    root.insert("path".into(), Value::Map(path));
    let mut tf = Map::new();
    tf.insert("workspace".into(), Value::String(env.workspace.clone()));
    root.insert("terraform".into(), Value::Map(tf));
    root.insert("local".into(), Value::Map(Map::new()));
    root.insert("module".into(), Value::Map(Map::new()));
    root.insert("data".into(), Value::Map(Map::new()));

    // ---- dependency graph --------------------------------------------------
    let local_idx: HashMap<&str, usize> = parsed.locals.iter().enumerate().map(|(i, (n, _))| (n.as_str(), i)).collect();
    let module_idx: HashMap<&str, usize> = parsed.module_calls.iter().enumerate().map(|(i, m)| (m.name.as_str(), i)).collect();
    let mut res_idx: HashMap<(String, String), usize> = HashMap::new();
    for (i, r) in parsed.resources.iter().enumerate() {
        let t = if r.mode == Mode::Data { format!("data.{}", r.rtype) } else { r.rtype.clone() };
        res_idx.insert((t, r.name.clone()), i);
    }

    let resolve = |refs: Vec<(String, Option<String>)>| -> Vec<Node> {
        let mut deps = vec![];
        for (root_name, attr) in refs {
            let Some(attr) = attr else { continue };
            match root_name.as_str() {
                "local" => {
                    if let Some(&i) = local_idx.get(attr.as_str()) {
                        deps.push(Node::Local(i))
                    }
                }
                "module" => {
                    if let Some(&i) = module_idx.get(attr.as_str()) {
                        deps.push(Node::Module(i))
                    }
                }
                "var" | "path" | "terraform" | "each" | "count" | "self" => {}
                _ => {
                    if let Some(&i) = res_idx.get(&(root_name, attr)) {
                        deps.push(Node::Resource(i))
                    }
                }
            }
        }
        deps
    };

    let mut nodes: Vec<Node> = vec![];
    let mut deps: HashMap<Node, Vec<Node>> = HashMap::new();
    for (i, p) in parsed.providers.iter().enumerate() {
        let mut r = vec![];
        body_refs(&p.body, &mut r);
        nodes.push(Node::Provider(i));
        deps.insert(Node::Provider(i), resolve(r));
    }
    for (i, (_, e)) in parsed.locals.iter().enumerate() {
        let mut r = vec![];
        collect_refs(e, &mut r);
        nodes.push(Node::Local(i));
        deps.insert(Node::Local(i), resolve(r));
    }
    for (i, m) in parsed.module_calls.iter().enumerate() {
        let mut r = vec![];
        body_refs(&m.body, &mut r);
        nodes.push(Node::Module(i));
        let mut d = resolve(r);
        d.extend((0..parsed.providers.len()).map(Node::Provider));
        deps.insert(Node::Module(i), d);
    }
    for (i, rb) in parsed.resources.iter().enumerate() {
        let mut r = vec![];
        body_refs(&rb.body, &mut r);
        nodes.push(Node::Resource(i));
        let mut d = resolve(r);
        d.extend((0..parsed.providers.len()).map(Node::Provider));
        deps.insert(Node::Resource(i), d);
    }

    // DFS topological order, tolerant of cycles.
    let mut order = Vec::with_capacity(nodes.len());
    let mut state: HashMap<Node, u8> = HashMap::new();
    fn visit(n: Node, deps: &HashMap<Node, Vec<Node>>, state: &mut HashMap<Node, u8>, order: &mut Vec<Node>) {
        match state.get(&n) {
            Some(_) => return,
            None => {
                state.insert(n, 1);
            }
        }
        if let Some(ds) = deps.get(&n) {
            for d in ds {
                visit(*d, deps, state, order);
            }
        }
        state.insert(n, 2);
        order.push(n);
    }
    for n in &nodes {
        visit(*n, &deps, &mut state, &mut order);
    }

    // ---- evaluation --------------------------------------------------------
    for node in order {
        match node {
            Node::Provider(i) => {
                let p = &parsed.providers[i];
                let cfg = {
                    let ev = Evaluator::new(&root, dir);
                    provider_from_body(&ev, &p.body)
                };
                let key = match &p.alias {
                    Some(a) => format!("{}.{a}", p.name),
                    None => p.name.clone(),
                };
                let entry = providers.entry(key).or_default();
                if cfg.project.is_some() {
                    entry.project = cfg.project;
                }
                if cfg.region.is_some() {
                    entry.region = cfg.region;
                }
                if cfg.zone.is_some() {
                    entry.zone = cfg.zone;
                }
            }
            Node::Local(i) => {
                let (name, expr) = &parsed.locals[i];
                let v = Evaluator::new(&root, dir).eval_lenient(expr);
                map_mut(&mut root, "local").insert(name.clone(), v);
            }
            Node::Resource(i) => {
                let rb = &parsed.resources[i];
                let mut val = eval_resource(env, rb, &root, dir, &providers, addr_prefix, out);
                if env.remote_state && rb.mode == Mode::Data && rb.rtype == "terraform_remote_state" {
                    let address = format!("{addr_prefix}data.terraform_remote_state.{}", rb.name);
                    val = super::remote_state::resolve(val, &mut out.warnings, &address, &mut out.external, dir);
                }
                if rb.mode == Mode::Data {
                    map_mut(map_mut(&mut root, "data"), &rb.rtype).insert(rb.name.clone(), val);
                } else {
                    map_mut(&mut root, &rb.rtype).insert(rb.name.clone(), val);
                }
            }
            Node::Module(i) => {
                let mc = &parsed.module_calls[i];
                if depth >= MAX_DEPTH {
                    out.warnings.push(format!("{addr_prefix}module.{}: max module depth reached", mc.name));
                    continue;
                }
                let key = if manifest_key.is_empty() { mc.name.clone() } else { format!("{manifest_key}.{}", mc.name) };
                let child_dir = match env.resolver.resolve(dir, &mc.source, mc.version.as_deref(), env.manifest_root.as_deref(), &key) {
                    Ok(d) => d,
                    Err(e) => {
                        out.warnings.push(format!("{addr_prefix}module.{}: cannot load source '{}': {e}", mc.name, mc.source));
                        continue;
                    }
                };
                let child = parse_module_dir(&child_dir);

                // Provider inheritance: default providers pass through; explicit
                // `providers` maps remap parent configurations.
                let mut child_providers: Providers =
                    providers.iter().filter(|(k, _)| !k.contains('.')).map(|(k, v)| (k.clone(), v.clone())).collect();
                for (child_name, parent_name) in &mc.providers {
                    if let Some(cfg) = providers.get(parent_name) {
                        child_providers.insert(child_name.clone(), cfg.clone());
                    }
                }

                let mut warns = vec![];
                let instances = {
                    let ev = Evaluator::new(&root, dir);
                    expand(&ev, &mc.count, &mc.for_each, &mut warns, &format!("{addr_prefix}module.{}", mc.name))
                };
                out.warnings.extend(warns);

                let eval_inputs = |scope: Option<&Scope>| -> Map {
                    let ev = Evaluator::new(&root, dir);
                    let mut m = Map::new();
                    for a in mc.body.attributes() {
                        let k = a.key.as_str();
                        if matches!(k, "source" | "version" | "count" | "for_each" | "providers" | "depends_on") {
                            continue;
                        }
                        m.insert(k.to_string(), ev.eval_in(&a.expr, scope).unwrap_or(Value::Unknown));
                    }
                    m
                };

                let result = match instances {
                    Instances::Single => {
                        let inputs = eval_inputs(None);
                        let prefix = format!("{addr_prefix}module.{}.", mc.name);
                        Value::Map(evaluate_module(env, &child, &inputs, child_providers, &prefix, &key, depth + 1, out))
                    }
                    Instances::Count(n) => {
                        let mut list = vec![];
                        for idx in 0..n {
                            let mut c = Map::new();
                            c.insert("index".into(), Value::Number(idx as f64));
                            let s = Scope::new("count", Value::Map(c), None);
                            let inputs = eval_inputs(Some(&s));
                            let prefix = format!("{addr_prefix}module.{}[{idx}].", mc.name);
                            list.push(Value::Map(evaluate_module(
                                env,
                                &child,
                                &inputs,
                                child_providers.clone(),
                                &prefix,
                                &key,
                                depth + 1,
                                out,
                            )));
                        }
                        Value::List(list)
                    }
                    Instances::ForEach(items) => {
                        let mut map = Map::new();
                        for (k, v) in items {
                            let mut e = Map::new();
                            e.insert("key".into(), Value::String(k.clone()));
                            e.insert("value".into(), v);
                            let s = Scope::new("each", Value::Map(e), None);
                            let inputs = eval_inputs(Some(&s));
                            let prefix = format!("{addr_prefix}module.{}{}.", mc.name, fmt_key(&k));
                            map.insert(
                                k,
                                Value::Map(evaluate_module(env, &child, &inputs, child_providers.clone(), &prefix, &key, depth + 1, out)),
                            );
                        }
                        Value::Map(map)
                    }
                };
                map_mut(&mut root, "module").insert(mc.name.clone(), result);
            }
        }
    }

    let ev = Evaluator::new(&root, dir);
    parsed.outputs.iter().map(|(n, e)| (n.clone(), ev.eval_lenient(e))).collect()
}

fn eval_resource(
    env: &EvalEnv,
    rb: &ResourceBlock,
    root: &Map,
    dir: &Path,
    providers: &Providers,
    addr_prefix: &str,
    out: &mut EvalOutput,
) -> Value {
    let ev = Evaluator::new(root, dir);
    let base = if rb.mode == Mode::Data {
        format!("{addr_prefix}data.{}.{}", rb.rtype, rb.name)
    } else {
        format!("{addr_prefix}{}.{}", rb.rtype, rb.name)
    };
    let mut warns = vec![];
    // Data sources are read by `terraform plan` before anything is built from
    // them: their values in the configuration's own state stand in for that.
    // Managed resources only take the ids Google assigned to them.
    let state = match env.own_state {
        Some(s) if rb.rtype != "terraform_remote_state" && (rb.mode == Mode::Data || super::own_state::reads_managed(&rb.rtype)) => {
            s.lookup(&base, &rb.rtype)
        }
        _ => None,
    };
    let instances = match (expand_known(&ev, &rb.count, &rb.for_each), &state) {
        (Some(i), _) => i,
        (None, _) if rb.mode == Mode::Managed => expand(&ev, &rb.count, &rb.for_each, &mut warns, &base),
        (None, Some(st)) if rb.count.is_some() => Instances::Count(st.keys().filter(|k| matches!(k, Key::Index(_))).count()),
        (None, Some(st)) => Instances::ForEach(
            st.keys().filter_map(|k| if let Key::Name(n) = k { Some((n.clone(), Value::Unknown)) } else { None }).collect(),
        ),
        (None, None) => expand(&ev, &rb.count, &rb.for_each, &mut warns, &base),
    };
    let mut stale = false;
    let mut fill = |key: Key, attrs: &mut Map| {
        if let Some(st) = &state {
            stale |= !st.fill(&key, attrs);
        }
    };

    let provider_key = rb.provider.clone().unwrap_or_else(|| rb.rtype.split('_').next().unwrap_or("google").to_string());
    let provider = providers
        .get(&provider_key)
        .or_else(|| providers.get(provider_key.split('.').next().unwrap_or("")))
        .or_else(|| providers.get("google"))
        .or_else(|| providers.get("google-beta"))
        .cloned()
        .unwrap_or_default();

    let module_path = addr_prefix.trim_end_matches('.').to_string();
    let refs: Vec<String> = {
        let mut r = vec![];
        body_refs(&rb.body, &mut r);
        let mut v: Vec<String> = r
            .into_iter()
            .filter(|(root, a)| a.is_some() && root.contains('_') && !root.starts_with("data."))
            .map(|(root, a)| format!("{root}.{}", a.unwrap()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let push = |address: String, attrs: &mut Map, out: &mut EvalOutput| {
        // Synthetic identifiers let other resources reference this one.
        for k in ["id", "self_link", "self_link_unique"] {
            if !matches!(attrs.get(k), Some(Value::String(_))) {
                attrs.insert(k.to_string(), Value::String(format!("{}{address}", crate::REF_PREFIX)));
            }
        }
        if rb.mode == Mode::Managed {
            out.resources.push(ResourceInstance {
                address,
                mode: rb.mode,
                rtype: rb.rtype.clone(),
                name: rb.name.clone(),
                module_path: module_path.clone(),
                attrs: Value::Map(attrs.clone()),
                provider: provider.clone(),
                file: Some(rb.file.clone()),
                warnings: warns.clone(),
                refs: refs.clone(),
            });
        }
    };

    let val = match instances {
        Instances::Single => {
            let mut attrs = eval_body(&ev, &rb.body, None, true);
            fill(Key::None, &mut attrs);
            push(base.clone(), &mut attrs, out);
            Value::Map(attrs)
        }
        Instances::Count(n) => {
            let mut list = Vec::with_capacity(n);
            for i in 0..n {
                let mut c = Map::new();
                c.insert("index".into(), Value::Number(i as f64));
                let s = Scope::new("count", Value::Map(c), None);
                let mut attrs = eval_body(&ev, &rb.body, Some(&s), true);
                fill(Key::Index(i), &mut attrs);
                push(format!("{base}[{i}]"), &mut attrs, out);
                list.push(Value::Map(attrs));
            }
            Value::List(list)
        }
        Instances::ForEach(items) => {
            let mut map = Map::new();
            for (k, v) in items {
                let mut e = Map::new();
                e.insert("key".into(), Value::String(k.clone()));
                e.insert("value".into(), v);
                let s = Scope::new("each", Value::Map(e), None);
                let mut attrs = eval_body(&ev, &rb.body, Some(&s), true);
                fill(Key::Name(k.clone()), &mut attrs);
                push(format!("{base}{}", fmt_key(&k)), &mut attrs, out);
                map.insert(k, Value::Map(attrs));
            }
            Value::Map(map)
        }
    };
    if stale {
        out.warnings.push(format!("{base}: arguments differ from the state of this configuration, its values are unknown"));
    }
    val
}
