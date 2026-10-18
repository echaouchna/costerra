//! `--rightsize`: rightsizing suggestions based on observed usage. Each one
//! says why it is suggested (the metrics and the threshold they crossed),
//! where the value is set in the code when it can be found, the cost before
//! and after the change, and a link to the resource in the Google Cloud
//! console to check the metrics. The code is never edited.

use crate::cost::{Basis, CodeChange, Report};
use crate::engine::address_group;
use crate::project::Project;
use crate::tf::evaluate::ResourceInstance;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where a value is set in the code.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Location {
    pub file: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// The line as it is now (trimmed).
    pub text: String,
    /// The attribute is not set: it goes into the block starting at this line.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insert: bool,
    /// The entry of a map that holds the value (`dev` in
    /// `memory = { dev = "2Gi", prod = "4Gi" }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The value is computed on this line (from a variable, a map…): it is
    /// not written as such.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub computed: bool,
}

impl Location {
    fn at(file: &Path, line: usize, text: &str) -> Location {
        Location { file: file.to_path_buf(), line, text: text.trim().to_string(), insert: false, entry: None, computed: false }
    }

    /// The line with the suggested value, when the value is written on it.
    pub fn changed(&self, change: &CodeChange) -> Option<String> {
        if self.insert || self.computed {
            return None;
        }
        let keys: Vec<String> = self.entry.iter().cloned().collect();
        replace_value_in(&self.text, change, &keys)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub project: String,
    /// Address as shown in the report (`name[*] (N instances)` for groups).
    pub address: String,
    pub rtype: String,
    pub instances: usize,
    pub title: String,
    /// Why: the observed metrics and the threshold they crossed.
    pub detail: String,
    pub change: CodeChange,
    /// Monthly cost of the resources concerned, before and after the change.
    pub monthly_cost: f64,
    pub monthly_cost_after: f64,
    /// False when the cost of the change cannot be estimated (usage-based
    /// costs without usage): the costs above are then unchanged.
    pub cost_known: bool,
    /// Where the value is set: one line, several candidates, or none found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<Location>,
    /// What to know before changing it (shared value, module default…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The resource in the Google Cloud console, to check the metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
}

impl Suggestion {
    pub fn savings(&self) -> f64 {
        self.monthly_cost - self.monthly_cost_after
    }
}

/// Rightsizing of a report: suggestions and totals.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub suggestions: Vec<Suggestion>,
    pub monthly_cost: f64,
    pub monthly_cost_after: f64,
}

