//! Where a Terraform state is, from a backend configuration, and how to read
//! it (read only) for every backend type:
//!
//! * `gcs`: the Cloud Storage API, with the Google credentials;
//! * `local`: the file;
//! * `s3`: `aws s3 cp` (profiles, SSO, assumed roles… as configured for the
//!   AWS CLI);
//! * `azurerm`: `az storage blob download` (Azure CLI login, SAS token or
//!   access key);
//! * `http`: a GET of the address (basic auth from the configuration or
//!   `TF_HTTP_USERNAME` / `TF_HTTP_PASSWORD`);
//! * `remote` / `cloud` (HCP Terraform, Terraform Enterprise): the API, with
//!   the token of `TF_TOKEN_<host>` or of `terraform login`;
//! * any other backend: `terraform state pull` in a directory initialized for
//!   that state.
//!
//! Callers keep only what they need of a state, in memory: it can hold
//! secrets.

use crate::hcl::value::{Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Gcs {
        bucket: String,
        object: String,
    },
    Local(PathBuf),
    S3 {
        bucket: String,
        key: String,
        region: Option<String>,
        profile: Option<String>,
        endpoint: Option<String>,
    },
    Azure {
        account: String,
        container: String,
        blob: String,
        sas: Option<String>,
        access_key: Option<String>,
    },
    Http {
        address: String,
        username: Option<String>,
        password: Option<String>,
    },
    Tfc {
        host: String,
        organization: String,
        workspace: String,
    },
    /// `terraform state pull` in an initialized directory.
    Pull {
        dir: PathBuf,
        workspace: String,
    },
}

/// A string setting known statically.
fn get(config: &Map, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).filter(|s| !s.is_empty() && !s.starts_with(crate::REF_PREFIX)).map(str::to_string)
}

