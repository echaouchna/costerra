//! Terraform built-in functions.

use super::eval::{EvalError, EvalResult, Evaluator, err, values_equal};
use super::value::{Map, Value, fmt_number};
use std::path::{Path, PathBuf};

macro_rules! unknown_if_any {
    ($args:expr) => {
        if $args.iter().any(|a| a.is_unknown()) {
            return Ok(Value::Unknown);
        }
    };
}

fn arg<'v>(args: &'v [Value], i: usize, f: &str) -> Result<&'v Value, EvalError> {
    args.get(i).ok_or_else(|| EvalError(format!("{f}: missing argument {}", i + 1)))
}

fn s(args: &[Value], i: usize, f: &str) -> Result<String, EvalError> {
    let v = arg(args, i, f)?;
    v.to_interp_string().ok_or_else(|| EvalError(format!("{f}: argument {} must be a string", i + 1)))
}

fn n(args: &[Value], i: usize, f: &str) -> Result<f64, EvalError> {
    arg(args, i, f)?.as_f64().ok_or_else(|| EvalError(format!("{f}: argument {} must be a number", i + 1)))
}

fn list<'v>(args: &'v [Value], i: usize, f: &str) -> Result<&'v Vec<Value>, EvalError> {
    arg(args, i, f)?.as_list().ok_or_else(|| EvalError(format!("{f}: argument {} must be a list", i + 1)))
}

fn resolve_path(ev: &Evaluator, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { ev.base_dir.join(path) }
}

