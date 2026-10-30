//! `data "terraform_remote_state"`: the outputs of the other configuration
//! are read from its state, when the credentials allow it, so that values
//! such as project ids are known statically. Every backend type is read (see
//! `state_source`).
//!
//! The resources of the other configuration that the resources of this one
//! refer to by id (workstation configs and clusters) are read too, for
//! lookups only: e.g. the machine type and disks of the config of a
//! workstation declared here. They are never priced.
//!
//! State files can hold secrets: only the outputs and those resources are
//! kept, in memory, for the current process; sensitive outputs and
//! attributes are left out; nothing is printed, logged or written to the
//! cache.

use super::evaluate::{ProviderCfg, ResourceInstance};
use super::own_state::{Key, state_instance};
use super::parse::Mode;
use super::state_source::Source;
use crate::hcl::value::{Map, Value};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Managed resources of other configurations that resources refer to by id.
const LOOKUP_TYPES: &[&str] = &["google_workstations_workstation_config", "google_workstations_workstation_cluster"];

/// The GCS API (replaced by a local server in tests).
static API: Mutex<Option<String>> = Mutex::new(None);

fn api() -> String {
    API.lock().clone().unwrap_or_else(|| "https://storage.googleapis.com".into())
}

/// What is kept of a state: its outputs, and the resources of `LOOKUP_TYPES`
/// (module address, type, name, instance key, attributes).
#[derive(Default)]
struct Remote {
    outputs: Map,
    resources: Vec<(Option<String>, String, String, Key, Map)>,
}

/// Per state (`Source::describe`), or why it could not be read.
type Fetched = Result<Arc<Remote>, String>;
static CACHE: Mutex<Option<HashMap<String, Fetched>>> = Mutex::new(None);
/// Token for GCS, obtained once.
static TOKEN: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Fill `outputs` of evaluated `terraform_remote_state` data sources (one
/// instance, or the instances of `count` / `for_each`), and add the resources
/// of the other configuration used for lookups to `external`. Problems are
/// returned as warnings; the outputs then stay unknown. `dir` is the
/// directory of the module (relative paths of the local backend).
pub fn resolve(val: Value, warnings: &mut Vec<String>, address: &str, external: &mut Vec<ResourceInstance>, dir: &Path) -> Value {
    match val {
        Value::Map(m) if m.contains_key("backend") => Value::Map(resolve_one(m, warnings, address, external, dir)),
        Value::List(l) => Value::List(l.into_iter().map(|v| resolve(v, warnings, address, external, dir)).collect()),
        Value::Map(m) => Value::Map(m.into_iter().map(|(k, v)| (k, resolve(v, warnings, address, external, dir))).collect()),
        v => v,
    }
}

fn resolve_one(mut attrs: Map, warnings: &mut Vec<String>, address: &str, external: &mut Vec<ResourceInstance>, dir: &Path) -> Map {
    let Some(kind) = attrs.get("backend").and_then(Value::as_str).map(str::to_string) else { return attrs };
    let config = attrs.get("config").and_then(Value::as_map).cloned().unwrap_or_default();
    let workspace = attrs.get("workspace").and_then(Value::as_str).unwrap_or("default").to_string();
    let source = match Source::from_backend(&kind, &config, &workspace, dir) {
        Ok(Some(s)) => s,
        // Its location is not known statically.
        Ok(None) => return attrs,
        Err(e) => {
            warnings.push(format!("{address}: {e}; its outputs are unknown"));
            return attrs;
        }
    };
    let key = source.describe();
    let fetched = {
        let mut cache = CACHE.lock();
        cache.get_or_insert_with(HashMap::new).get(&key).cloned()
    };
    let fetched = fetched.unwrap_or_else(|| {
        let r = fetch(&source);
        CACHE.lock().get_or_insert_with(HashMap::new).insert(key.clone(), r.clone());
        r
    });
    match fetched {
        Ok(remote) => {
            // `defaults` apply to outputs the state does not have.
            let mut merged = match attrs.get("defaults") {
                Some(Value::Map(d)) => d.clone(),
                _ => Map::new(),
            };
            merged.extend(remote.outputs.clone());
            attrs.insert("outputs".into(), Value::Map(merged));
            for (module, rtype, name, key, a) in &remote.resources {
                let index = match key {
                    Key::None => String::new(),
                    Key::Index(i) => format!("[{i}]"),
                    Key::Name(n) => format!("[{}]", serde_json::Value::String(n.clone())),
                };
                let module = module.as_deref().map(|m| format!("{m}.")).unwrap_or_default();
                let addr = format!("{address}:{module}{rtype}.{name}{index}");
                if external.iter().any(|e| e.address == addr) {
                    continue;
                }
                let project = a.get("project").and_then(Value::as_str).map(str::to_string);
                external.push(ResourceInstance {
                    address: addr,
                    mode: Mode::Managed,
                    rtype: rtype.clone(),
                    name: name.clone(),
                    module_path: String::new(),
                    attrs: Value::Map(a.clone()),
                    provider: ProviderCfg { project, ..Default::default() },
                    file: None,
                    warnings: vec![],
                    refs: vec![],
                });
            }
        }
        Err(e) => warnings.push(format!("{address}: cannot read {key} ({e}); its outputs are unknown")),
    }
    attrs
}

fn token() -> Option<String> {
    let mut t = TOKEN.lock();
    if let Some(v) = t.as_ref() {
        return v.clone();
    }
    // In a thread of its own: the caller may run inside an async runtime.
    let got = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        match rt.block_on(crate::pricing::sync::resolve_auth(None)) {
            Ok(crate::pricing::sync::Auth::Bearer(t)) => Some(t),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!("remote state: no credentials: {e:#}");
                None
            }
        }
    })
    .join()
    .ok()
    .flatten();
    *t = Some(got.clone());
    got
}

