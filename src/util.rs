//! Small shared helpers: atomic, locked cache files and URL sanitizing.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write a file atomically: a temporary file in the same directory is
/// renamed over the destination, so readers never see a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new().prefix(".costerra-").suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_data()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Run `f` while holding an exclusive advisory lock on `<path>.lock`.
/// Several costerra processes (parallel CI jobs, the background price
/// refresh) share the cache directory. Locking is best effort: when the lock
/// file cannot be created, `f` still runs.
pub fn with_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lp = lock_path(path);
    if let Some(d) = lp.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).ok();
    let locked = file.as_ref().is_some_and(|f| f.lock().is_ok());
    let out = f();
    if locked && let Some(f) = &file {
        let _ = f.unlock();
    }
    out
}

/// Try to take an exclusive lock on `<path>.lock` without waiting. The lock is
/// held until the returned file is dropped. `None` when another process holds it.
pub fn try_lock(path: &Path) -> Option<std::fs::File> {
    let lp = lock_path(path);
    if let Some(d) = lp.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).ok()?;
    f.try_lock().ok()?;
    Some(f)
}

/// Read a JSON file, or the default value when missing or invalid.
pub fn read_json<T: Default + DeserializeOwned>(path: &Path) -> T {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Read-modify-write a JSON file under an exclusive lock, written atomically,
/// so concurrent writers merge their changes instead of overwriting each other.
pub fn update_json<T: Default + Serialize + DeserializeOwned, R>(path: &Path, f: impl FnOnce(&mut T) -> R) -> R {
    with_lock(path, || {
        let mut v: T = read_json(path);
        let r = f(&mut v);
        match serde_json::to_vec(&v) {
            Ok(b) => {
                if let Err(e) = write_atomic(path, &b) {
                    tracing::debug!("writing {}: {e}", path.display());
                }
            }
            Err(e) => tracing::debug!("serializing {}: {e}", path.display()),
        }
        r
    })
}

/// Deserialize YAML with YAML 1.2 semantics (like Terraform's `yamldecode`):
/// only `true` / `false` are booleans, `yes`, `no`, `on`, `y`… stay strings.
pub fn from_yaml<T: DeserializeOwned>(src: &str) -> Result<T, serde_saphyr::Error> {
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    serde_saphyr::from_str_with_options(src, options)
}

/// Parse a duration such as `0`, `90s`, `15m`, `1h` or `2d` into seconds.
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("invalid duration '{s}' (examples: 0, 90s, 15m, 1h)"))
}

/// Remove credentials (`user:token@`) from a URL, e.g. the
/// `https://gitlab-ci-token:<token>@gitlab.com/…` remotes of GitLab CI.
pub fn strip_url_credentials(url: &str) -> String {
    let url = url.trim();
    let Some(scheme_end) = url.find("://") else { return url.to_string() };
    let (scheme, rest) = (&url[..scheme_end + 3], &url[scheme_end + 3..]);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    match authority.rfind('@') {
        // `ssh://git@host` keeps the conventional `git` user (no secret).
        Some(at) if &authority[..at] != "git" => format!("{scheme}{}{path}", &authority[at + 1..]),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_stripped() {
        assert_eq!(
            strip_url_credentials("https://gitlab-ci-token:glcbt-abc@gitlab.com/example/infra.git"),
            "https://gitlab.com/example/infra.git"
        );
        assert_eq!(strip_url_credentials("https://oauth2:x@y@github.com/a/b"), "https://github.com/a/b");
        assert_eq!(strip_url_credentials("https://github.com/a/b"), "https://github.com/a/b");
        assert_eq!(strip_url_credentials("ssh://git@gitlab.com:2222/a/b.git"), "ssh://git@gitlab.com:2222/a/b.git");
        assert_eq!(strip_url_credentials("git@gitlab.com:a/b.git"), "git@gitlab.com:a/b.git");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("0"), Ok(0));
        assert_eq!(parse_duration_secs("90s"), Ok(90));
        assert_eq!(parse_duration_secs("15m"), Ok(900));
        assert_eq!(parse_duration_secs("1h"), Ok(3600));
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("99999999999999999999d").is_err());
    }

    #[test]
    fn locked_updates_merge() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.json");
        std::thread::scope(|s| {
            for i in 0..16 {
                let p = &p;
                s.spawn(move || {
                    update_json(p, |m: &mut std::collections::BTreeMap<String, u32>| {
                        m.insert(format!("k{i}"), i);
                    })
                });
            }
        });
        let m: std::collections::BTreeMap<String, u32> = read_json(&p);
        assert_eq!(m.len(), 16);
    }
}

/// A local HTTP server answering canned responses, for tests of the Google
/// API clients (no network nor credentials).
#[cfg(test)]
pub(crate) mod fake_http {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A received request: path and decoded query parameters.
    #[derive(Debug, Clone)]
    pub struct Request {
        pub path: String,
        pub query: Vec<(String, String)>,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Request {
        pub fn param(&self, k: &str) -> Option<&str> {
            self.query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
        }
        pub fn header(&self, k: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
        }
    }

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Server {
        pub fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < b.len() => {
                    out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?'));
                    i += 2;
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Serve `handler(request) -> (status, JSON body)` until the test ends.
    pub fn serve(handler: impl Fn(&Request) -> (u16, String) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut headers = vec![];
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.trim().split_once(':') {
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
                let query = qs
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let (k, v) = p.split_once('=').unwrap_or((p, ""));
                        (decode(k), decode(v))
                    })
                    .collect();
                let len =
                    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                let mut body = vec![0u8; len];
                if std::io::Read::read_exact(&mut reader, &mut body).is_err() {
                    continue;
                }
                let req = Request { path: path.to_string(), query, headers, body: String::from_utf8_lossy(&body).into_owned() };
                let (status, body) = handler(&req);
                log.lock().unwrap().push(req);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Server { url, requests }
    }
}