pub fn call(name: &str, args: &[Value], ev: &Evaluator) -> EvalResult {
    match name {
        // ---- type conversion -------------------------------------------------
        "tostring" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::Null => Ok(Value::Null),
                v => v.to_interp_string().map(Value::String).ok_or_else(|| EvalError("tostring: invalid".into())),
            }
        }
        "tonumber" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::Null => Ok(Value::Null),
                v => v.as_f64().map(Value::Number).ok_or_else(|| EvalError("tonumber: invalid".into())),
            }
        }
        "tobool" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::Null => Ok(Value::Null),
                v => v.as_bool().map(Value::Bool).ok_or_else(|| EvalError("tobool: invalid".into())),
            }
        }
        "tolist" | "toset" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::List(l) => {
                    if name == "toset" {
                        Ok(Value::List(dedup(l.clone())))
                    } else {
                        Ok(Value::List(l.clone()))
                    }
                }
                Value::Map(m) => Ok(Value::List(m.values().cloned().collect())),
                Value::Null => Ok(Value::Null),
                _ => err(format!("{name}: invalid argument")),
            }
        }
        "tomap" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                v @ (Value::Map(_) | Value::Null) => Ok(v.clone()),
                _ => err("tomap: invalid argument"),
            }
        }
        "sensitive" | "nonsensitive" | "type" => Ok(arg(args, 0, name)?.clone()),
        "ephemeralasnull" => Ok(Value::Null),

        // ---- numeric ---------------------------------------------------------
        "abs" => num1(args, name, f64::abs),
        "ceil" => num1(args, name, f64::ceil),
        "floor" => num1(args, name, f64::floor),
        "signum" => num1(args, name, f64::signum),
        "log" => {
            unknown_if_any!(args);
            Ok(Value::Number(n(args, 0, name)?.log(n(args, 1, name)?)))
        }
        "pow" => {
            unknown_if_any!(args);
            Ok(Value::Number(n(args, 0, name)?.powf(n(args, 1, name)?)))
        }
        "max" | "min" => {
            let vals = flatten_args(args);
            unknown_if_any!(vals);
            let nums: Result<Vec<f64>, _> = vals.iter().map(|v| v.as_f64().ok_or(())).collect();
            let nums = nums.map_err(|_| EvalError(format!("{name}: non-number")))?;
            if nums.is_empty() {
                return err(format!("{name}: no arguments"));
            }
            let r =
                if name == "max" { nums.iter().cloned().fold(f64::MIN, f64::max) } else { nums.iter().cloned().fold(f64::MAX, f64::min) };
            Ok(Value::Number(r))
        }
        "parseint" => {
            unknown_if_any!(args);
            let st = s(args, 0, name)?;
            let base = n(args, 1, name)? as u32;
            i64::from_str_radix(&st, base).map(|v| Value::Number(v as f64)).map_err(|e| EvalError(format!("parseint: {e}")))
        }
        "sum" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            if l.iter().any(Value::is_unknown) {
                return Ok(Value::Unknown);
            }
            Ok(Value::Number(l.iter().filter_map(Value::as_f64).sum()))
        }

        // ---- string ----------------------------------------------------------
        "lower" => str1(args, name, |s| s.to_lowercase()),
        "upper" => str1(args, name, |s| s.to_uppercase()),
        "title" => str1(args, name, |s| {
            s.split(' ')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }),
        "trimspace" => str1(args, name, |s| s.trim().to_string()),
        "chomp" => str1(args, name, |s| s.trim_end_matches(['\n', '\r']).to_string()),
        "strrev" => str1(args, name, |s| s.chars().rev().collect()),
        "trim" => {
            unknown_if_any!(args);
            let (a, cut) = (s(args, 0, name)?, s(args, 1, name)?);
            Ok(Value::String(a.trim_matches(|c| cut.contains(c)).to_string()))
        }
        "trimprefix" => {
            unknown_if_any!(args);
            let (a, p) = (s(args, 0, name)?, s(args, 1, name)?);
            Ok(Value::String(a.strip_prefix(p.as_str()).unwrap_or(&a).to_string()))
        }
        "trimsuffix" => {
            unknown_if_any!(args);
            let (a, p) = (s(args, 0, name)?, s(args, 1, name)?);
            Ok(Value::String(a.strip_suffix(p.as_str()).unwrap_or(&a).to_string()))
        }
        "startswith" => {
            unknown_if_any!(args);
            Ok(Value::Bool(s(args, 0, name)?.starts_with(&s(args, 1, name)?)))
        }
        "endswith" => {
            unknown_if_any!(args);
            Ok(Value::Bool(s(args, 0, name)?.ends_with(&s(args, 1, name)?)))
        }
        "strcontains" => {
            unknown_if_any!(args);
            Ok(Value::Bool(s(args, 0, name)?.contains(&s(args, 1, name)?)))
        }
        "substr" => {
            unknown_if_any!(args);
            let st: Vec<char> = s(args, 0, name)?.chars().collect();
            let mut off = n(args, 1, name)? as i64;
            let len = n(args, 2, name)? as i64;
            if off < 0 {
                off += st.len() as i64;
            }
            let off = off.clamp(0, st.len() as i64) as usize;
            let end = if len < 0 { st.len() } else { (off + len as usize).min(st.len()) };
            Ok(Value::String(st[off..end].iter().collect()))
        }
        "indent" => {
            unknown_if_any!(args);
            let pad = " ".repeat(n(args, 0, name)? as usize);
            let st = s(args, 1, name)?;
            Ok(Value::String(st.replace('\n', &format!("\n{pad}"))))
        }
        "join" => {
            unknown_if_any!(args);
            let sep = s(args, 0, name)?;
            let mut parts = vec![];
            for a in &args[1..] {
                for v in a.as_list().ok_or_else(|| EvalError("join: expected list".into()))? {
                    if v.is_unknown() {
                        return Ok(Value::Unknown);
                    }
                    parts.push(v.to_interp_string().unwrap_or_default());
                }
            }
            Ok(Value::String(parts.join(&sep)))
        }
        "split" => {
            unknown_if_any!(args);
            let (sep, st) = (s(args, 0, name)?, s(args, 1, name)?);
            if st.is_empty() {
                return Ok(Value::List(vec![Value::String(String::new())]));
            }
            Ok(Value::List(st.split(sep.as_str()).map(Value::from).collect()))
        }
        "replace" => {
            unknown_if_any!(args);
            let (st, sub, rep) = (s(args, 0, name)?, s(args, 1, name)?, s(args, 2, name)?);
            if sub.len() > 2 && sub.starts_with('/') && sub.ends_with('/') {
                let re = cached_regex(&sub[1..sub.len() - 1]).map_err(|e| EvalError(e.to_string()))?;
                Ok(Value::String(re.replace_all(&st, rep.as_str()).to_string()))
            } else {
                Ok(Value::String(st.replace(&sub, &rep)))
            }
        }
        "regex" | "regexall" => {
            unknown_if_any!(args);
            let (pat, st) = (s(args, 0, name)?, s(args, 1, name)?);
            let re = cached_regex(&pat).map_err(|e| EvalError(e.to_string()))?;
            let conv = |c: regex::Captures| -> Value {
                let names: Vec<_> = re.capture_names().flatten().collect();
                if !names.is_empty() {
                    let mut m = Map::new();
                    for nm in names {
                        m.insert(nm.to_string(), c.name(nm).map(|x| Value::from(x.as_str())).unwrap_or(Value::Null));
                    }
                    Value::Map(m)
                } else if c.len() > 1 {
                    Value::List((1..c.len()).map(|i| c.get(i).map(|x| Value::from(x.as_str())).unwrap_or(Value::Null)).collect())
                } else {
                    Value::from(c.get(0).map(|x| x.as_str()).unwrap_or(""))
                }
            };
            if name == "regex" {
                re.captures(&st).map(conv).ok_or_else(|| EvalError("regex: no match".into()))
            } else {
                Ok(Value::List(re.captures_iter(&st).map(conv).collect()))
            }
        }
        "format" => {
            unknown_if_any!(args);
            let f = s(args, 0, name)?;
            format(&f, &args[1..]).map(Value::String)
        }
        "formatlist" => {
            unknown_if_any!(args);
            let f = s(args, 0, name)?;
            let rest = &args[1..];
            let len = rest.iter().filter_map(|a| a.as_list().map(|l| l.len())).max().unwrap_or(1);
            let mut out = vec![];
            for i in 0..len {
                let row: Vec<Value> = rest
                    .iter()
                    .map(|a| match a {
                        Value::List(l) => l.get(i).cloned().unwrap_or(Value::Null),
                        o => o.clone(),
                    })
                    .collect();
                out.push(Value::String(format(&f, &row)?));
            }
            Ok(Value::List(out))
        }

        // ---- collections -----------------------------------------------------
        "length" => {
            unknown_if_any!(args);
            let v = arg(args, 0, name)?;
            v.len().map(|l| Value::Number(l as f64)).ok_or_else(|| EvalError("length: invalid".into()))
        }
        "concat" => {
            unknown_if_any!(args);
            let mut out = vec![];
            for a in args {
                match a {
                    Value::List(l) => out.extend(l.iter().cloned()),
                    Value::Null => {}
                    _ => return err("concat: expected lists"),
                }
            }
            Ok(Value::List(out))
        }
        "merge" => {
            let mut out = Map::new();
            for a in args {
                match a {
                    Value::Map(m) => {
                        for (k, v) in m {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    Value::Null => {}
                    Value::Unknown => return Ok(Value::Unknown),
                    _ => return err("merge: expected maps"),
                }
            }
            Ok(Value::Map(out))
        }
        "lookup" => {
            let m = arg(args, 0, name)?;
            let k = arg(args, 1, name)?;
            if m.is_unknown() || k.is_unknown() {
                return Ok(Value::Unknown);
            }
            let key = k.to_interp_string().unwrap_or_default();
            match m.get(&key) {
                Some(v) => Ok(v.clone()),
                None => match args.get(2) {
                    Some(d) => Ok(d.clone()),
                    None => err(format!("lookup: key {key} not found")),
                },
            }
        }
        "keys" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::Map(m) => {
                    let mut k: Vec<_> = m.keys().cloned().collect();
                    k.sort();
                    Ok(Value::List(k.into_iter().map(Value::String).collect()))
                }
                _ => err("keys: expected map"),
            }
        }
        "values" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::Map(m) => {
                    let mut kv: Vec<_> = m.iter().collect();
                    kv.sort_by(|a, b| a.0.cmp(b.0));
                    Ok(Value::List(kv.into_iter().map(|(_, v)| v.clone()).collect()))
                }
                _ => err("values: expected map"),
            }
        }
        "contains" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            let v = arg(args, 1, name)?;
            Ok(Value::Bool(l.iter().any(|x| values_equal(x, v))))
        }
        "element" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            if l.is_empty() {
                return err("element: empty list");
            }
            let i = n(args, 1, name)? as usize;
            Ok(l[i % l.len()].clone())
        }
        "index" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            let v = arg(args, 1, name)?;
            l.iter().position(|x| values_equal(x, v)).map(|i| Value::Number(i as f64)).ok_or_else(|| EvalError("index: not found".into()))
        }
        "flatten" => {
            unknown_if_any!(args);
            fn fl(v: &Value, out: &mut Vec<Value>) {
                match v {
                    Value::List(l) => l.iter().for_each(|x| fl(x, out)),
                    o => out.push(o.clone()),
                }
            }
            let mut out = vec![];
            fl(arg(args, 0, name)?, &mut out);
            Ok(Value::List(out))
        }
        "distinct" => {
            unknown_if_any!(args);
            Ok(Value::List(dedup(list(args, 0, name)?.clone())))
        }
        "compact" => {
            unknown_if_any!(args);
            Ok(Value::List(list(args, 0, name)?.iter().filter(|v| !matches!(v, Value::Null) && v.as_str() != Some("")).cloned().collect()))
        }
        "coalesce" => {
            for a in args {
                match a {
                    Value::Unknown => return Ok(Value::Unknown),
                    Value::Null => {}
                    Value::String(s) if s.is_empty() => {}
                    v => return Ok(v.clone()),
                }
            }
            err("coalesce: no non-null arguments")
        }
        "coalescelist" => {
            for a in args {
                match a {
                    Value::Unknown => return Ok(Value::Unknown),
                    Value::List(l) if !l.is_empty() => return Ok(a.clone()),
                    _ => {}
                }
            }
            Ok(Value::List(vec![]))
        }
        "one" => {
            unknown_if_any!(args);
            match arg(args, 0, name)? {
                Value::List(l) if l.is_empty() => Ok(Value::Null),
                Value::List(l) if l.len() == 1 => Ok(l[0].clone()),
                Value::Null => Ok(Value::Null),
                _ => err("one: more than one element"),
            }
        }
        "range" => {
            unknown_if_any!(args);
            let (start, end, step) = match args.len() {
                1 => (0.0, n(args, 0, name)?, 1.0),
                2 => (n(args, 0, name)?, n(args, 1, name)?, 1.0),
                _ => (n(args, 0, name)?, n(args, 1, name)?, n(args, 2, name)?),
            };
            let mut out = vec![];
            let mut i = start;
            while (step > 0.0 && i < end) || (step < 0.0 && i > end) {
                out.push(Value::Number(i));
                i += step;
                if out.len() > 100_000 {
                    break;
                }
            }
            Ok(Value::List(out))
        }
        "reverse" => {
            unknown_if_any!(args);
            let mut l = list(args, 0, name)?.clone();
            l.reverse();
            Ok(Value::List(l))
        }
        "sort" => {
            unknown_if_any!(args);
            let mut l: Vec<String> = list(args, 0, name)?.iter().filter_map(Value::to_interp_string).collect();
            l.sort();
            Ok(Value::List(l.into_iter().map(Value::String).collect()))
        }
        "slice" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            let a = (n(args, 1, name)? as usize).min(l.len());
            let b = (n(args, 2, name)? as usize).min(l.len());
            Ok(Value::List(l[a..b.max(a)].to_vec()))
        }
        "chunklist" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            let sz = (n(args, 1, name)? as usize).max(1);
            Ok(Value::List(l.chunks(sz).map(|c| Value::List(c.to_vec())).collect()))
        }
        "zipmap" => {
            unknown_if_any!(args);
            let k = list(args, 0, name)?;
            let v = list(args, 1, name)?;
            Ok(Value::Map(k.iter().zip(v.iter()).map(|(k, v)| (k.to_interp_string().unwrap_or_default(), v.clone())).collect()))
        }
        "alltrue" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            if l.iter().any(Value::is_unknown) {
                return Ok(Value::Unknown);
            }
            Ok(Value::Bool(l.iter().all(|v| v.as_bool() == Some(true))))
        }
        "anytrue" => {
            unknown_if_any!(args);
            let l = list(args, 0, name)?;
            Ok(Value::Bool(l.iter().any(|v| v.as_bool() == Some(true))))
        }
        "setunion" => {
            unknown_if_any!(args);
            let mut out = vec![];
            for a in args {
                out.extend(a.as_list().cloned().unwrap_or_default());
            }
            Ok(Value::List(dedup(out)))
        }
        "setintersection" => {
            unknown_if_any!(args);
            let mut cur = dedup(list(args, 0, name)?.clone());
            for a in &args[1..] {
                let other = a.as_list().cloned().unwrap_or_default();
                cur.retain(|x| other.iter().any(|y| values_equal(x, y)));
            }
            Ok(Value::List(cur))
        }
        "setsubtract" => {
            unknown_if_any!(args);
            let a = list(args, 0, name)?;
            let b = list(args, 1, name)?;
            Ok(Value::List(dedup(a.clone()).into_iter().filter(|x| !b.iter().any(|y| values_equal(x, y))).collect()))
        }
        "setproduct" => {
            unknown_if_any!(args);
            let mut acc: Vec<Vec<Value>> = vec![vec![]];
            for a in args {
                let l = a.as_list().cloned().unwrap_or_default();
                let mut next = vec![];
                for prefix in &acc {
                    for x in &l {
                        let mut p = prefix.clone();
                        p.push(x.clone());
                        next.push(p);
                    }
                }
                acc = next;
            }
            Ok(Value::List(acc.into_iter().map(Value::List).collect()))
        }
        "matchkeys" => {
            unknown_if_any!(args);
            let vals = list(args, 0, name)?;
            let keys = list(args, 1, name)?;
            let search = list(args, 2, name)?;
            Ok(Value::List(
                vals.iter().zip(keys.iter()).filter(|(_, k)| search.iter().any(|s| values_equal(s, k))).map(|(v, _)| v.clone()).collect(),
            ))
        }
        "transpose" => {
            unknown_if_any!(args);
            let mut out: indexmap::IndexMap<String, Vec<Value>> = indexmap::IndexMap::new();
            if let Value::Map(m) = arg(args, 0, name)? {
                for (k, v) in m {
                    for item in v.as_list().cloned().unwrap_or_default() {
                        out.entry(item.to_interp_string().unwrap_or_default()).or_default().push(Value::String(k.clone()));
                    }
                }
            }
            Ok(Value::Map(out.into_iter().map(|(k, v)| (k, Value::List(v))).collect()))
        }

        // ---- encoding --------------------------------------------------------
        "jsondecode" => {
            unknown_if_any!(args);
            let st = s(args, 0, name)?;
            serde_json::from_str::<serde_json::Value>(&st).map(|j| Value::from_json(&j)).map_err(|e| EvalError(format!("jsondecode: {e}")))
        }
        "jsonencode" => {
            unknown_if_any!(args);
            if !arg(args, 0, name)?.is_wholly_known() {
                return Ok(Value::Unknown);
            }
            Ok(Value::String(arg(args, 0, name)?.to_json().to_string()))
        }
        "yamldecode" => {
            unknown_if_any!(args);
            let st = s(args, 0, name)?;
            Value::parse_yaml(&st).map_err(|e| EvalError(format!("yamldecode: {e}")))
        }
        "yamlencode" => {
            unknown_if_any!(args);
            serde_saphyr::to_string(&arg(args, 0, name)?.to_json()).map(Value::String).map_err(|e| EvalError(e.to_string()))
        }
        "csvdecode" => {
            unknown_if_any!(args);
            let st = s(args, 0, name)?;
            let mut lines = st.lines();
            let header: Vec<&str> = lines.next().unwrap_or("").split(',').map(str::trim).collect();
            Ok(Value::List(
                lines
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        Value::Map(
                            header
                                .iter()
                                .zip(l.split(','))
                                .map(|(h, v)| (h.to_string(), Value::from(v.trim().trim_matches('"'))))
                                .collect(),
                        )
                    })
                    .collect(),
            ))
        }
        "urlencode" => str1(args, name, |s| {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect()
        }),
        "base64encode" | "base64decode" | "base64gzip" | "textencodebase64" | "textdecodebase64" | "filebase64" | "md5" | "sha1"
        | "sha256" | "sha512" | "base64sha256" | "base64sha512" | "filemd5" | "filesha1" | "filesha256" | "filesha512"
        | "filebase64sha256" | "filebase64sha512" | "uuid" | "uuidv5" | "bcrypt" | "timestamp" | "plantimestamp" | "rsadecrypt" => {
            Ok(Value::Unknown)
        }
        "formatdate" | "timeadd" | "timecmp" => Ok(Value::Unknown),
        "cidrhost" | "cidrnetmask" | "cidrsubnet" | "cidrsubnets" => Ok(Value::Unknown),

        // ---- filesystem ------------------------------------------------------
        "file" => {
            unknown_if_any!(args);
            let p = resolve_path(ev, &s(args, 0, name)?);
            std::fs::read_to_string(&p).map(Value::String).map_err(|e| EvalError(format!("file({}): {e}", p.display())))
        }
        "fileexists" => {
            unknown_if_any!(args);
            Ok(Value::Bool(resolve_path(ev, &s(args, 0, name)?).is_file()))
        }
        "templatefile" => {
            unknown_if_any!(args);
            let p = resolve_path(ev, &s(args, 0, name)?);
            let src = std::fs::read_to_string(&p).map_err(|e| EvalError(format!("templatefile: {e}")))?;
            let vars = match args.get(1) {
                Some(Value::Map(m)) => m.clone(),
                _ => Map::new(),
            };
            ev.eval_template_str(&src, &vars)
        }
        "fileset" => {
            unknown_if_any!(args);
            let base = resolve_path(ev, &s(args, 0, name)?);
            let pat = s(args, 1, name)?;
            let glob = globset::Glob::new(&pat).map_err(|e| EvalError(e.to_string()))?.compile_matcher();
            let mut out = vec![];
            for e in walkdir::WalkDir::new(&base).into_iter().flatten() {
                if e.file_type().is_file()
                    && let Ok(rel) = e.path().strip_prefix(&base)
                    && glob.is_match(rel)
                {
                    out.push(rel.to_string_lossy().to_string());
                }
            }
            out.sort();
            Ok(Value::List(out.into_iter().map(Value::String).collect()))
        }
        "abspath" => {
            str1(args, name, |p| std::path::absolute(p).map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string()))
        }
        "dirname" => str1(args, name, |p| Path::new(p).parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| ".".into())),
        "basename" => str1(args, name, |p| Path::new(p).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default()),
        "pathexpand" => str1(args, name, |p| {
            if let Some(rest) = p.strip_prefix("~/") {
                dirs::home_dir().map(|h| h.join(rest).to_string_lossy().to_string()).unwrap_or_else(|| p.into())
            } else {
                p.to_string()
            }
        }),

        _ => {
            tracing::debug!("unsupported function: {name}");
            Ok(Value::Unknown)
        }
    }
}

