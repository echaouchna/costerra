//! The data sources of the configuration's own state.
//!
//! `terraform plan` reads data sources (Azure AD group members, existing
//! projects, secrets…) before it can expand `for_each` / `count` built from
//! them. Statically, their values as of the last apply or refresh are the
//! best stand-in: when the arguments of a data source are the ones of its
//! state, its result there is used, so that what is built from it is known.
//!
//! Managed resources are not built from the state, but the ids Google
//! assigned to them (`uid`) are taken from it: the billing export names some
//! resources by them.
//!
//! The state is found like `terraform init` would: the backend block (or the
//! default local backend), completed by the backend configuration file of the
//! environment, or the configuration the directory was initialized with
//! (`.terraform/terraform.tfstate`) when it is the same state. Every backend
//! type is read (see `state_source`).
//!
//! Like `remote_state`, only what is needed is kept, in memory, for the
//! current process: the data sources, without their sensitive attributes and
//! without the data sources that hold secrets, and the `uid` of managed
//! resources. Nothing is printed, logged or written to the cache.

use super::evaluate::eval_body;
use super::parse::ParsedModule;
use super::state_source::{Source, initialized};
use crate::hcl::eval::Evaluator;
use crate::hcl::value::{Map, Value};
use crate::tf::terragrunt::read_tfvars;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Instance key of a resource in the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    None,
    Index(usize),
    Name(String),
}

/// Instances per data source address (`module.a.data.type.name`).
type DataSources = HashMap<String, Vec<(Key, Map)>>;
type Fetched = Result<Arc<DataSources>, String>;
/// Per state (`Source::describe`).
static CACHE: Mutex<Option<HashMap<String, Fetched>>> = Mutex::new(None);

/// Data source types whose values are secrets: never kept.
fn holds_secrets(rtype: &str) -> bool {
    ["secret", "token", "password", "credential", "private_key", "_key", "client_config", "keys"].iter().any(|w| rtype.contains(w))
}

/// The state of one root module (for one environment), read on first use.
pub struct OwnState {
    /// Where it is, or why it is not read.
    source: Result<Source, String>,
    failed: AtomicBool,
}

impl OwnState {
    /// The state of a root module, like `terraform init` finds it:
    ///
    /// * the backend block (`local` when there is none), completed by the
    ///   backend configuration file of the environment
    ///   (`backends/<env>.tfvars`, `<env>.tfbackend`…) as passed to
    ///   `terraform init -backend-config=…` in CI. The environment is the
    ///   name of its variable file: the discovered one or a `--var-file`
    ///   (`environments/prod.tfvars` → `backends/prod.tfvars`);
    /// * or, when the directory was initialized for that same state, the full
    ///   configuration it was initialized with (including
    ///   `-backend-config=key=value`), and its selected workspace;
    /// * for a backend that cannot be read directly, `terraform state pull`
    ///   in the directory initialized for it.
    ///
    /// The workspace is `workspace` (`--workspace`, `TF_WORKSPACE`), else the
    /// one selected in the directory, else `default`.
    pub fn locate(parsed: &ParsedModule, root: &Path, var_files: &[&Path], workspace: Option<&str>) -> Option<OwnState> {
        let empty = Map::new();
        let (kind, mut config) = match &parsed.backend {
            Some((kind, body)) => (kind.clone(), eval_body(&Evaluator::new(&empty, root), body, None, false)),
            None => ("local".to_string(), Map::new()),
        };
        let env_file = var_files.iter().find_map(|f| backend_config_file(root, Some(f)));
        if let Some(f) = env_file.clone().or_else(|| backend_config_file(root, None)) {
            config.extend(read_tfvars(&f));
        }
        // The initialization of the directory is for this state when the
        // settings known here are its settings. With a variable file but
        // without a backend file for it, nothing says which environment the
        // directory was initialized for: it is not used.
        let init = initialized(root).filter(|(k, c, _)| {
            *k == kind && (var_files.is_empty() || env_file.is_some()) && config.iter().all(|(key, v)| same_setting(v, c.get(key)))
        });
        let workspace =
            workspace.map(str::to_string).or_else(|| init.as_ref().and_then(|(_, _, ws)| ws.clone())).unwrap_or_else(|| "default".into());
        if let Some((_, c, _)) = &init {
            config = c.clone();
        }
        let source = match Source::from_backend(&kind, &config, &workspace, root) {
            // Without a backend and without a state file: nothing to read.
            Ok(Some(Source::Local(p))) if parsed.backend.is_none() && !p.is_file() => return None,
            Ok(Some(s)) => Ok(s),
            Ok(None) if init.is_some() => Ok(Source::Pull { dir: root.to_path_buf(), workspace }),
            Ok(None) => return None,
            Err(_) if init.is_some() => Ok(Source::Pull { dir: root.to_path_buf(), workspace }),
            Err(e) => Err(e),
        };
        Some(OwnState { source, failed: AtomicBool::new(false) })
    }

