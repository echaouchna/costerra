//! Module source resolution: local paths, `.terraform/modules` manifests,
//! git repositories, Terraform registries and HTTP archives.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

type Slot = Arc<OnceLock<Result<PathBuf, String>>>;

pub struct ModuleResolver {
    pub cache_dir: PathBuf,
    pub offline: bool,
    inflight: Mutex<HashMap<String, Slot>>,
    manifests: Mutex<HashMap<PathBuf, Arc<HashMap<String, PathBuf>>>>,
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn fnv_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Split `url//subdir?query` into (url?query, subdir).
fn split_subdir(src: &str) -> (String, Option<String>) {
    let (body, query) = match src.find('?') {
        Some(i) => (&src[..i], Some(&src[i..])),
        None => (src, None),
    };
    let scheme_end = body.find("://").map(|i| i + 3).unwrap_or(0);
    let (base, sub) = match body[scheme_end..].find("//") {
        Some(i) => (&body[..scheme_end + i], Some(body[scheme_end + i + 2..].to_string())),
        None => (body, None),
    };
    (format!("{base}{}", query.unwrap_or("")), sub.filter(|s| !s.is_empty()))
}

fn take_query(url: &str, key: &str) -> (String, Option<String>) {
    let Some(i) = url.find('?') else { return (url.to_string(), None) };
    let (base, q) = (&url[..i], &url[i + 1..]);
    let mut val = None;
    let rest: Vec<&str> = q
        .split('&')
        .filter(|kv| {
            if let Some(v) = kv.strip_prefix(&format!("{key}=")) {
                val = Some(v.to_string());
                false
            } else {
                !kv.is_empty()
            }
        })
        .collect();
    let url = if rest.is_empty() { base.to_string() } else { format!("{base}?{}", rest.join("&")) };
    (url, val)
}

impl ModuleResolver {
    pub fn new(cache_dir: PathBuf, offline: bool) -> Self {
        ModuleResolver { cache_dir, offline, inflight: Mutex::new(HashMap::new()), manifests: Mutex::new(HashMap::new()) }
    }

    fn manifest(&self, root: &Path) -> Arc<HashMap<String, PathBuf>> {
        let mut guard = self.manifests.lock();
        if let Some(m) = guard.get(root) {
            return m.clone();
        }
        let mut map = HashMap::new();
        let path = root.join(".terraform/modules/modules.json");
        if let Ok(src) = std::fs::read_to_string(&path)
            && let Ok(j) = serde_json::from_str::<serde_json::Value>(&src)
        {
            for m in j["Modules"].as_array().into_iter().flatten() {
                if let (Some(k), Some(d)) = (m["Key"].as_str(), m["Dir"].as_str()) {
                    map.insert(k.to_string(), root.join(d));
                }
            }
        }
        let arc = Arc::new(map);
        guard.insert(root.to_path_buf(), arc.clone());
        arc
    }

    pub fn resolve(
        &self,
        caller: &Path,
        source: &str,
        version: Option<&str>,
        manifest_root: Option<&Path>,
        manifest_key: &str,
    ) -> Result<PathBuf, String> {
        if source.starts_with("./") || source.starts_with("../") || source == "." || source == ".." {
            let p = normalize(&caller.join(source));
            return if p.is_dir() { Ok(p) } else { Err(format!("directory {} not found", p.display())) };
        }
        if source.starts_with('/') {
            return Ok(PathBuf::from(source));
        }
        if let Some(root) = manifest_root
            && let Some(d) = self.manifest(root).get(manifest_key)
            && d.is_dir()
        {
            return Ok(d.clone());
        }
        self.fetch_remote(source, version)
    }

    /// Fetch a remote source (git/registry/http). Results are cached on disk and
    /// concurrent requests for the same source are coalesced.
    pub fn fetch_remote(&self, source: &str, version: Option<&str>) -> Result<PathBuf, String> {
        let key = format!("{source}@{}", version.unwrap_or(""));
        let slot = self.inflight.lock().entry(key).or_default().clone();
        slot.get_or_init(|| self.fetch_uncached(source, version)).clone()
    }

    fn fetch_uncached(&self, source: &str, version: Option<&str>) -> Result<PathBuf, String> {
        let (src, sub) = split_subdir(source);
        let base = self.fetch_base(&src, version)?;
        Ok(match sub {
            Some(s) => normalize(&base.join(s)),
            None => base,
        })
    }