/// Object of the state of a workspace, with the gcs backend.
pub(crate) fn object_name(prefix: &str, workspace: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() { format!("{workspace}.tfstate") } else { format!("{prefix}/{workspace}.tfstate") }
}

fn fetch(source: &Source) -> Fetched {
    let state = source.fetch()?;
    let mut remote = Remote::default();
    for (name, o) in state["outputs"].as_object().into_iter().flatten() {
        // Sensitive outputs stay unknown (they are never needed for costs).
        if o["sensitive"].as_bool() == Some(true) {
            continue;
        }
        remote.outputs.insert(name.clone(), Value::from_json(&o["value"]));
    }
    for r in state["resources"].as_array().into_iter().flatten() {
        let (Some(rtype), Some(name)) = (r["type"].as_str(), r["name"].as_str()) else { continue };
        if r["mode"] != "managed" || !LOOKUP_TYPES.contains(&rtype) {
            continue;
        }
        let module = r["module"].as_str().map(str::to_string);
        for (key, attrs) in r["instances"].as_array().into_iter().flatten().filter_map(state_instance) {
            remote.resources.push((module.clone(), rtype.to_string(), name.to_string(), key, attrs));
        }
    }
    Ok(Arc::new(remote))
}

/// The state object, parsed. Callers keep only what they need of it.
pub(crate) fn fetch_state(bucket: &str, object: &str) -> Result<serde_json::Value, String> {
    let token = token().ok_or("no Google credentials")?;
    let url = format!("{}/storage/v1/b/{bucket}/o/{}", api(), encode(object));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let r = client.get(&url).query(&[("alt", "media")]).bearer_auth(token).send().map_err(|e| e.without_url().to_string())?;
    match r.status().as_u16() {
        200 => {}
        401 | 403 => return Err("permission denied: grant roles/storage.objectViewer on the bucket".into()),
        404 => return Err("not found".into()),
        s => return Err(format!("HTTP {s}")),
    }
    r.json().map_err(|_| "not a state file".to_string())
}