    /// The state of a backend configured elsewhere (Terragrunt `remote_state`).
    pub fn from_backend(kind: &str, config: &Map, workspace: &str, root: &Path) -> Option<OwnState> {
        let source = Source::from_backend(kind, config, workspace, root).transpose()?;
        Some(OwnState { source, failed: AtomicBool::new(false) })
    }

    /// The state instances of a data source, if its state can be read.
    fn data_source(&self, address: &str) -> Option<Vec<(Key, Map)>> {
        let Ok(source) = &self.source else {
            self.failed.store(true, Ordering::Relaxed);
            return None;
        };
        let key = source.describe();
        let fetched = {
            let cache = CACHE.lock();
            cache.as_ref().and_then(|c| c.get(&key).cloned())
        };
        let fetched = fetched.unwrap_or_else(|| {
            let r = source.fetch().map(|s| Arc::new(data_sources(&s)));
            CACHE.lock().get_or_insert_with(HashMap::new).insert(key, r.clone());
            r
        });
        match fetched {
            Ok(ds) => ds.get(address).cloned(),
            Err(_) => {
                self.failed.store(true, Ordering::Relaxed);
                None
            }
        }
    }

    /// Why the state could not be read, once it was needed.
    pub fn failure(&self) -> Option<String> {
        if !self.failed.load(Ordering::Relaxed) {
            return None;
        }
        let source = match &self.source {
            Ok(s) => s,
            Err(why) => return Some(format!("the state of this configuration is not read: {why}; data sources stay unknown")),
        };
        let cache = CACHE.lock();
        let err = cache.as_ref()?.get(&source.describe())?.as_ref().err()?.clone();
        Some(format!("cannot read the state of this configuration {} ({err}); data sources stay unknown", source.describe()))
    }

    /// The resource `address` (with its module prefix) as it is in the state:
    /// all of a data source, the `uid` of a managed resource.
    pub fn lookup(&self, address: &str, rtype: &str) -> Option<StateInstances> {
        if holds_secrets(rtype) {
            return None;
        }
        self.data_source(address).map(|instances| StateInstances { instances })
    }
}

/// A backend setting of the configuration is the one the directory was
/// initialized with (settings not known statically match anything).
fn same_setting(conf: &Value, init: Option<&Value>) -> bool {
    match (conf.to_interp_string(), init.and_then(Value::to_interp_string)) {
        (Some(a), Some(b)) => a == b,
        (Some(a), None) => a.is_empty(),
        (None, _) => true,
    }
}

pub struct StateInstances {
    instances: Vec<(Key, Map)>,
}

impl StateInstances {
    /// The keys of the instances, for a `for_each` / `count` that is not
    /// known statically.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.instances.iter().map(|(k, _)| k)
    }

    /// Complete the evaluated arguments of an instance with its attributes in
    /// the state. Nothing is changed when the instance is not in the state, or
    /// when a known argument differs from the state (it would read something
    /// else): `false` in that case.
    pub fn fill(&self, key: &Key, attrs: &mut Map) -> bool {
        let Some((_, state)) = self.instances.iter().find(|(k, _)| k == key) else { return true };
        if attrs.iter().any(|(k, v)| state.get(k).is_some_and(|s| !same(v, s))) {
            return false;
        }
        for (k, v) in state {
            match attrs.get(k) {
                Some(cur) if cur.is_wholly_known() => {}
                _ => {
                    attrs.insert(k.clone(), v.clone());
                }
            }
        }
        true
    }
}

/// An argument of the configuration matches the state (unknown and unset
/// arguments match anything; sets may be in any order).
fn same(conf: &Value, state: &Value) -> bool {
    match (conf, state) {
        (Value::Unknown | Value::Null, _) => true,
        (Value::Map(a), Value::Map(b)) => a.iter().all(|(k, v)| b.get(k).is_none_or(|w| same(v, w))),
        (Value::Map(a), Value::List(b)) if b.len() == 1 => same(&Value::Map(a.clone()), &b[0]),
        (Value::List(a), Value::List(b)) => {
            a.len() == b.len() && (a.iter().zip(b).all(|(x, y)| same(x, y)) || a.iter().all(|x| b.iter().any(|y| same(x, y))))
        }
        (a, b) => a.to_interp_string().is_some() && a.to_interp_string() == b.to_interp_string(),
    }
}