/// Build the suggestions from the recommendations of `report` (computed with
/// observed usage). `var_files` are the `--var-file`s of the run.
pub fn plan(report: &Report, projects: &[Project], var_files: &[PathBuf]) -> Plan {
    let mut finder = Finder { files: HashMap::new(), var_files, cache_dir: crate::pricing::catalog::default_cache_dir() };
    let mut suggestions = vec![];
    // (project, address) of the resources each suggestion covers.
    let mut covered: Vec<Vec<(String, String)>> = vec![];
    for pr in &report.projects {
        let Some(p) = projects.iter().find(|p| p.name == pr.name) else { continue };
        // Recommendations based on data (observed usage, usage file) that
        // come with a single-value change of the code.
        for rec in pr.recommendations.iter().filter(|r| r.change.is_some() && r.basis != Basis::Code) {
            let Some(change) = &rec.change else { continue };
            let (group, n) = match rec.address.rsplit_once(" (") {
                Some((g, rest)) => (g.to_string(), rest.trim_end_matches(" instances)").parse().unwrap_or(1)),
                None => (rec.address.clone(), 1),
            };
            let members: Vec<&ResourceInstance> =
                p.resources.iter().filter(|r| (r.address == group || address_group(&r.address) == group) && has_value(r, change)).collect();
            let Some(first) = members.first() else { continue };
            let before: f64 = pr.resources.iter().filter(|c| members.iter().any(|m| m.address == c.address)).map(|c| c.monthly_cost).sum();
            let saved = rec.monthly_savings.unwrap_or(0.0);
            let (locations, mut note) = finder.locate(p, first, change);
            if members.len() != n {
                note = Some(format!(
                    "only {n} of the {} instances need it: they share this value, so change it for those instances only (for example with a separate variable)",
                    members.len()
                ));
            }
            suggestions.push(Suggestion {
                project: pr.name.clone(),
                address: rec.address.clone(),
                rtype: first.rtype.clone(),
                instances: n,
                title: rec.title.clone(),
                detail: rec.detail.clone(),
                change: change.clone(),
                monthly_cost: before,
                monthly_cost_after: before - saved,
                cost_known: rec.monthly_savings.is_some(),
                locations,
                note,
                console_url: rec.console_url.clone(),
            });
            covered.push(members.iter().map(|m| (p.name.clone(), m.address.clone())).collect());
        }
    }

    // A line that also sets the value of other resources: changing it changes them too.
    let mut users: HashMap<Location, Vec<(String, String)>> = HashMap::new();
    let wanted: Vec<(&Location, &CodeChange, &str)> =
        suggestions.iter().filter(|s| s.locations.len() == 1).map(|s| (&s.locations[0], &s.change, s.rtype.as_str())).collect();
    for p in projects {
        for r in &p.resources {
            for (loc, change, rtype) in &wanted {
                if r.rtype != *rtype || !has_value(r, change) {
                    continue;
                }
                if finder.locate(p, r, change).0.as_slice() == [(*loc).clone()] {
                    users.entry((*loc).clone()).or_default().push((p.name.clone(), r.address.clone()));
                }
            }
        }
    }
    for i in 0..suggestions.len() {
        let [loc] = suggestions[i].locations.as_slice() else { continue };
        let loc = loc.clone();
        let same: Vec<usize> = (0..suggestions.len())
            .filter(|&j| suggestions[j].locations.as_slice() == [loc.clone()] && suggestions[j].change == suggestions[i].change)
            .collect();
        let mine: Vec<&(String, String)> = same.iter().flat_map(|&j| covered[j].iter()).collect();
        let others: Vec<&(String, String)> = users.get(&loc).into_iter().flatten().filter(|u| !mine.contains(u)).collect();
        if suggestions[i].note.is_none()
            && let Some((project, address)) = others.first()
        {
            let more = if others.len() > 1 { format!(" and {} more", others.len() - 1) } else { String::new() };
            // `. (dev)` is the dev environment of the analysed directory.
            let project = match project.strip_prefix(". (") {
                Some(env) => format!("the {} environment's", env.trim_end_matches(')')),
                None => format!("{project}:"),
            };
            let (it, does) = if others.len() > 1 { ("they", "do") } else { ("it", "does") };
            suggestions[i].note = Some(format!(
                "this line also sets {project} {address}{more}: {it} {does} not need the change, change it for this resource only"
            ));
        }
    }

    let saved: f64 = suggestions.iter().map(Suggestion::savings).sum();
    Plan { suggestions, monthly_cost: report.monthly_cost, monthly_cost_after: report.monthly_cost - saved }
}

/// The attribute as written in the code (`settings.0.tier` → `tier`).
fn key(change: &CodeChange) -> &str {
    change.attribute.rsplit('.').next().unwrap_or(&change.attribute)
}

/// The attribute path as shown to people (`settings.0.tier` → `settings.tier`).
pub fn display_attribute(attribute: &str) -> String {
    attribute.split('.').filter(|s| s.parse::<usize>().is_err()).collect::<Vec<_>>().join(".")
}

fn numeric(v: &str) -> bool {
    v.parse::<f64>().is_ok()
}