/// Percent-encoding of an object name in a URL path.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::util::fake_http;
    use serde_json::json;

    /// Remote state tests share process-wide state: one at a time.
    pub static SERIAL: Mutex<()> = Mutex::new(());

    /// Serve states from a fake GCS, with a token, for the duration of a test.
    pub fn with_fake_gcs<T>(handler: impl Fn(&fake_http::Request) -> (u16, String) + Send + 'static, f: impl FnOnce() -> T) -> T {
        let _g = SERIAL.lock();
        let server = fake_http::serve(handler);
        *API.lock() = Some(server.url.clone());
        *TOKEN.lock() = Some(Some("t0k3n".into()));
        *CACHE.lock() = None;
        super::super::own_state::reset_cache();
        let out = f();
        *API.lock() = None;
        *TOKEN.lock() = None;
        *CACHE.lock() = None;
        out
    }

    fn state_ds(bucket: &str, prefix: &str) -> Value {
        let mut config = Map::new();
        config.insert("bucket".into(), Value::from(bucket));
        config.insert("prefix".into(), Value::from(prefix));
        let mut m = Map::new();
        m.insert("backend".into(), Value::from("gcs"));
        m.insert("config".into(), Value::Map(config));
        Value::Map(m)
    }

    #[test]
    fn outputs_are_read_and_sensitive_ones_left_out() {
        let served = with_fake_gcs(
            |req| {
                assert_eq!(req.path, "/storage/v1/b/tf-state/o/team%2Fplat%2Fdefault.tfstate");
                assert_eq!(req.param("alt"), Some("media"));
                assert_eq!(req.header("authorization"), Some("Bearer t0k3n"));
                let state = json!({"version": 4, "outputs": {
                    "projects": {"value": {"app": "app-prod-1234"}, "type": ["map", "string"]},
                    "db_password": {"value": "s3cr3t", "type": "string", "sensitive": true}
                }, "resources": [{"secret": "x"}]});
                (200, state.to_string())
            },
            || {
                let mut w = vec![];
                let v =
                    resolve(state_ds("tf-state", "team/plat/"), &mut w, "data.terraform_remote_state.team", &mut vec![], Path::new("."));
                // Read once per process.
                let again =
                    resolve(state_ds("tf-state", "team/plat"), &mut w, "data.terraform_remote_state.team", &mut vec![], Path::new("."));
                (v, again, w)
            },
        );
        let (v, again, w) = served;
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(v.path("outputs.projects.app").and_then(Value::as_str), Some("app-prod-1234"));
        assert!(v.path("outputs.db_password").is_none(), "sensitive outputs stay unknown");
        assert_eq!(v.get("outputs"), again.get("outputs"));
    }

    #[test]
    fn unreadable_states_are_warnings() {
        let w = with_fake_gcs(
            |req| if req.path.contains("denied") { (403, "{}".into()) } else { (404, "{}".into()) },
            || {
                let mut w = vec![];
                let v = resolve(state_ds("denied", "x"), &mut w, "data.terraform_remote_state.a", &mut vec![], Path::new("."));
                assert!(v.get("outputs").is_none());
                resolve(state_ds("missing", "x"), &mut w, "data.terraform_remote_state.b", &mut vec![], Path::new("."));
                // Other backends and unknown buckets are left alone.
                let mut other = Map::new();
                other.insert("backend".into(), Value::from("s3"));
                resolve(Value::Map(other), &mut w, "data.terraform_remote_state.c", &mut vec![], Path::new("."));
                w
            },
        );
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("gs://denied/x/default.tfstate") && w[0].contains("roles/storage.objectViewer"), "{w:?}");
        assert!(w[1].contains("not found"), "{w:?}");
    }

    #[test]
    fn project_ids_from_remote_state_are_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.tf"),
            r#"
variable "state_bucket" { default = "my-tf-state-dev" }

data "terraform_remote_state" "team" {
  backend = "gcs"
  config = {
    bucket = var.state_bucket
    prefix = "team-managed-resources"
  }
}

locals {
  project_id = data.terraform_remote_state.team.outputs.applications_project_id["pbi"]
}