fn num1(args: &[Value], name: &str, f: fn(f64) -> f64) -> EvalResult {
    unknown_if_any!(args);
    Ok(Value::Number(f(n(args, 0, name)?)))
}

fn str1(args: &[Value], name: &str, f: impl Fn(&str) -> String) -> EvalResult {
    unknown_if_any!(args);
    Ok(Value::String(f(&s(args, 0, name)?)))
}

fn flatten_args(args: &[Value]) -> Vec<Value> {
    if args.len() == 1
        && let Value::List(l) = &args[0]
    {
        return l.clone();
    }
    args.to_vec()
}

fn dedup(l: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(l.len());
    for v in l {
        if !out.iter().any(|x| values_equal(x, &v)) {
            out.push(v);
        }
    }
    out
}

/// Implements Terraform's `format` verbs: %s %d %f %v %q %t %% with width/precision.
pub fn format(f: &str, args: &[Value]) -> Result<String, EvalError> {
    let mut out = String::new();
    let chars: Vec<char> = f.chars().collect();
    let mut i = 0;
    let mut ai = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let mut flags = String::new();
        while i < chars.len() && "-+# 0".contains(chars[i]) {
            flags.push(chars[i]);
            i += 1;
        }
        let mut width = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            width.push(chars[i]);
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                p.push(chars[i]);
                i += 1;
            }
            prec = Some(p.parse().unwrap_or(0));
        }
        if i >= chars.len() {
            break;
        }
        let verb = chars[i];
        i += 1;
        let a = args.get(ai).ok_or_else(|| EvalError("format: not enough arguments".into()))?;
        ai += 1;
        let mut s = match verb {
            's' | 'v' => match a {
                Value::String(s) => s.clone(),
                Value::Number(n) => fmt_number(*n),
                other => other.to_json().to_string(),
            },
            'q' => format!("{:?}", a.to_interp_string().unwrap_or_default()),
            'd' => format!("{}", a.as_f64().unwrap_or(0.0) as i64),
            'f' | 'e' | 'g' => format!("{:.*}", prec.unwrap_or(6), a.as_f64().unwrap_or(0.0)),
            't' => a.as_bool().unwrap_or(false).to_string(),
            'x' => format!("{:x}", a.as_f64().unwrap_or(0.0) as i64),
            'X' => format!("{:X}", a.as_f64().unwrap_or(0.0) as i64),
            'o' => format!("{:o}", a.as_f64().unwrap_or(0.0) as i64),
            'b' => format!("{:b}", a.as_f64().unwrap_or(0.0) as i64),
            _ => return err(format!("format: unsupported verb %{verb}")),
        };
        if verb == 's'
            && let Some(p) = prec
        {
            s = s.chars().take(p).collect();
        }
        if let Ok(w) = width.parse::<usize>() {
            let len = s.chars().count();
            if len < w {
                let pad = w - len;
                if flags.contains('-') {
                    s.push_str(&" ".repeat(pad));
                } else if flags.contains('0') && matches!(verb, 'd' | 'f' | 'x' | 'X' | 'o' | 'b') {
                    let neg = s.starts_with('-');
                    let digits = if neg { &s[1..] } else { &s[..] };
                    s = format!("{}{}{}", if neg { "-" } else { "" }, "0".repeat(pad), digits);
                } else {
                    s = format!("{}{}", " ".repeat(pad), s);
                }
            }
        }
        out.push_str(&s);
    }
    Ok(out)
}

/// Compiled regexes of `regex`, `regexall` and `replace`: the same patterns
/// are evaluated for every instance of a resource or module.
fn cached_regex(pat: &str) -> Result<std::sync::Arc<regex::Regex>, regex::Error> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    static CACHE: LazyLock<Mutex<HashMap<String, Arc<regex::Regex>>>> = LazyLock::new(Default::default);
    if let Some(r) = CACHE.lock().ok().and_then(|c| c.get(pat).cloned()) {
        return Ok(r);
    }
    let re = Arc::new(regex::Regex::new(pat)?);
    if let Ok(mut c) = CACHE.lock() {
        if c.len() > 1024 {
            c.clear();
        }
        c.insert(pat.to_string(), re.clone());
    }
    Ok(re)
}