    fn fetch_base(&self, src: &str, version: Option<&str>) -> Result<PathBuf, String> {
        let s = src.strip_prefix("git::").unwrap_or(src);
        if src.starts_with("git::") || s.starts_with("git@") || s.ends_with(".git") || s.contains(".git?") {
            return self.git(s);
        }
        if let Some(rest) = s.strip_prefix("github.com/") {
            return self.git(&format!("https://github.com/{rest}"));
        }
        if let Some(rest) = s.strip_prefix("bitbucket.org/") {
            return self.git(&format!("https://bitbucket.org/{rest}"));
        }
        if let Some(rest) = s.strip_prefix("tfr://") {
            let (path, ver) = take_query(rest, "version");
            let path = path.trim_start_matches('/');
            let path = if path.starts_with("registry.terraform.io/") || path.split('/').count() == 4 {
                path.to_string()
            } else {
                format!("registry.terraform.io/{path}")
            };
            return self.registry(&path, ver.as_deref().or(version));
        }
        if s.starts_with("http://") || s.starts_with("https://") {
            if (s.contains("github.com/") || s.contains("gitlab.com/") || s.contains("bitbucket.org/"))
                && !(s.ends_with(".tar.gz") || s.ends_with(".tgz") || s.ends_with(".zip"))
            {
                return self.git(s);
            }
            return self.http_archive(s);
        }
        if s.starts_with("s3::") || s.starts_with("gcs::") || s.starts_with("hg::") {
            return Err("unsupported module source type".into());
        }
        static REGISTRY_SOURCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^([a-zA-Z0-9.\-]+\.[a-zA-Z]+/)?[\w\-]+/[\w\-]+/[\w\-]+$").expect("valid regex")
        });
        if REGISTRY_SOURCE.is_match(s) {
            return self.registry(s, version);
        }
        Err("unrecognized module source".into())
    }

    fn git(&self, url: &str) -> Result<PathBuf, String> {
        let (url, git_ref) = take_query(url, "ref");
        let (url, _depth) = take_query(&url, "depth");
        let dest = self.cache_dir.join("git").join(fnv_hash(&format!("{url}#{}", git_ref.clone().unwrap_or_default())));
        if dest.join(".costerra-ok").exists() {
            return Ok(dest);
        }
        if self.offline {
            return Err("offline mode: module not in cache".into());
        }
        let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.parent().unwrap()).map_err(|e| e.to_string())?;
        tracing::info!("cloning {url} {}", git_ref.clone().unwrap_or_default());
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--quiet", "--depth", "1"]);
        if let Some(r) = &git_ref {
            cmd.args(["--branch", r]);
        }
        cmd.arg(&url).arg(&tmp).env("GIT_TERMINAL_PROMPT", "0");
        let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
        if !ok {
            // Ref may be a commit SHA: full clone + checkout.
            let _ = std::fs::remove_dir_all(&tmp);
            let o = Command::new("git")
                .args(["clone", "--quiet"])
                .arg(&url)
                .arg(&tmp)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .map_err(|e| e.to_string())?;
            if !o.status.success() {
                return Err(format!("git clone failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
            }
            if let Some(r) = &git_ref {
                let o = Command::new("git").arg("-C").arg(&tmp).args(["checkout", "--quiet", r]).output().map_err(|e| e.to_string())?;
                if !o.status.success() {
                    return Err(format!("git checkout {r} failed"));
                }
            }
        }
        let _ = std::fs::remove_dir_all(tmp.join(".git"));
        let _ = std::fs::write(tmp.join(".costerra-ok"), url.as_bytes());
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        Ok(dest)
    }

    fn http_client() -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())
    }

    fn registry(&self, path: &str, version: Option<&str>) -> Result<PathBuf, String> {
        let parts: Vec<&str> = path.split('/').collect();
        let (host, ns, name, provider) = match parts.as_slice() {
            [ns, n, p] => ("registry.terraform.io", *ns, *n, *p),
            [h, ns, n, p] => (*h, *ns, *n, *p),
            _ => return Err("bad registry path".into()),
        };
        let cache_key = fnv_hash(&format!("{host}/{ns}/{name}/{provider}@{}", version.unwrap_or("latest")));
        let marker = self.cache_dir.join("registry").join(format!("{cache_key}.src"));
        if let Ok(src) = std::fs::read_to_string(&marker) {
            return self.fetch_uncached(src.trim(), None);
        }
        if self.offline {
            return Err("offline mode: module not in cache".into());
        }
        let client = Self::http_client()?;
        let token = std::env::var(format!("TF_TOKEN_{}", host.replace(['.', '-'], "_"))).ok();
        let get = |url: &str| {
            let mut rq = client.get(url);
            if let Some(t) = &token {
                rq = rq.bearer_auth(t);
            }
            rq.send().map_err(|e| e.to_string())
        };
        let disco: serde_json::Value = get(&format!("https://{host}/.well-known/terraform.json"))?.json().map_err(|e| e.to_string())?;
        let base = disco["modules.v1"].as_str().unwrap_or("/v1/modules/");
        let base = if base.starts_with("http") { base.to_string() } else { format!("https://{host}{base}") };
        let versions: serde_json::Value = get(&format!("{base}{ns}/{name}/{provider}/versions"))?.json().map_err(|e| e.to_string())?;
        let mut avail: Vec<String> = versions["modules"][0]["versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["version"].as_str().map(str::to_string))
            .collect();
        avail.sort_by(|a, b| semver_cmp(b, a));
        let chosen = avail
            .iter()
            .find(|v| version.is_none_or(|c| constraint_matches(c, v)) && !v.contains('-'))
            .ok_or_else(|| format!("no version matching '{}'", version.unwrap_or("*")))?
            .clone();
        let resp = get(&format!("{base}{ns}/{name}/{provider}/{chosen}/download"))?;
        let loc = resp
            .headers()
            .get("x-terraform-get")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .ok_or("registry did not return a download location")?;
        let loc = if loc.starts_with('/') { format!("https://{host}{loc}") } else { loc };
        std::fs::create_dir_all(marker.parent().unwrap()).map_err(|e| e.to_string())?;
        let _ = std::fs::write(&marker, &loc);
        self.fetch_uncached(&loc, None)
    }

    fn http_archive(&self, url: &str) -> Result<PathBuf, String> {
        let (url, archive) = take_query(url, "archive");
        let dest = self.cache_dir.join("http").join(fnv_hash(&url));
        if dest.join(".costerra-ok").exists() {
            return Ok(dest);
        }
        if self.offline {
            return Err("offline mode: module not in cache".into());
        }
        let bytes =
            Self::http_client()?.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes()).map_err(|e| e.to_string())?;
        let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let is_zip = archive.as_deref() == Some("zip") || url.ends_with(".zip") || bytes.starts_with(b"PK");
        if is_zip {
            let zpath = tmp.join("archive.zip");
            std::fs::write(&zpath, &bytes).map_err(|e| e.to_string())?;
            let o = Command::new("unzip").arg("-q").arg(&zpath).arg("-d").arg(&tmp).output().map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&zpath);
            if !o.status.success() {
                return Err("unzip failed".into());
            }
        } else {
            let gz = flate2::read::GzDecoder::new(&bytes[..]);
            tar::Archive::new(gz).unpack(&tmp).map_err(|e| e.to_string())?;
        }
        // Archives often contain a single top-level directory.
        let entries: Vec<_> = std::fs::read_dir(&tmp).map_err(|e| e.to_string())?.flatten().collect();
        let root = if entries.len() == 1 && entries[0].path().is_dir() { entries[0].path() } else { tmp.clone() };
        let _ = std::fs::write(root.join(".costerra-ok"), url.as_bytes());
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(&root, &dest).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(dest)
    }
}