/// The data sources of a state, without sensitive attributes.
fn data_sources(state: &serde_json::Value) -> DataSources {
    let mut out = DataSources::new();
    for r in state["resources"].as_array().into_iter().flatten() {
        let (Some(rtype), Some(name)) = (r["type"].as_str(), r["name"].as_str()) else { continue };
        if rtype == "terraform_remote_state" || holds_secrets(rtype) {
            continue;
        }
        let managed = r["mode"] == "managed";
        let address = match (r["module"].as_str(), managed) {
            (Some(m), true) => format!("{m}.{rtype}.{name}"),
            (None, true) => format!("{rtype}.{name}"),
            (Some(m), false) => format!("{m}.data.{rtype}.{name}"),
            (None, false) => format!("data.{rtype}.{name}"),
        };
        let instances = r["instances"].as_array().into_iter().flatten().filter_map(state_instance).filter_map(|(key, mut attrs)| {
            // Of a managed resource, only the server-assigned ids.
            if managed {
                attrs.retain(|k, _| MANAGED_ATTRS.contains(&k.as_str()));
                if attrs.is_empty() {
                    return None;
                }
            }
            Some((key, attrs))
        });
        out.entry(address).or_default().extend(instances);
    }
    out.retain(|_, v| !v.is_empty());
    out
}

/// Attributes kept of the managed resources of the state: ids assigned by
/// Google, by which the billing export names some resources (e.g. the disks
/// and VMs of a workstation carry its `uid` as a label).
const MANAGED_ATTRS: &[&str] = &["uid"];

/// Managed resource types whose ids are read from the state (the state is
/// not read for the others).
const MANAGED_TYPES: &[&str] = &["google_workstations_workstation"];

/// Whether the state is worth reading for a managed resource of this type.
pub fn reads_managed(rtype: &str) -> bool {
    MANAGED_TYPES.contains(&rtype)
}

/// The key and the attributes of an instance of a state resource, without its
/// sensitive attributes.
pub(crate) fn state_instance(i: &serde_json::Value) -> Option<(Key, Map)> {
    let key = match &i["index_key"] {
        serde_json::Value::String(s) => Key::Name(s.clone()),
        serde_json::Value::Number(n) => Key::Index(n.as_u64()? as usize),
        _ => Key::None,
    };
    let mut sensitive = vec![];
    sensitive_names(&i["sensitive_attributes"], &mut sensitive);
    let attrs: Map = i["attributes"]
        .as_object()?
        .iter()
        .filter(|(k, _)| !sensitive.contains(k))
        .map(|(k, v)| (k.clone(), Value::from_json(v)))
        .collect();
    Some((key, attrs))
}

/// Attribute names in `sensitive_attributes`, whatever the version of its
/// format: every `get_attr` step of every path, so that a nested sensitive
/// attribute also hides the top-level one of the same name (too much rather
/// than too little).
fn sensitive_names(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Array(a) => a.iter().for_each(|x| sensitive_names(x, out)),
        serde_json::Value::Object(o) if o.get("type").and_then(|t| t.as_str()) == Some("get_attr") => {
            if let Some(n) = o.get("value").and_then(|n| n.as_str()) {
                out.push(n.to_string());
            }
        }
        _ => {}
    }
}