/// A nested block or object (`workspaces { … }` or `workspaces = { … }`).
fn nested<'a>(config: &'a Map, key: &str) -> Option<&'a Map> {
    match config.get(key)? {
        Value::Map(m) => Some(m),
        Value::List(l) => l.first()?.as_map(),
        _ => None,
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl Source {
    /// The state of `workspace` for a backend configuration. `Ok(None)` when
    /// a required setting is not known statically; `Err` when the backend
    /// cannot be read. `root` is the directory Terraform runs in (relative
    /// paths of the local backend).
    pub fn from_backend(kind: &str, config: &Map, workspace: &str, root: &Path) -> Result<Option<Source>, String> {
        let default = workspace == "default";
        Ok(Some(match kind {
            "gcs" => {
                let Some(bucket) = get(config, "bucket") else { return Ok(None) };
                let prefix = get(config, "prefix").unwrap_or_default();
                Source::Gcs { bucket, object: super::remote_state::object_name(&prefix, workspace) }
            }
            "local" => {
                let path = if default {
                    get(config, "path").unwrap_or_else(|| "terraform.tfstate".into())
                } else {
                    let dir = get(config, "workspace_dir").unwrap_or_else(|| "terraform.tfstate.d".into());
                    format!("{dir}/{workspace}/terraform.tfstate")
                };
                Source::Local(root.join(path))
            }
            "s3" => {
                let (Some(bucket), Some(key)) = (get(config, "bucket"), get(config, "key")) else { return Ok(None) };
                let key = if default {
                    key
                } else {
                    format!("{}/{workspace}/{key}", get(config, "workspace_key_prefix").unwrap_or_else(|| "env:".into()))
                };
                let endpoint = get(config, "endpoint").or_else(|| nested(config, "endpoints").and_then(|e| get(e, "s3")));
                Source::S3 { bucket, key, region: get(config, "region"), profile: get(config, "profile"), endpoint }
            }
            "azurerm" => {
                let (Some(account), Some(container), Some(key)) =
                    (get(config, "storage_account_name"), get(config, "container_name"), get(config, "key"))
                else {
                    return Ok(None);
                };
                Source::Azure {
                    account,
                    container,
                    blob: if default { key } else { format!("{key}env:{workspace}") },
                    sas: get(config, "sas_token").or_else(|| env("ARM_SAS_TOKEN")),
                    access_key: get(config, "access_key").or_else(|| env("ARM_ACCESS_KEY")),
                }
            }
            "http" => {
                let Some(address) = get(config, "address").or_else(|| env("TF_HTTP_ADDRESS")) else { return Ok(None) };
                Source::Http {
                    address,
                    username: get(config, "username").or_else(|| env("TF_HTTP_USERNAME")),
                    password: get(config, "password").or_else(|| env("TF_HTTP_PASSWORD")),
                }
            }
            "remote" | "cloud" => {
                let host = get(config, "hostname")
                    .or_else(|| (kind == "cloud").then(|| env("TF_CLOUD_HOSTNAME")).flatten())
                    .unwrap_or_else(|| "app.terraform.io".into());
                let organization =
                    get(config, "organization").or_else(|| (kind == "cloud").then(|| env("TF_CLOUD_ORGANIZATION")).flatten());
                let Some(organization) = organization else { return Ok(None) };
                let ws = nested(config, "workspaces");
                let name = ws.and_then(|w| get(w, "name")).or_else(|| match ws.and_then(|w| get(w, "prefix")) {
                    Some(p) => Some(format!("{p}{workspace}")),
                    // `cloud` with tags: the workspace selected.
                    None => (!default).then(|| workspace.to_string()),
                });
                let Some(workspace) = name else { return Ok(None) };
                Source::Tfc { host, organization, workspace }
            }
            other => return Err(format!("the {other} backend is only read with `terraform state pull` in an initialized directory")),
        }))
    }

    /// Where the state is, for messages (never with credentials).
    pub fn describe(&self) -> String {
        match self {
            Source::Gcs { bucket, object } => format!("gs://{bucket}/{object}"),
            Source::Local(p) => p.display().to_string(),
            Source::S3 { bucket, key, .. } => format!("s3://{bucket}/{key}"),
            Source::Azure { account, container, blob, .. } => format!("azurerm://{account}/{container}/{blob}"),
            Source::Http { address, .. } => match reqwest::Url::parse(address) {
                Ok(mut u) => {
                    let _ = u.set_password(None);
                    let _ = u.set_username("");
                    u.set_query(None);
                    u.to_string()
                }
                Err(_) => "http backend".into(),
            },
            Source::Tfc { host, organization, workspace } => format!("{host}/{organization}/{workspace}"),
            Source::Pull { dir, workspace } => format!("`terraform state pull` (workspace {workspace}) in {}", dir.display()),
        }
    }

    /// The state, parsed.
    pub fn fetch(&self) -> Result<serde_json::Value, String> {
        let parse = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).map_err(|_| "not a state file".to_string());
        match self {
            Source::Gcs { bucket, object } => super::remote_state::fetch_state(bucket, object),
            Source::Local(p) => match std::fs::read(p) {
                Ok(b) => parse(&b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("not found".into()),
                Err(e) => Err(e.to_string()),
            },
            Source::S3 { bucket, key, region, profile, endpoint } => {
                let mut c = Command::new("aws");
                c.args(["s3", "cp", &format!("s3://{bucket}/{key}"), "-", "--only-show-errors"]);
                for (flag, v) in [("--region", region), ("--profile", profile), ("--endpoint-url", endpoint)] {
                    if let Some(v) = v {
                        c.args([flag, v]);
                    }
                }
                parse(&run(c, "aws (AWS CLI)")?)
            }
            Source::Azure { account, container, blob, sas, access_key } => {
                let mut c = Command::new("az");
                c.args(["storage", "blob", "download", "--account-name", account, "--container-name", container, "--name", blob]);
                c.args(["--file", "/dev/stdout", "--no-progress", "--only-show-errors", "--output", "none"]);
                match (sas, access_key) {
                    (Some(s), _) => c.args(["--sas-token", s]),
                    (None, Some(k)) => c.args(["--account-key", k]),
                    (None, None) => c.args(["--auth-mode", "login"]),
                };
                parse(&run(c, "az (Azure CLI)")?)
            }
            Source::Http { address, username, password } => {
                let mut rq = client()?.get(address);
                if let Some(u) = username {
                    rq = rq.basic_auth(u, password.as_ref());
                }
                let r = rq.send().map_err(|e| e.without_url().to_string())?;
                match r.status().as_u16() {
                    200 => parse(&r.bytes().map_err(|e| e.to_string())?),
                    204 | 404 => Err("not found".into()),
                    401 | 403 => Err("permission denied".into()),
                    s => Err(format!("HTTP {s}")),
                }
            }
            Source::Tfc { host, organization, workspace } => fetch_tfc(host, organization, workspace),
            Source::Pull { dir, workspace } => {
                let bin =
                    env("COSTERRA_TERRAFORM_BIN").unwrap_or_else(|| if which("terraform") { "terraform".into() } else { "tofu".into() });
                let mut c = Command::new(&bin);
                c.current_dir(dir).args(["state", "pull"]).env("TF_WORKSPACE", workspace).env("TF_IN_AUTOMATION", "1").env("TF_INPUT", "0");
                let out = run(c, &bin)?;
                if out.iter().all(u8::is_ascii_whitespace) {
                    return Err("no state".into());
                }
                parse(&out)
            }
        }
    }
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Run a read-only command without input, with a time limit; its output.
fn run(mut c: Command, what: &str) -> Result<Vec<u8>, String> {
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{what} is not installed"),
        _ => format!("{what}: {e}"),
    })?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reader = std::thread::spawn(move || {
        let mut b = vec![];
        out.read_to_end(&mut b).map(|_| b)
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("{what}: no answer in time"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = reader.join().map_err(|_| format!("{what}: output lost"))?.map_err(|e| e.to_string())?;
    if !status.success() {
        let msg = err_reader.join().unwrap_or_default();
        let msg = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").chars().take(200).collect::<String>();
        return Err(format!("{what}: {msg}"));
    }
    Ok(out)
}

/// Token of `terraform login` / `TF_TOKEN_<host>` for an HCP Terraform or
/// Terraform Enterprise host.
fn tfc_token(host: &str) -> Option<String> {
    let var = format!("TF_TOKEN_{}", host.replace('-', "__").replace('.', "_"));
    if let Some(t) = env(&var).or_else(|| (host == "app.terraform.io").then(|| env("TFE_TOKEN")).flatten()) {
        return Some(t);
    }
    let file = std::env::var_os("TF_CLI_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".terraform.d")))?
        .join("credentials.tfrc.json");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    j["credentials"][host]["token"].as_str().map(str::to_string)
}

fn fetch_tfc(host: &str, organization: &str, workspace: &str) -> Result<serde_json::Value, String> {
    let token = tfc_token(host).ok_or_else(|| format!("no token for {host} (`terraform login` or TF_TOKEN_*)"))?;
    let client = client()?;
    let api = |url: &str| -> Result<serde_json::Value, String> {
        let r = client
            .get(url)
            .bearer_auth(&token)
            .header("Content-Type", "application/vnd.api+json")
            .send()
            .map_err(|e| e.without_url().to_string())?;
        match r.status().as_u16() {
            200 => r.json().map_err(|e| e.to_string()),
            401 | 403 => Err("permission denied".into()),
            404 => Err("not found (or no access)".into()),
            s => Err(format!("HTTP {s}")),
        }
    };
    let ws = api(&format!("https://{host}/api/v2/organizations/{organization}/workspaces/{workspace}"))?;
    let id = ws["data"]["id"].as_str().ok_or("workspace without id")?;
    let sv = api(&format!("https://{host}/api/v2/workspaces/{id}/current-state-version"))?;
    let url = sv["data"]["attributes"]["hosted-state-download-url"].as_str().ok_or("no state version")?;
    api(url)
}

/// The backend Terraform was initialized with in `dir`
/// (`.terraform/terraform.tfstate`): its type and full configuration, and the
/// workspace selected (`.terraform/environment`).
pub fn initialized(dir: &Path) -> Option<(String, Map, Option<String>)> {
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(".terraform/terraform.tfstate")).ok()?).ok()?;
    let kind = j["backend"]["type"].as_str()?.to_string();
    let config = Value::from_json(&j["backend"]["config"]).as_map().cloned().unwrap_or_default();
    let ws = std::fs::read_to_string(dir.join(".terraform/environment")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Some((kind, config, ws))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Map {
        pairs.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect()
    }

    #[test]
    fn state_location_per_backend() {
        let root = Path::new("/r");
        let src = |kind: &str, c: &[(&str, &str)], ws: &str| Source::from_backend(kind, &cfg(c), ws, root).unwrap().unwrap().describe();
        assert_eq!(src("gcs", &[("bucket", "b"), ("prefix", "p/")], "default"), "gs://b/p/default.tfstate");
        assert_eq!(src("gcs", &[("bucket", "b")], "dev"), "gs://b/dev.tfstate");
        assert_eq!(src("local", &[], "default"), "/r/terraform.tfstate");
        assert_eq!(src("local", &[("path", "state/x.tfstate")], "default"), "/r/state/x.tfstate");
        assert_eq!(src("local", &[], "dev"), "/r/terraform.tfstate.d/dev/terraform.tfstate");
        assert_eq!(src("s3", &[("bucket", "b"), ("key", "app/tf.tfstate")], "default"), "s3://b/app/tf.tfstate");
        assert_eq!(src("s3", &[("bucket", "b"), ("key", "tf.tfstate")], "dev"), "s3://b/env:/dev/tf.tfstate");
        assert_eq!(src("s3", &[("bucket", "b"), ("key", "tf.tfstate"), ("workspace_key_prefix", "ws")], "dev"), "s3://b/ws/dev/tf.tfstate");
        let az = [("storage_account_name", "acct"), ("container_name", "tf"), ("key", "app.tfstate")];
        assert_eq!(src("azurerm", &az, "default"), "azurerm://acct/tf/app.tfstate");
        assert_eq!(src("azurerm", &az, "dev"), "azurerm://acct/tf/app.tfstateenv:dev");
        assert_eq!(src("http", &[("address", "https://u:p@state.example.com/tf?token=x")], "default"), "https://state.example.com/tf");
        let mut remote = cfg(&[("organization", "acme")]);
        remote.insert("workspaces".into(), Value::List(vec![Value::Map(cfg(&[("prefix", "app-")]))]));
        assert_eq!(Source::from_backend("remote", &remote, "prod", root).unwrap().unwrap().describe(), "app.terraform.io/acme/app-prod");
        // Not known statically, or not readable directly.
        assert_eq!(Source::from_backend("gcs", &Map::new(), "default", root), Ok(None));
        assert!(Source::from_backend("pg", &Map::new(), "default", root).is_err());
    }

    #[test]
    fn local_and_initialized_states() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("terraform.tfstate"), r#"{"version": 4, "resources": []}"#).unwrap();
        let s = Source::from_backend("local", &Map::new(), "default", d).unwrap().unwrap();
        assert_eq!(s.fetch().unwrap()["version"], 4);
        assert_eq!(Source::Local(d.join("missing")).fetch().unwrap_err(), "not found");
        std::fs::create_dir_all(d.join(".terraform")).unwrap();
        std::fs::write(
            d.join(".terraform/terraform.tfstate"),
            r#"{"backend": {"type": "s3", "config": {"bucket": "b", "key": "k", "region": "eu-west-3"}}}"#,
        )
        .unwrap();
        std::fs::write(d.join(".terraform/environment"), "staging\n").unwrap();
        let (kind, config, ws) = initialized(d).unwrap();
        assert_eq!((kind.as_str(), get(&config, "region").as_deref(), ws.as_deref()), ("s3", Some("eu-west-3"), Some("staging")));
    }

    #[test]
    fn commands_are_run_without_input_and_report_their_error() {
        let mut c = Command::new("sh");
        c.args(["-c", "cat; echo '{\"version\": 4}'"]);
        assert_eq!(run(c, "sh").unwrap(), b"{\"version\": 4}\n");
        let mut c = Command::new("sh");
        c.args(["-c", "echo 'An error occurred (AccessDenied)' >&2; exit 1"]);
        assert_eq!(run(c, "aws (AWS CLI)").unwrap_err(), "aws (AWS CLI): An error occurred (AccessDenied)");
        assert_eq!(run(Command::new("costerra-no-such-cli"), "x").unwrap_err(), "x is not installed");
    }
}