resource "google_compute_instance" "vm" {
  project      = local.project_id
  name         = "powerbi-vm"
  machine_type = "n2-standard-4"
  zone         = "europe-west1-b"
}
"#,
        )
        .unwrap();
        let projects = with_fake_gcs(
            |req| {
                if req.path == "/storage/v1/b/my-tf-state-dev/o/team-managed-resources%2Fdefault.tfstate" {
                    let state = json!({"outputs": {"applications_project_id": {"value": {"pbi": "pbivm-plat-dev-de66"}}}});
                    (200, state.to_string())
                } else {
                    (404, "{}".into())
                }
            },
            || {
                let opts = crate::project::LoadOptions {
                    remote_state: true,
                    cache_dir: std::env::temp_dir().join("costerra-rs"),
                    ..Default::default()
                };
                crate::project::load(dir.path(), &opts).unwrap()
            },
        );
        let vm = projects[0].resources.iter().find(|r| r.rtype == "google_compute_instance").unwrap();
        assert_eq!(vm.attrs.get("project").and_then(Value::as_str), Some("pbivm-plat-dev-de66"));
    }

    #[test]
    fn each_environment_reads_its_own_state() {
        let dir = tempfile::tempdir().unwrap();
        let w = |f: &str, src: &str| {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        };
        w(
            "main.tf",
            r#"variable "state_bucket" {}
data "terraform_remote_state" "team" {
  backend = "gcs"
  config  = { bucket = var.state_bucket, prefix = "team" }
}
resource "google_storage_bucket" "b" {
  name     = "data"
  location = "EU"
  project  = data.terraform_remote_state.team.outputs.project_id
}
"#,
        );
        w("environments/dev.tfvars", "state_bucket = \"state-dev\"\n");
        w("environments/prod.tfvars", "state_bucket = \"state-prod\"\n");
        let project_of = |p: &crate::project::Project| p.resources[0].attrs.get("project").and_then(Value::as_str).map(str::to_string);
        let (all, prod_only) = with_fake_gcs(
            |req| {
                let project = match req.path.as_str() {
                    "/storage/v1/b/state-dev/o/team%2Fdefault.tfstate" => "app-dev",
                    "/storage/v1/b/state-prod/o/team%2Fdefault.tfstate" => "app-prod",
                    _ => return (404, "{}".into()),
                };
                (200, json!({"outputs": {"project_id": {"value": project}}}).to_string())
            },
            || {
                let base = crate::project::LoadOptions {
                    remote_state: true,
                    cache_dir: std::env::temp_dir().join("costerra-rs"),
                    ..Default::default()
                };
                // No var file: one project per environment, each with its own state.
                let all = crate::project::load(dir.path(), &base).unwrap();
                // A var file: that environment's state.
                let opts = crate::project::LoadOptions {
                    var_files: vec![dir.path().join("environments/prod.tfvars")],
                    no_env_discovery: true,
                    ..base.clone()
                };
                (all, crate::project::load(dir.path(), &opts).unwrap())
            },
        );
        let by_env: Vec<(String, Option<String>)> =
            all.iter().map(|p| (p.name.rsplit(' ').next().unwrap().to_string(), project_of(p))).collect();
        assert_eq!(by_env, vec![("(dev)".into(), Some("app-dev".into())), ("(prod)".into(), Some("app-prod".into()))]);
        assert_eq!(project_of(&prod_only[0]).as_deref(), Some("app-prod"));
    }

    #[test]
    fn not_read_when_disabled_or_offline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.tf"),
            r#"data "terraform_remote_state" "t" {
  backend = "gcs"
  config  = { bucket = "b" }
}
resource "google_storage_bucket" "b" {
  name     = "x"
  location = "EU"
  project  = data.terraform_remote_state.t.outputs.project
}
"#,
        )
        .unwrap();
        let requests = with_fake_gcs(
            |_| (200, json!({"outputs": {"project": {"value": "p"}}}).to_string()),
            || {
                for (remote_state, offline) in [(false, false), (true, true)] {
                    let opts = crate::project::LoadOptions {
                        remote_state,
                        offline,
                        cache_dir: std::env::temp_dir().join("costerra-rs"),
                        ..Default::default()
                    };
                    let p = crate::project::load(dir.path(), &opts).unwrap();
                    assert!(p[0].resources[0].attrs.get("project").is_none_or(|v| v.as_str() != Some("p")));
                }
                CACHE.lock().as_ref().map_or(0, |c| c.len())
            },
        );
        assert_eq!(requests, 0);
    }
}