/// The backend configuration file of an environment, as passed to
/// `terraform init -backend-config=<file>`.
fn backend_config_file(root: &Path, env_file: Option<&Path>) -> Option<PathBuf> {
    const DIRS: &[&str] = &["backends", "backend", "backend-config", "backend-configs", "backend_config", "backend_configs"];
    let mut candidates = vec![];
    match env_file.and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()) {
        Some(name) => {
            let env = name.trim_end_matches(".json").trim_end_matches(".tfvars");
            let names = [format!("{env}.tfvars"), format!("{env}.hcl"), format!("{env}.tfbackend"), format!("{env}.backend.tfvars")];
            for d in DIRS {
                candidates.extend(names.iter().map(|n| root.join(d).join(n)));
            }
            let env_dir = env_file.and_then(Path::parent).unwrap_or(root);
            for dir in [env_dir, root] {
                for n in [
                    format!("{env}.tfbackend"),
                    format!("{env}.backend.tfvars"),
                    format!("{env}.backend.hcl"),
                    format!("backend-{env}.tfvars"),
                ] {
                    candidates.push(dir.join(n));
                }
            }
        }
        None => {
            for n in ["backend.tfvars", "backend.hcl", "backend.tfbackend", "config.gcs.tfbackend"] {
                candidates.push(root.join(n));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
pub(crate) fn reset_cache() {
    *CACHE.lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn sensitive_attributes_are_left_out() {
        let state = json!({"resources": [
            {"mode": "data", "type": "azuread_group", "name": "g", "instances": [{
                "attributes": {"members": ["a", "b"], "description": "x"},
                "sensitive_attributes": [[{"type": "get_attr", "value": "description"}, {"type": "index", "value": 0}]]
            }]},
            {"mode": "data", "type": "azuread_user", "name": "u", "module": "module.m[\"k\"]", "instances": [
                {"index_key": "a", "attributes": {"upn": "a@x"}, "sensitive_attributes": [{"type": "get_attr", "value": "mail"}]},
                {"index_key": "b", "attributes": {"upn": "b@x", "mail": "b@x"}, "sensitive_attributes": [{"type": "get_attr", "value": "mail"}]}
            ]},
            {"mode": "data", "type": "google_secret_manager_secret_version", "name": "s", "instances": [{"attributes": {"secret_data": "x"}}]},
            {"mode": "managed", "type": "google_project", "name": "p", "instances": [{"attributes": {"project_id": "p"}}]}
        ]});
        let ds = data_sources(&state);
        assert_eq!(ds.len(), 2, "{:?}", ds.keys());
        let g = &ds["data.azuread_group.g"][0];
        assert_eq!(g.0, Key::None);
        assert!(g.1.get("description").is_none() && g.1.get("members").is_some());
        let u = &ds["module.m[\"k\"].data.azuread_user.u"];
        assert_eq!(u[1].0, Key::Name("b".into()));
        assert!(u[1].1.get("mail").is_none() && u[1].1.get("upn").is_some());
    }

    #[test]
    fn state_values_fill_only_matching_instances() {
        let mut state = Map::new();
        state.insert("object_id".into(), Value::from("1"));
        state.insert("type".into(), Value::from("User"));
        let st = StateInstances { instances: vec![(Key::Name("1".into()), state)] };
        let mut attrs = Map::new();
        attrs.insert("object_id".into(), Value::from("1"));
        assert!(st.fill(&Key::Name("1".into()), &mut attrs));
        assert_eq!(attrs.get("type").and_then(Value::as_str), Some("User"));
        // Different arguments: the state result does not apply.
        let mut other = Map::new();
        other.insert("object_id".into(), Value::from("2"));
        assert!(!st.fill(&Key::Name("1".into()), &mut other));
        assert!(other.get("type").is_none());
        // Not in the state: stays unknown.
        let mut new = Map::new();
        assert!(st.fill(&Key::Name("3".into()), &mut new));
        assert!(new.is_empty());
    }

    #[test]
    fn for_each_over_data_sources_is_known_from_the_state() {
        use super::super::remote_state::tests::with_fake_gcs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backends")).unwrap();
        std::fs::create_dir_all(root.join("environments")).unwrap();
        std::fs::write(root.join("backends/prod.tfvars"), "bucket = \"b-prod\"\nprefix = \"app\"\n").unwrap();
        std::fs::write(root.join("environments/prod.tfvars"), "group = \"devs\"\nreclaimed = [\"bob\"]\n").unwrap();
        std::fs::write(
            root.join("main.tf"),
            r#"
terraform {
  backend "gcs" {}
}
variable "group" {}
variable "reclaimed" { default = [] }

data "azuread_group" "g" {
  mail_nickname = var.group
}
data "azuread_directory_object" "o" {
  for_each  = { for id in data.azuread_group.g.members : id => id }
  object_id = each.key
}
data "azuread_user" "u" {
  for_each  = { for o in data.azuread_directory_object.o : o.object_id => o.object_id if o.type == "User" }
  object_id = each.key
}
data "google_secret_manager_secret_version" "s" {
  secret = "x"
}
locals {
  users = { for u in data.azuread_user.u : lower(u.sam) => u if !contains(var.reclaimed, lower(u.sam)) }
}
resource "google_compute_disk" "home" {
  for_each = local.users
  name     = "home-${each.key}"
  zone     = "europe-west1-b"
  type     = "pd-standard"
  size     = 200
  labels   = { secret = data.google_secret_manager_secret_version.s.secret_data }
}
"#,
        )
        .unwrap();
        let ds = |rtype: &str, name: &str, instances: serde_json::Value| json!({"mode": "data", "type": rtype, "name": name, "instances": instances});
        let state = json!({"version": 4, "resources": [
            ds("azuread_group", "g", json!([{"attributes": {"mail_nickname": "devs", "members": ["1", "2", "3"]}}])),
            ds("azuread_directory_object", "o", json!([
                {"index_key": "1", "attributes": {"object_id": "1", "type": "User"}},
                {"index_key": "2", "attributes": {"object_id": "2", "type": "User"}},
                {"index_key": "3", "attributes": {"object_id": "3", "type": "Group"}}
            ])),
            ds("azuread_user", "u", json!([
                {"index_key": "1", "attributes": {"object_id": "1", "sam": "ALICE"}},
                {"index_key": "2", "attributes": {"object_id": "2", "sam": "BOB"}}
            ])),
            ds("google_secret_manager_secret_version", "s", json!([{"attributes": {"secret": "x", "secret_data": "s3cr3t"}}])),
            {"mode": "managed", "type": "google_compute_disk", "name": "home", "instances": []}
        ]});
        let projects = with_fake_gcs(
            move |req| {
                if req.path == "/storage/v1/b/b-prod/o/app%2Fdefault.tfstate" { (200, state.to_string()) } else { (404, "{}".into()) }
            },
            || {
                let opts = crate::project::LoadOptions {
                    remote_state: true,
                    cache_dir: std::env::temp_dir().join("costerra-own-state"),
                    ..Default::default()
                };
                crate::project::load(root, &opts).unwrap()
            },
        );
        let p = &projects[0];
        let disks: Vec<&str> = p.resources.iter().map(|r| r.address.as_str()).collect();
        assert_eq!(disks, ["google_compute_disk.home[\"alice\"]"], "{:?}", p.warnings);
        let labels = p.resources[0].attrs.get("labels").unwrap();
        assert!(!labels.contains_string("s3cr3t"), "secrets are never read from the state");
    }

    #[test]
    fn unreadable_state_is_a_warning_only_when_needed() {
        use super::super::remote_state::tests::with_fake_gcs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("backend.tfvars"), "bucket = \"denied\"\n").unwrap();
        let load = |tf: &str| {
            std::fs::write(root.join("main.tf"), tf).unwrap();
            super::super::parse::invalidate(root);
            let opts = crate::project::LoadOptions {
                remote_state: true,
                cache_dir: std::env::temp_dir().join("costerra-own-state"),
                ..Default::default()
            };
            with_fake_gcs(|_| (403, "{}".into()), || crate::project::load(root, &opts).unwrap()).remove(0).warnings
        };
        let backend = "terraform {\n  backend \"gcs\" {}\n}\ndata \"azuread_group\" \"g\" {\n  mail_nickname = \"x\"\n}\n";
        let known = load(&format!("{backend}resource \"google_compute_disk\" \"d\" {{\n  name = data.azuread_group.g.display_name\n}}\n"));
        assert!(known.iter().all(|w| !w.contains("gs://denied")), "{known:?}");
        let unknown = load(&format!(
            "{backend}resource \"google_compute_disk\" \"d\" {{\n  for_each = toset(data.azuread_group.g.members)\n  name = each.key\n}}\n"
        ));
        assert!(
            unknown.iter().any(|w| w.contains("gs://denied/default.tfstate") && w.contains("roles/storage.objectViewer")),
            "{unknown:?}"
        );
    }

    /// The layout of a workstations access repository: users from an Azure AD
    /// group (data sources of the own state), configs in the cluster's
    /// configuration (remote state), partial backend per environment. With
    /// `--var-file` or per-environment discovery, the workstations, their
    /// `uid` (for the billing) and their config (for the price) are known.
    #[test]
    fn workstations_of_a_group_priced_with_the_config_of_the_cluster_state() {
        use super::super::remote_state::tests::with_fake_gcs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backends")).unwrap();
        std::fs::create_dir_all(root.join("environments")).unwrap();
        std::fs::write(root.join("backends/prod.tfvars"), "bucket = \"state-prod\"\nprefix = \"access/state\"\n").unwrap();
        std::fs::write(root.join("environments/prod.tfvars"), "group = \"devs\"\n").unwrap();
        std::fs::write(
            root.join("main.tf"),
            r#"
terraform {
  backend "gcs" {}
}
variable "group" {}

data "terraform_remote_state" "cluster" {
  backend = "gcs"
  config  = { bucket = "state-prod", prefix = "cluster/state" }
}
data "azuread_group" "g" {
  mail_nickname = var.group
}
data "azuread_user" "u" {
  for_each  = toset(data.azuread_group.g.members)
  object_id = each.key
}
locals {
  configs = data.terraform_remote_state.cluster.outputs.workstation_configs
  assignments = merge([
    for u in data.azuread_user.u : {
      for k, c in local.configs : "${lower(u.sam)}-${k}" => { user = lower(u.sam), config = c }
    }
  ]...)
}
resource "google_workstations_workstation" "ws" {
  for_each               = local.assignments
  provider               = google-beta
  project                = "ws-prod"
  location               = each.value.config.location
  workstation_cluster_id = each.value.config.cluster_id
  workstation_config_id  = each.value.config.config_id
  workstation_id         = replace(each.key, "_", "-")
}
"#,
        )
        .unwrap();
        let own = json!({"version": 4, "resources": [
            {"mode": "data", "type": "azuread_group", "name": "g", "instances": [{"attributes": {"mail_nickname": "devs", "members": ["1", "2"]}}]},
            {"mode": "data", "type": "azuread_user", "name": "u", "instances": [
                {"index_key": "1", "attributes": {"object_id": "1", "sam": "ALICE"}},
                {"index_key": "2", "attributes": {"object_id": "2", "sam": "BOB"}}
            ]},
            {"mode": "managed", "type": "google_workstations_workstation", "name": "ws", "instances": [
                {"index_key": "alice-rstudio", "attributes": {"uid": "uid-alice", "env": {"TOKEN": "x"}}},
                {"index_key": "bob-rstudio", "attributes": {"uid": "uid-bob"}}
            ]}
        ]});
        let cluster = json!({"version": 4,
            "outputs": {"workstation_configs": {"value": {"rstudio": {"cluster_id": "euw1", "config_id": "rstudio-v2", "location": "europe-west1"}}}},
            "resources": [{"mode": "managed", "type": "google_workstations_workstation_config", "name": "cfg", "instances": [
                {"index_key": "rstudio", "attributes": {
                    "workstation_config_id": "rstudio-v2", "workstation_cluster_id": "euw1", "location": "europe-west1", "project": "ws-prod",
                    "host": [{"gce_instance": [{"machine_type": "e2-standard-8", "boot_disk_size_gb": 200, "pool_size": 0, "accelerators": []}]}],
                    "persistent_directories": [{"mount_path": "/home", "gce_hd": [], "gce_pd": [{"disk_type": "pd-ssd", "size_gb": 100}]}]
                }},
                // Same config id in another cluster: not the one of the workstations.
                {"index_key": "other", "attributes": {"workstation_config_id": "rstudio-v2", "workstation_cluster_id": "euw4", "location": "europe-west4"}}
            ]}]
        });
        let load = |var_files: Vec<PathBuf>| {
            let (own, cluster) = (own.to_string(), cluster.to_string());
            with_fake_gcs(
                move |req| match req.path.as_str() {
                    "/storage/v1/b/state-prod/o/access%2Fstate%2Fdefault.tfstate" => (200, own.clone()),
                    "/storage/v1/b/state-prod/o/cluster%2Fstate%2Fdefault.tfstate" => (200, cluster.clone()),
                    _ => (404, "{}".into()),
                },
                || {
                    let opts = crate::project::LoadOptions {
                        remote_state: true,
                        var_files,
                        cache_dir: std::env::temp_dir().join("costerra-own-state"),
                        ..Default::default()
                    };
                    crate::project::load(root, &opts).unwrap()
                },
            )
        };
        for (how, var_files) in [("discovered", vec![]), ("--var-file", vec![root.join("environments/prod.tfvars")])] {
            let projects = load(var_files);
            let p = &projects[0];
            let ws: Vec<(&str, Option<&str>)> =
                p.resources.iter().map(|r| (r.address.as_str(), r.attrs.get("uid").and_then(Value::as_str))).collect();
            assert_eq!(
                ws,
                [
                    ("google_workstations_workstation.ws[\"alice-rstudio\"]", Some("uid-alice")),
                    ("google_workstations_workstation.ws[\"bob-rstudio\"]", Some("uid-bob"))
                ],
                "{how}: {:?}",
                p.warnings
            );
            assert!(p.resources[0].attrs.get("env").is_none(), "{how}: only the uid of managed resources is read");
            // Billing: matched by the uid label.
            let stack = crate::billing::stack_of(&[&projects], |_| None);
            assert_eq!(stack.workstations.iter().map(String::as_str).collect::<Vec<_>>(), ["ws-prod/uid-alice", "ws-prod/uid-bob"]);
            // Price: the config of the cluster state, not the default one.
            let (pricer, config, usage) =
                (crate::pricing::query::Pricer::new(None, false), crate::config::Config::default(), Default::default());
            let engine = crate::engine::Engine {
                pricer: &pricer,
                list_pricer: None,
                usage_profile: None,
                examples: false,
                usage: &usage,
                config: &config,
                recommendations: false,
                observed: None,
            };
            let r = engine.estimate_project(p);
            let ws = &r.resources[0];
            assert!(ws.warnings.iter().all(|w| !w.contains("config not found")), "{how}: {:?}", ws.warnings);
            let names: Vec<&str> = ws.components.iter().map(|c| c.name.as_str()).collect();
            assert!(names.iter().any(|n| n.contains("e2-standard-8")), "{how}: {names:?}");
            assert!(names.iter().any(|n| n.contains("pd-ssd") && n.contains("100 GiB")), "{how}: {names:?}");
        }
    }

    /// A configuration whose `for_each` is built from a data source, with a
    /// state holding the members of the group.
    fn group_config(root: &Path, backend: &str) {
        std::fs::write(
            root.join("main.tf"),
            format!(
                r#"
{backend}
data "azuread_group" "g" {{
  mail_nickname = "devs"
}}
resource "google_compute_disk" "home" {{
  for_each = toset(data.azuread_group.g.members)
  name     = "home-${{each.key}}"
  zone     = "europe-west1-b"
  size     = 10
}}
"#
            ),
        )
        .unwrap();
    }

    fn group_state() -> String {
        json!({"version": 4, "resources": [
            {"mode": "data", "type": "azuread_group", "name": "g", "instances": [{"attributes": {"mail_nickname": "devs", "members": ["alice", "bob"]}}]}
        ]})
        .to_string()
    }

    fn load(root: &Path, var_files: Vec<PathBuf>) -> crate::project::Project {
        super::super::parse::invalidate(root);
        reset_cache();
        let opts = crate::project::LoadOptions {
            remote_state: true,
            var_files,
            cache_dir: std::env::temp_dir().join("costerra-own-state"),
            ..Default::default()
        };
        crate::project::load(root, &opts).unwrap().remove(0)
    }

    fn disks(p: &crate::project::Project) -> Vec<&str> {
        p.resources.iter().map(|r| r.address.as_str()).collect()
    }

    /// No backend block: Terraform's default local state file.
    #[test]
    fn default_local_state() {
        let _g = super::super::remote_state::tests::SERIAL.lock();
        let dir = tempfile::tempdir().unwrap();
        group_config(dir.path(), "");
        std::fs::write(dir.path().join("terraform.tfstate"), group_state()).unwrap();
        let p = load(dir.path(), vec![]);
        assert_eq!(disks(&p), ["google_compute_disk.home[\"alice\"]", "google_compute_disk.home[\"bob\"]"], "{:?}", p.warnings);
    }

    /// The configuration `terraform init` was run with (e.g. with
    /// `-backend-config=path=…`) and the workspace it selected.
    #[test]
    fn state_of_the_initialized_directory_and_its_workspace() {
        let _g = super::super::remote_state::tests::SERIAL.lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        group_config(root, "terraform {\n  backend \"local\" {}\n}");
        std::fs::create_dir_all(root.join(".terraform")).unwrap();
        std::fs::create_dir_all(root.join("states/staging")).unwrap();
        std::fs::write(
            root.join(".terraform/terraform.tfstate"),
            r#"{"backend": {"type": "local", "config": {"path": null, "workspace_dir": "states"}}}"#,
        )
        .unwrap();
        std::fs::write(root.join(".terraform/environment"), "staging").unwrap();
        std::fs::write(root.join("states/staging/terraform.tfstate"), group_state()).unwrap();
        let p = load(root, vec![]);
        assert_eq!(disks(&p).len(), 2, "{:?}", p.warnings);
    }

    /// A backend that is not read directly: `terraform state pull` in the
    /// directory initialized for it; without initialization, a warning says
    /// why the state is not read.
    #[test]
    fn other_backends_with_terraform_state_pull() {
        let _g = super::super::remote_state::tests::SERIAL.lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        group_config(root, "terraform {\n  backend \"pg\" {}\n}");
        let p = load(root, vec![]);
        assert_eq!(disks(&p), ["google_compute_disk.home[\"<unknown>\"]"]);
        assert!(p.warnings.iter().any(|w| w.contains("the pg backend is only read with `terraform state pull`")), "{:?}", p.warnings);

        std::fs::create_dir_all(root.join(".terraform")).unwrap();
        std::fs::write(
            root.join(".terraform/terraform.tfstate"),
            r#"{"backend": {"type": "pg", "config": {"conn_str": "postgres://db"}}}"#,
        )
        .unwrap();
        let bin = dir.path().join("fake-terraform");
        std::fs::write(root.join("pulled.tfstate"), group_state()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\n[ \"$1 $2\" = \"state pull\" ] && [ \"$TF_WORKSPACE\" = default ] && cat pulled.tfstate\n")
            .unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // SAFETY: tests touching the process environment hold SERIAL.
        unsafe { std::env::set_var("COSTERRA_TERRAFORM_BIN", &bin) };
        let p = load(root, vec![]);
        unsafe { std::env::remove_var("COSTERRA_TERRAFORM_BIN") };
        assert_eq!(disks(&p).len(), 2, "{:?}", p.warnings);
    }

    /// A Terragrunt unit: its `remote_state` block locates its state.
    #[test]
    fn terragrunt_remote_state() {
        let _g = super::super::remote_state::tests::SERIAL.lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("modules/app")).unwrap();
        std::fs::create_dir_all(root.join("live/prod")).unwrap();
        group_config(&root.join("modules/app"), "");
        std::fs::write(
            root.join("live/prod/terragrunt.hcl"),
            "terraform {\n  source = \"../../modules/app\"\n}\nremote_state {\n  backend = \"local\"\n  config = {\n    path = \"unit.tfstate\"\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("live/prod/unit.tfstate"), group_state()).unwrap();
        reset_cache();
        let opts = crate::project::LoadOptions {
            remote_state: true,
            cache_dir: std::env::temp_dir().join("costerra-own-state"),
            ..Default::default()
        };
        let projects = crate::project::load(&root.join("live"), &opts).unwrap();
        let p = projects.iter().find(|p| p.name.contains("prod")).unwrap();
        assert_eq!(disks(p).len(), 2, "{:?}", p.warnings);
    }

    /// `terraform_remote_state` with another backend than gcs.
    #[test]
    fn remote_state_of_a_local_backend() {
        let _g = super::super::remote_state::tests::SERIAL.lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("network.tfstate"), json!({"version": 4, "outputs": {"zone": {"value": "europe-west1-b"}}}).to_string())
            .unwrap();
        std::fs::write(
            root.join("app/main.tf"),
            r#"
data "terraform_remote_state" "network" {
  backend = "local"
  config  = { path = "../network.tfstate" }
}
resource "google_compute_disk" "d" {
  name = "d"
  zone = data.terraform_remote_state.network.outputs.zone
  size = 10
}
"#,
        )
        .unwrap();
        let p = load(&root.join("app"), vec![]);
        assert_eq!(p.resources[0].attrs.get("zone").and_then(Value::as_str), Some("europe-west1-b"), "{:?}", p.warnings);
    }

    #[test]
    fn backend_config_of_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backends")).unwrap();
        std::fs::create_dir_all(root.join("environments")).unwrap();
        std::fs::write(root.join("backends/prod.tfvars"), "bucket = \"b-prod\"\nprefix = \"terraform/app/state\"\n").unwrap();
        std::fs::write(root.join("main.tf"), "terraform {\n  backend \"gcs\" {}\n}\n").unwrap();
        let parsed = super::super::parse::parse_module_dir(root);
        let s = OwnState::locate(&parsed, root, &[&root.join("environments/prod.tfvars")], None).unwrap();
        assert_eq!(s.source.unwrap().describe(), "gs://b-prod/terraform/app/state/default.tfstate");
        assert!(OwnState::locate(&parsed, root, &[&root.join("environments/dev.tfvars")], None).is_none());
    }
}