fn parse_ver(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn semver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut x, mut y) = (parse_ver(a), parse_ver(b));
    let n = x.len().max(y.len()).max(3);
    x.resize(n, 0);
    y.resize(n, 0);
    x.cmp(&y)
}

/// Terraform version constraints: `=`, `!=`, `>`, `>=`, `<`, `<=`, `~>`, comma-separated.
pub fn constraint_matches(constraint: &str, version: &str) -> bool {
    use std::cmp::Ordering::*;
    constraint.split(',').map(str::trim).filter(|c| !c.is_empty()).all(|c| {
        let (op, v) = ["~>", ">=", "<=", "!=", ">", "<", "="]
            .iter()
            .find_map(|op| c.strip_prefix(op).map(|rest| (*op, rest.trim())))
            .unwrap_or(("=", c));
        let ord = semver_cmp(version, v);
        match op {
            "=" => ord == Equal,
            "!=" => ord != Equal,
            ">" => ord == Greater,
            ">=" => ord != Less,
            "<" => ord == Less,
            "<=" => ord != Greater,
            "~>" => {
                let parts = parse_ver(v);
                if ord == Less {
                    return false;
                }
                let n = v.trim_start_matches('v').split('.').count();
                let mut upper = parts.clone();
                upper.resize(3, 0);
                if n <= 1 {
                    return true;
                }
                upper.truncate(n - 1);
                *upper.last_mut().unwrap() += 1;
                let upper_s = upper.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".");
                semver_cmp(version, &upper_s) == Less
            }
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdir_split() {
        assert_eq!(
            split_subdir("git::https://gitlab.com/org/repo.git//modules/vm?ref=v1.2.0"),
            ("git::https://gitlab.com/org/repo.git?ref=v1.2.0".into(), Some("modules/vm".into()))
        );
        assert_eq!(split_subdir("terraform-google-modules/network/google//modules/vpc").1, Some("modules/vpc".into()));
    }

    #[test]
    fn constraints() {
        assert!(constraint_matches("~> 1.2", "1.9.0"));
        assert!(!constraint_matches("~> 1.2", "2.0.0"));
        assert!(constraint_matches("~> 1.2.3", "1.2.9"));
        assert!(!constraint_matches("~> 1.2.3", "1.3.0"));
        assert!(constraint_matches(">= 1.0, < 2.0", "1.5.0"));
        assert!(constraint_matches("1.2.3", "1.2.3"));
    }
}