/// Regex of a value as written in the code: `"v"`, or also `v` for numbers.
fn value_re(v: &str) -> String {
    let e = regex::escape(v);
    if numeric(v) { format!(r#"(?:"{e}"|{e}\b)"#) } else { format!(r#""{e}""#) }
}

/// What precedes a value: `key = `, `key: ` or `optional(type, `.
/// `key = `, `"key": ` or `optional(type, ` before a value; `key` is captured.
const ASSIGN: &str = r#"(?:"?(?P<key>[A-Za-z0-9_.-]+)"?\s*[=:]|(?P<opt>optional\([^,()]+,))\s*"#;

/// The word a key must contain to be about this attribute: `machine` for
/// `machine_type`, the whole key for numbers (`min_instance`), which are not
/// distinctive.
fn key_word(change: &CodeChange) -> String {
    let k = key(change);
    if numeric(&change.from) { k.trim_end_matches("_count").to_lowercase() } else { k.split('_').next().unwrap_or(k).to_lowercase() }
}

/// Where the value is assigned to a key about the attribute on this line:
/// `machine_type = "x"`, or `optional(string, "x")` on a line about it. The
/// value assigned to another key (`dev = "x"` in a per-environment map) does
/// not count, so that the right entry is edited.
fn value_span(line: &str, change: &CodeChange) -> Option<(usize, usize)> {
    value_span_in(line, change, &[]).map(|(s, e, _)| (s, e))
}

/// Like `value_span`, also accepting an entry whose key is one of `entries`
/// (an environment or a service name) on a line about the attribute:
/// `memory = { dev = "2Gi", prod = "4Gi" }` for the `dev` environment.
/// Returns the entry key used, if any.
fn value_span_in(line: &str, change: &CodeChange, entries: &[String]) -> Option<(usize, usize, Option<String>)> {
    let word = key_word(change);
    let about_line = line.to_lowercase().contains(&word);
    let re = regex::Regex::new(&format!("{ASSIGN}(?P<value>{})", value_re(&change.from))).ok()?;
    re.captures_iter(line).find_map(|c| {
        let v = c.name("value")?;
        match (c.name("key"), c.name("opt")) {
            (Some(k), _) if k.as_str().to_lowercase().contains(&word) => Some((v.start(), v.end(), None)),
            (Some(k), _) if about_line && entries.iter().any(|e| e == k.as_str()) => {
                Some((v.start(), v.end(), Some(k.as_str().to_string())))
            }
            (None, Some(_)) if about_line => Some((v.start(), v.end(), None)),
            _ => None,
        }
    })
}

/// The line with the old value replaced by the new one (quoting kept).
pub fn replace_value(line: &str, change: &CodeChange) -> Option<String> {
    replace_value_in(line, change, &[])
}

fn replace_value_in(line: &str, change: &CodeChange, entries: &[String]) -> Option<String> {
    let (start, end, _) = value_span_in(line, change, entries)?;
    let new = if line[start..end].starts_with('"') { format!("\"{}\"", change.to) } else { change.to.clone() };
    Some(format!("{}{new}{}", &line[..start], &line[end..]))
}

/// The line added for an attribute that is not set.
pub fn inserted(change: &CodeChange) -> String {
    if numeric(&change.to) { format!("{} = {}", key(change), change.to) } else { format!("{} = \"{}\"", key(change), change.to) }
}

/// Whether the resource has the value the change replaces.
fn has_value(r: &ResourceInstance, change: &CodeChange) -> bool {
    use crate::hcl::value::Value;
    match r.attrs.path(&change.attribute) {
        None | Some(Value::Null) => change.unset,
        Some(Value::String(s)) => !change.unset && *s == change.from,
        Some(v) => !change.unset && v.as_f64().is_some_and(|n| change.from.parse::<f64>().is_ok_and(|f| f == n)),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |x: &Path| x.canonicalize().unwrap_or_else(|_| x.to_path_buf());
    canon(a) == canon(b)
}

struct Finder<'a> {
    files: HashMap<PathBuf, Option<String>>,
    var_files: &'a [PathBuf],
    cache_dir: PathBuf,
}

impl Finder<'_> {
    fn read(&mut self, f: &Path) -> Option<&str> {
        self.files.entry(f.to_path_buf()).or_insert_with(|| std::fs::read_to_string(f).ok()).as_deref()
    }

    /// The lines that may set `change.from` for this resource (one when it is
    /// clear, the candidates otherwise, or the line that computes it), and a
    /// note saying what to do.
    fn locate(&mut self, p: &Project, r: &ResourceInstance, change: &CodeChange) -> (Vec<Location>, Option<String>) {
        let downloaded = |l: &Location, cache: &Path| l.file.starts_with(cache).then(|| "set in a downloaded module".to_string());
        let module = module_name(r);
        // 0. An attribute that is not set: it goes into the resource block.
        if change.unset {
            let Some(file) = r.file.as_deref() else { return (vec![], None) };
            let Some((header, _)) = self.resource_block(file, r) else { return (vec![], None) };
            let mut l = Location::at(file, header.0, &header.1);
            l.insert = true;
            let note = downloaded(&l, &self.cache_dir).or_else(|| {
                file.parent()
                    .is_some_and(|m| !same_dir(m, &p.path))
                    .then(|| format!("the resource is defined in a module: add {} there, or pass it as a variable", inserted(change)))
            });
            return (vec![l], note);
        }
        // 1. A literal value in the resource block itself.
        if let Some(file) = r.file.as_deref()
            && let Some(l) = self.in_block(file, r, change)
        {
            let note = downloaded(&l, &self.cache_dir);
            return (vec![l], note);
        }
        // Keys that select this resource's entry in a map: its environment,
        // its for_each keys, and the keys its code uses (`var.apps["api"]`).
        let env = p.name.rsplit_once(" (").map(|(_, e)| e.trim_end_matches(')').to_string());
        let mut selectors: Vec<String> = env.iter().cloned().collect();
        selectors.extend(quoted(&r.address));
        for (_, l) in self.source_lines(p, r, module.as_deref(), change) {
            selectors.extend(quoted(&l.text));
        }
        selectors.sort();
        selectors.dedup();

        // 2. A value assigned elsewhere: var files, the project files (tfvars,
        // terragrunt inputs, the module call), parent Terragrunt files, module defaults.
        let mut files: Vec<(PathBuf, u32)> = self.var_files.iter().map(|f| (std::path::absolute(f).unwrap_or(f.clone()), 60)).collect();
        for f in list(&p.path, &["tfvars", "hcl", "tf"]) {
            files.push((f, 50));
        }
        for d in ["environments", "envs", "env", "vars", "tfvars"] {
            for f in list(&p.path.join(d), &["tfvars"]) {
                let stem = f.file_stem().map(|s| s.to_string_lossy().trim_end_matches(".auto").to_string());
                match &env {
                    Some(e) if stem.as_deref() == Some(e.as_str()) => files.push((f, 55)),
                    Some(_) => {}
                    None => files.push((f, 50)),
                }
            }
        }
        let mut dir = p.path.parent();
        while let Some(d) = dir {
            for f in list(d, &["hcl"]) {
                files.push((f, 20));
            }
            if d.join(".git").exists() {
                break;
            }
            dir = d.parent();
        }
        if let Some(m) = r.file.as_deref().and_then(|f| f.parent()).filter(|m| !same_dir(m, &p.path)) {
            for f in list(m, &["tf"]) {
                files.push((f, 10));
            }
        }
        let mut found: Vec<(Location, u32)> = vec![];
        for (f, score) in files {
            for l in self.assignments(&f, change, &selectors) {
                if !found.iter().any(|(x, _)| *x == l) {
                    found.push((l, score));
                }
            }
        }
        if let Some(best) = found.iter().map(|(_, s)| *s).max() {
            let mut top: Vec<Location> = found.into_iter().filter(|(_, s)| *s == best).map(|(l, _)| l).collect();
            // Several lines: keep the one in this resource's entry (`backend = { … }`).
            if top.len() > 1 {
                let mine: Vec<Location> = top
                    .iter()
                    .filter(|l| l.entry.is_some() || self.enclosing_key(&l.file, l.line).is_some_and(|k| selectors.contains(&k)))
                    .cloned()
                    .collect();
                if !mine.is_empty() {
                    top = mine;
                }
            }
            let note = if top.len() > 1 {
                let keys: Vec<String> = top
                    .iter()
                    .map(|l| self.enclosing_key(&l.file, l.line).map_or_else(|| "?".to_string(), |k| format!("\"{k}\"")))
                    .collect();
                Some(format!(
                    "{} = {} is set on {} lines (in {}): change the one that configures {}",
                    key(change),
                    change.from,
                    top.len(),
                    keys.join(", "),
                    r.address
                ))
            } else if let Some(n) = downloaded(&top[0], &self.cache_dir) {
                Some(n)
            } else if best <= 10 {
                Some(format!("module default, used by every caller of the module: set {} where the module is called", key(change)))
            } else {
                None
            };
            return (top, note);
        }

        // 3. Not written as such: the lines that compute it (in the module
        // call, or in the resource block).
        let computing: Vec<Location> = self.source_lines(p, r, module.as_deref(), change).into_iter().map(|(_, l)| l).collect();
        let to = if numeric(&change.to) { change.to.clone() } else { format!("\"{}\"", change.to) };
        let env_part = env.as_deref().map(|e| format!(" for {e}")).unwrap_or_default();
        let note = match computing.first() {
            Some(l) => Some(format!(
                "the value is computed from {}: make it give {to}{env_part} (where that expression is defined)",
                expression_of(&l.text)
            )),
            None => Some(match &module {
                Some(m) => format!("{} is set inside module \"{m}\": pass {to} to the module call{env_part}", key(change)),
                None => format!(
                    "{} = {} is computed: search the code for {} and make it give {to}{env_part}",
                    key(change),
                    change.from,
                    key(change)
                ),
            }),
        };
        (
            computing
                .into_iter()
                .map(|mut l| {
                    l.computed = true;
                    l
                })
                .collect(),
            note,
        )
    }

    /// Lines of the resource block and of its module call (in the analysed
    /// directory) that are about the attribute: they set or compute its value.
    fn source_lines(&mut self, p: &Project, r: &ResourceInstance, module: Option<&str>, change: &CodeChange) -> Vec<(u32, Location)> {
        let word = key_word(change);
        let about = |l: &str| {
            let t = l.trim_start();
            !t.starts_with('#')
                && !t.starts_with("//")
                && l.split(['=', ':']).next().is_some_and(|k| k.to_lowercase().contains(&word))
                && l.contains('=')
        };
        let mut out = vec![];
        if let Some(m) = module {
            let head = format!(r#"module\s+"{}"\s*\{{"#, regex::escape(m));
            for f in list(&p.path, &["tf"]) {
                if let Some((first, body)) = self.block(&f, &head).map(|(h, b)| (h.0, b.1)) {
                    out.extend(body.lines().enumerate().filter(|(_, l)| about(l)).map(|(i, l)| (50, Location::at(&f, first + i, l))));
                }
            }
        }
        if let Some(file) = r.file.as_deref().filter(|f| !f.starts_with(&self.cache_dir))
            && let Some(((first, _), (_, body))) = self.resource_block(file, r)
        {
            out.extend(body.lines().enumerate().filter(|(_, l)| about(l)).map(|(i, l)| (100, Location::at(file, first + i, l))));
        }
        out
    }

    /// The key of the map that contains a line (`backend` for a line inside
    /// `backend = { … }`).
    fn enclosing_key(&mut self, file: &Path, line: usize) -> Option<String> {
        let src = self.read(file)?;
        let lines: Vec<&str> = src.lines().collect();
        let open = regex::Regex::new(r#"^\s*"?([A-Za-z0-9_.-]+)"?\s*[=:]?\s*\{"#).expect("valid regex");
        let mut depth = 0i32;
        for l in lines[..line.saturating_sub(1).min(lines.len())].iter().rev() {
            depth += l.matches('}').count() as i32 - l.matches('{').count() as i32;
            if depth < 0 {
                return open.captures(l).map(|c| c[1].to_string());
            }
        }
        None
    }

    /// A block whose header matches `head`: the header line (number, trimmed
    /// text) and the block source.
    fn block(&mut self, file: &Path, head: &str) -> Option<((usize, String), (usize, String))> {
        let src = self.read(file)?;
        let m = regex::Regex::new(head).ok()?.find(src)?;
        // End of the block: matching brace (strings are rare enough in a block header to ignore).
        let mut depth = 0i32;
        let mut end = src.len();
        for (i, c) in src[m.end() - 1..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = m.end() - 1 + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let first_line = src[..m.start()].matches('\n').count() + 1;
        let header = src[m.start()..].lines().next().unwrap_or_default().trim().to_string();
        Some(((first_line, header), (first_line, src[m.start()..end].to_string())))
    }

    fn resource_block(&mut self, file: &Path, r: &ResourceInstance) -> Option<((usize, String), (usize, String))> {
        self.block(file, &format!(r#"resource\s+"{}"\s+"{}"\s*\{{"#, regex::escape(&r.rtype), regex::escape(&r.name)))
    }

    fn in_block(&mut self, file: &Path, r: &ResourceInstance, change: &CodeChange) -> Option<Location> {
        let (_, (first_line, body)) = self.resource_block(file, r)?;
        // `key = value`, `"key" = value` or an entry of an inline map, not in a comment.
        let k = key(change);
        body.lines()
            .enumerate()
            .find(|(_, l)| {
                let t = l.trim_start();
                !t.starts_with('#') && !t.starts_with("//") && value_span(l, change).is_some() && l.contains(k)
            })
            .map(|(i, l)| Location::at(file, first_line + i, l))
    }

    /// Lines of `file` that assign the value to a key about this attribute,
    /// or to the entry of this resource (`dev = "2Gi"` on a memory line).
    fn assignments(&mut self, file: &Path, change: &CodeChange, entries: &[String]) -> Vec<Location> {
        let Some(src) = self.read(file) else { return vec![] };
        src.lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let t = l.trim_start();
                if t.starts_with('#') || t.starts_with("//") {
                    return None;
                }
                let (_, _, entry) = value_span_in(l, change, entries)?;
                let mut loc = Location::at(file, i + 1, l);
                loc.entry = entry;
                Some(loc)
            })
            .collect()
    }
}

/// The module call of the root configuration a resource comes from
/// (`geniall` for `module.geniall.google_cloud_run_v2_service.cloudrun`).
fn module_name(r: &ResourceInstance) -> Option<String> {
    let rest = r.module_path.strip_prefix("module.")?;
    Some(rest.split(['.', '[']).next()?.to_string())
}

/// Quoted strings of a text (`"backend"` in `var.apps["backend"].cpu`).
fn quoted(text: &str) -> Vec<String> {
    text.split('"').skip(1).step_by(2).filter(|s| !s.is_empty() && s.len() < 64).map(str::to_string).collect()
}

/// The right-hand side of an assignment line.
fn expression_of(line: &str) -> String {
    line.split_once('=').map(|(_, e)| e.trim().to_string()).unwrap_or_else(|| line.trim().to_string())
}

/// Files of `dir` (not recursive) with one of these extensions, sorted.
fn list(dir: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| exts.iter().any(|x| e == *x)))
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load a project from files, and locate a change for one resource.
    fn locate_in(files: &[(&str, &str)], project: &str, address: &str, c: &CodeChange) -> (Vec<Location>, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        for (f, src) in files {
            let path = dir.path().join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, src).unwrap();
        }
        let opts = crate::project::LoadOptions { offline: true, cache_dir: dir.path().join(".cache"), ..Default::default() };
        let projects = crate::project::load(dir.path(), &opts).unwrap();
        // The root of the temporary directory is named after it: match on the environment.
        let env = project.strip_prefix('.').unwrap_or(project).trim();
        let p = projects
            .iter()
            .find(|p| if env.is_empty() { !p.name.ends_with(')') } else { p.name.ends_with(env) })
            .unwrap_or_else(|| panic!("{:?}", projects.iter().map(|p| &p.name).collect::<Vec<_>>()));
        let r = p
            .resources
            .iter()
            .find(|r| r.address == address)
            .unwrap_or_else(|| panic!("{:?}", p.resources.iter().map(|r| &r.address).collect::<Vec<_>>()));
        let mut finder = Finder { files: HashMap::new(), var_files: &[], cache_dir: dir.path().join(".cache") };
        let (mut locs, note) = finder.locate(p, r, c);
        for l in &mut locs {
            l.file = l.file.file_name().unwrap().into();
        }
        (locs, note)
    }

    const SERVICE_MODULE: &str = r#"
variable "env" {}
variable "memory" {}
variable "min_instances" { default = 0 }
resource "google_cloud_run_v2_service" "this" {
  name     = "svc-${var.env}"
  location = "europe-west1"
  template {
    scaling {
      min_instance_count = var.min_instances
    }
    containers {
      image = "img"
      resources {
        limits = { cpu = "1", memory = var.memory[var.env] }
      }
    }
  }
}
"#;

    #[test]
    fn the_entry_of_the_environment_in_a_module_call() {
        let files = [
            ("mod/main.tf", SERVICE_MODULE),
            (
                "main.tf",
                "variable \"env\" {}\nmodule \"svc\" {\n  source = \"./mod\"\n  env    = var.env\n  memory = { dev = \"2048Mi\", prod = \"2048Mi\" }\n}\n",
            ),
            ("environments/dev.tfvars", "env = \"dev\"\n"),
            ("environments/prod.tfvars", "env = \"prod\"\n"),
        ];
        let c = change("template.0.containers.0.resources.0.limits.memory", "2048Mi", "1Gi");
        let (locs, note) = locate_in(&files, ". (prod)", "module.svc.google_cloud_run_v2_service.this", &c);
        assert!(note.is_none(), "{note:?}");
        let [l] = locs.as_slice() else { panic!("{locs:?}") };
        assert_eq!((l.line, l.entry.as_deref()), (5, Some("prod")));
        // Only the prod entry changes.
        assert_eq!(l.changed(&c).unwrap(), r#"memory = { dev = "2048Mi", prod = "1Gi" }"#);
    }

    #[test]
    fn the_entry_of_the_service_among_several_lines() {
        let files = [
            ("mod/main.tf", SERVICE_MODULE),
            (
                "main.tf",
                r#"variable "app_config" {}
module "frontend" {
  source        = "./mod"
  env           = "dev"
  memory        = { dev = "1Gi" }
  min_instances = var.app_config["frontend"].min_instance_count
}
module "backend" {
  source        = "./mod"
  env           = "dev"
  memory        = { dev = "1Gi" }
  min_instances = var.app_config["backend"].min_instance_count
}
"#,
            ),
            (
                "terraform.tfvars",
                "app_config = {\n  frontend = {\n    min_instance_count = 1\n  }\n  backend = {\n    min_instance_count = 1\n  }\n}\n",
            ),
        ];
        let c = change("template.0.scaling.0.min_instance_count", "1", "0");
        let (locs, note) = locate_in(&files, ".", "module.backend.google_cloud_run_v2_service.this", &c);
        assert!(note.is_none(), "{note:?}");
        let [l] = locs.as_slice() else { panic!("{locs:?}") };
        assert_eq!((l.file.to_str(), l.line), (Some("terraform.tfvars"), 6));
        assert_eq!(l.changed(&c).unwrap(), "min_instance_count = 0");
    }

    #[test]
    fn a_computed_value_points_at_the_line_that_computes_it() {
        let files = [(
            "main.tf",
            r#"variable "memory_mb" { default = 2048 }
resource "google_cloud_run_v2_service" "api" {
  name     = "api"
  location = "europe-west1"
  template {
    containers {
      image = "img"
      resources {
        limits = {
          memory = "${var.memory_mb}Mi"
        }
      }
    }
  }
}
"#,
        )];
        let c = change("template.0.containers.0.resources.0.limits.memory", "2048Mi", "1Gi");
        let (locs, note) = locate_in(&files, ".", "google_cloud_run_v2_service.api", &c);
        let [l] = locs.as_slice() else { panic!("{locs:?}") };
        assert!(l.computed && l.line == 10 && l.changed(&c).is_none(), "{l:?}");
        let note = note.unwrap();
        assert!(note.contains(r#"computed from "${var.memory_mb}Mi": make it give "1Gi""#), "{note}");
    }

    fn change(attribute: &str, from: &str, to: &str) -> CodeChange {
        CodeChange { attribute: attribute.into(), from: from.into(), to: to.into(), unset: false }
    }

    #[test]
    fn values_are_replaced_with_their_quoting() {
        let c = change("machine_type", "n2-standard-8", "n2-standard-4");
        assert_eq!(replace_value(r#"  machine_type = "n2-standard-8" # big"#, &c).unwrap(), r#"  machine_type = "n2-standard-4" # big"#);
        assert_eq!(
            replace_value(r#"worker = { machine_type: "n2-standard-8" }"#, &c).unwrap(),
            r#"worker = { machine_type: "n2-standard-4" }"#
        );
        assert_eq!(
            replace_value(r#"machine_type = optional(string, "n2-standard-8")"#, &c).unwrap(),
            r#"machine_type = optional(string, "n2-standard-4")"#
        );
        assert!(replace_value(r#"name = "n2-standard-8-pool""#, &c).is_none());
        let n = change("template.0.scaling.0.min_instance_count", "2", "0");
        assert_eq!(replace_value("min_instance_count = 2", &n).unwrap(), "min_instance_count = 0");
        assert_eq!(replace_value(r#"min_instance_count = "2""#, &n).unwrap(), r#"min_instance_count = "0""#);
        assert!(replace_value("min_instance_count = 20", &n).is_none());
        assert_eq!(replace_value(r#""min_instance_count": 2,"#, &n).unwrap(), r#""min_instance_count": 0,"#);
    }

    #[test]
    fn only_the_value_of_the_attribute_key_is_edited() {
        // A per-environment map: the value belongs to `dev`, not to a cpu key.
        let cpu = change("cpu", "4", "2");
        assert!(replace_value(r#"resources_limits_cpu = { dev = "2", uat = "4", prod = "4" }"#, &cpu).is_none());
        assert_eq!(replace_value(r#"limits = { cpu = "4", memory = "4Gi" }"#, &cpu).unwrap(), r#"limits = { cpu = "2", memory = "4Gi" }"#);
        assert_eq!(
            replace_value(r#""cpu"    = "2000m"  # 2 vCPUs"#, &change("cpu", "2000m", "4000m")).unwrap(),
            r#""cpu"    = "4000m"  # 2 vCPUs"#
        );
        let m = change("machine_type", "n2-standard-8", "n2-standard-4");
        assert!(replace_value(r#"machine_types = { dev = "n2-standard-8", prod = "n2-standard-8" }"#, &m).is_none());
        assert_eq!(
            replace_value(r#"worker = { disk = "n2-standard-8", machine_type = "n2-standard-8" }"#, &m).unwrap(),
            r#"worker = { disk = "n2-standard-8", machine_type = "n2-standard-4" }"#
        );
    }

    #[test]
    fn attributes_are_shown_without_indexes() {
        assert_eq!(display_attribute("settings.0.tier"), "settings.tier");
        assert_eq!(key(&change("settings.0.tier", "a", "b")), "tier");
        assert_eq!(inserted(&change("storage_billing_model", "LOGICAL", "PHYSICAL")), r#"storage_billing_model = "PHYSICAL""#);
        assert_eq!(inserted(&change("min_instance_count", "1", "0")), "min_instance_count = 0");
    }
}
