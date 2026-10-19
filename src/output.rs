//! Report rendering: terminal table, JSON and Markdown (MR/PR comments).

use crate::cost::{Component, CostedResource, Recommendation, Report, Severity, Status};
use crate::diff::{Change, DiffReport};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// The terminal table
    #[value(alias = "console")]
    Table,
    Json,
    Markdown,
}

pub struct Style {
    pub color: bool,
    pub show_all: bool,
    pub show_warnings: bool,
    /// Collapse identical instances of a counted resource (`app[0]`…`app[3]`)
    /// into one `app[*] ×4` line.
    pub group: bool,
    /// `--details`: price notes, example usage per resource, recommendation
    /// sources and notes about data sources.
    pub details: bool,
}

impl Style {
    fn paint(&self, s: impl Into<String>) -> Paint {
        Paint { s: s.into(), on: self.color }
    }

    /// Color a (pre-padded) monthly cost by magnitude so that expensive items stand out.
    fn cost(&self, padded: String, v: f64) -> String {
        let p = self.paint(padded);
        if v >= 1000.0 {
            p.red().bold().s()
        } else if v >= 100.0 {
            p.yellow().bold().s()
        } else if v > 0.0 {
            p.s()
        } else {
            p.dim().s()
        }
    }

    /// Color a (pre-padded) cost difference: increases red, decreases green.
    fn delta(&self, padded: String, v: f64) -> String {
        let p = self.paint(padded);
        if v > 0.005 {
            p.red().bold().s()
        } else if v < -0.005 {
            p.green().bold().s()
        } else {
            p.dim().s()
        }
    }
}

/// Minimal ANSI styling that is a no-op when colors are disabled.
pub struct Paint {
    s: String,
    on: bool,
}

impl Paint {
    fn wrap(mut self, code: &str) -> Self {
        if self.on {
            self.s = format!("\x1b[{code}m{}\x1b[0m", self.s);
        }
        self
    }
    pub fn bold(self) -> Self {
        self.wrap("1")
    }
    pub fn dim(self) -> Self {
        self.wrap("2")
    }
    pub fn italic(self) -> Self {
        self.wrap("3")
    }
    pub fn red(self) -> Self {
        self.wrap("31")
    }
    pub fn green(self) -> Self {
        self.wrap("32")
    }
    pub fn yellow(self) -> Self {
        self.wrap("33")
    }
    pub fn blue(self) -> Self {
        self.wrap("34")
    }
    pub fn cyan(self) -> Self {
        self.wrap("36")
    }
    pub fn s(self) -> String {
        self.s
    }
}

pub fn money(currency: &str, v: f64) -> String {
    let abs = v.abs();
    let s = format!("{abs:.2}");
    // A residue that rounds to zero (`-1e-15`) is not negative.
    let neg = v < 0.0 && s != "0.00";
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sym = match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    };
    let body = if sym.is_empty() { format!("{grouped}.{frac} {currency}") } else { format!("{sym}{grouped}.{frac}") };
    if neg { format!("-{body}") } else { body }
}

fn signed(currency: &str, v: f64) -> String {
    if v >= 0.005 { format!("+{}", money(currency, v)) } else { money(currency, v) }
}

fn fmt_qty(q: f64) -> String {
    if q >= 1000.0 {
        let s = format!("{q:.0}");
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out
    } else if q.fract() == 0.0 {
        format!("{q:.0}")
    } else if q >= 10.0 {
        format!("{q:.1}")
    } else {
        format!("{q:.3}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn currency_symbol(currency: &str) -> &'static str {
    match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    }
}

/// Unit price with enough significant digits, without trailing noise:
/// 0.02 -> $0.02, 0.0054202 -> $0.00542, 3 -> $3.00.
pub fn unit_price(currency: &str, p: f64) -> String {
    let sym = currency_symbol(currency);
    let decimals = if p <= 0.0 { 2 } else { (3 - p.log10().floor() as i32).clamp(2, 7) as usize };
    let mut s = format!("{p:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') && s.len() - s.find('.').unwrap() > 3 {
            s.pop();
        }
    }
    if sym.is_empty() { format!("{s} {currency}") } else { format!("{sym}{s}") }
}

fn sev_label(s: Severity) -> &'static str {
    match s {
        Severity::High => "HIGH",
        Severity::Medium => "MEDIUM",
        Severity::Low => "LOW",
        Severity::Info => "INFO",
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------------
// Terminal table
// ---------------------------------------------------------------------------

const MAX_WIDTH: usize = 160;

/// Output width: the terminal width (capped), `COLUMNS`, or 120.
pub fn term_width() -> usize {
    if let Some((terminal_size::Width(w), _)) = terminal_size::terminal_size() {
        return (w as usize).clamp(60, MAX_WIDTH);
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse::<usize>().ok()).map(|w| w.clamp(60, MAX_WIDTH)).unwrap_or(120)
}

fn width_of(s: &str) -> usize {
    s.chars().count()
}

fn pad_right(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width_of(s))))
}

fn pad_left(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(width_of(s))))
}

enum CostCell {
    Amount(f64),
    Usage,
    Missing,
}

struct Cells {
    qty: String,
    unit: String,
    price: String,
    /// Public unit price, scaled like `price`.
    list_price: Option<String>,
    cost: CostCell,
}

/// Display values of a component. Tiny unit prices (per second / per
/// operation) are shown per million units.
fn cells(cur: &str, c: &Component) -> Cells {
    let tiny = c.unit_price.is_some_and(|p| p > 0.0 && p < 0.0001);
    let (scale, unit) = if tiny && c.unit.starts_with("MiB") {
        (1024.0, c.unit.replacen("MiB", "GiB", 1))
    } else if tiny {
        (1e6, format!("1M {}", c.unit))
    } else {
        (1.0, c.unit.clone())
    };
    let c_unit = unit.clone();
    Cells {
        qty: c.quantity.map(|q| fmt_qty(q / scale)).unwrap_or_default(),
        unit,
        price: c.unit_price.map(|p| unit_price(cur, p * scale)).unwrap_or_default(),
        list_price: c.list_unit_price.map(|p| format!("{} / {}", unit_price(cur, p * scale), singular_unit(&c_unit))),
        cost: match c.monthly_cost {
            Some(v) => CostCell::Amount(v),
            None if c.usage_based && c.quantity.is_none() => CostCell::Usage,
            None => CostCell::Missing,
        },
    }
}

/// Singular form of a unit label for "price / unit": `GiB-months` -> `GiB-month`,
/// `hours` -> `hour`; labels with a count prefix (`1M requests`) are kept.
fn singular_unit(unit: &str) -> String {
    if unit.contains(' ') {
        return unit.to_string();
    }
    if let Some(stem) = unit.strip_suffix("ies") {
        return format!("{stem}y");
    }
    match unit.strip_suffix('s') {
        Some(stem) if !stem.ends_with('s') && !stem.is_empty() => stem.to_string(),
        _ => unit.to_string(),
    }
}

/// Split an address at `.` boundaries outside brackets/quotes, keeping the dots.
fn address_segments(addr: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let (mut depth, mut quoted) = (0i32, false);
    for ch in addr.chars() {
        cur.push(ch);
        match ch {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth -= 1,
            '.' if !quoted && depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => {}
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Address units that should not be split: `module.NAME[key].` and the final `TYPE.NAME[key]`.
fn address_units(addr: &str) -> Vec<String> {
    let segs = address_segments(addr);
    let mut units: Vec<String> = vec![];
    let mut i = 0;
    while i < segs.len() {
        let pair = segs[i] == "module." || segs[i] == "data." || i + 2 == segs.len();
        if pair && i + 1 < segs.len() {
            units.push(format!("{}{}", segs[i], segs[i + 1]));
            i += 2;
        } else {
            units.push(segs[i].clone());
            i += 1;
        }
    }
    units
}

fn wrap_address(addr: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    let mut cur = String::new();
    for seg in address_units(addr) {
        if !cur.is_empty() && width_of(&cur) + width_of(&seg) > width {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push_str(&seg);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

struct Layout {
    name: usize,
    qty: usize,
    unit: usize,
    price: usize,
    cost: usize,
    /// Width of the public (list) cost column; 0 when list prices are used.
    public: usize,
}

impl Layout {
    fn tail(&self) -> usize {
        self.cost + if self.public > 0 { self.public + 2 } else { 0 }
    }

    fn total(&self) -> usize {
        1 + self.name + 2 + self.qty + 2 + self.unit + 2 + self.price + 2 + self.tail()
    }

    /// Width available left of the cost column(s) (for resource headers).
    fn header(&self) -> usize {
        self.total() - self.tail() - 3
    }

    fn compute(cur: &str, resources: &[&CostedResource], term: usize, contract: bool) -> Layout {
        let (price_h, cost_h) = if contract { ("Your Price", "Your Cost") } else { ("Unit Price", "Monthly Cost") };
        let mut l = Layout {
            name: 20,
            qty: "Monthly Qty".len(),
            unit: "Unit".len(),
            price: price_h.len(),
            cost: cost_h.len(),
            public: if contract { "Public Cost".len() } else { 0 },
        };
        let mut name_needed = 0;
        let mut addr_needed = 0;
        for r in resources {
            addr_needed = addr_needed.max(width_of(&r.address));
            l.cost = l.cost.max(width_of(&money(cur, r.monthly_cost)));
            for c in &r.components {
                name_needed = name_needed.max(width_of(&c.name) + 3);
                let x = cells(cur, c);
                l.qty = l.qty.max(width_of(&x.qty));
                l.unit = l.unit.max(width_of(&x.unit));
                l.price = l.price.max(width_of(&x.price));
                if let CostCell::Amount(v) = x.cost {
                    l.cost = l.cost.max(width_of(&money(cur, v)));
                }
                if contract {
                    if let Some(v) = c.list_monthly_cost {
                        l.public = l.public.max(width_of(&money(cur, v)));
                    }
                    if let Some(p) = &x.list_price {
                        l.public = l.public.max(width_of(p));
                    }
                }
            }
            if contract {
                l.public = l.public.max(width_of(&money(cur, r.list_monthly_cost.unwrap_or(r.monthly_cost))));
            }
        }
        let others = l.total() - l.name;
        // Headers span the qty/unit/price columns, so an address needs less name width.
        let for_addr = addr_needed.saturating_sub(l.qty + l.unit + l.price + 6);
        l.name = name_needed.max(for_addr).max(20).min(term.saturating_sub(others).max(20));
        l
    }
}

fn cost_cell(st: &Style, cur: &str, cell: &CostCell, w: usize) -> String {
    match cell {
        CostCell::Amount(v) => st.cost(pad_left(&money(cur, *v), w), *v),
        CostCell::Usage => st.paint(pad_left("usage-based", w)).dim().italic().s(),
        CostCell::Missing => st.paint(pad_left("no price", w)).red().bold().s(),
    }
}

/// The public (list price) cell, shown when contract prices are used. For
/// usage-based lines it shows the public unit price (`$0.40 / 1M requests`).
fn public_cell(st: &Style, cur: &str, l: &Layout, own: &CostCell, list: Option<f64>, list_price: Option<&str>) -> String {
    if l.public == 0 {
        return String::new();
    }
    let text = match (own, list, list_price) {
        (_, Some(v), _) => money(cur, v),
        (CostCell::Usage, None, Some(p)) => p.to_string(),
        (CostCell::Usage, None, None) => "usage-based".into(),
        _ => String::new(),
    };
    format!("  {}", st.paint(pad_left(&text, l.public)).dim().s())
}

fn component_line(out: &mut String, cur: &str, c: &Component, last: bool, st: &Style, l: &Layout) {
    let prefix = if last { "└─ " } else { "├─ " };
    let name = truncate(&c.name, l.name - 3);
    let x = cells(cur, c);
    let usage = matches!(x.cost, CostCell::Usage);
    let dim_if = |s: String| if usage { st.paint(s).dim().s() } else { s };
    let _ = writeln!(
        out,
        " {}{}  {}  {}  {}  {}{}",
        st.paint(prefix).dim().s(),
        pad_right(&name, l.name - 3),
        dim_if(pad_left(&x.qty, l.qty)),
        dim_if(pad_right(&x.unit, l.unit)),
        dim_if(pad_left(&x.price, l.price)),
        cost_cell(st, cur, &x.cost, l.cost),
        public_cell(st, cur, l, &x.cost, c.list_monthly_cost, x.list_price.as_deref())
    );
    if let Some(n) = c.note.as_ref().filter(|_| st.details) {
        let cont = if last { "   " } else { "│  " };
        for line in wrap(n, l.total().saturating_sub(8)) {
            let _ = writeln!(out, " {}   {}", st.paint(cont).dim().s(), st.paint(line).dim().italic().s());
        }
    }
}

fn resource_block(out: &mut String, cur: &str, r: &CostedResource, st: &Style, l: &Layout) {
    let lines = wrap_address(&r.address, l.header());
    let total = if r.has_unpriced_usage && r.monthly_cost == 0.0 { CostCell::Usage } else { CostCell::Amount(r.monthly_cost) };
    for (i, line) in lines.iter().enumerate() {
        if i + 1 < lines.len() {
            let _ = writeln!(out, " {}", st.paint(line.as_str()).bold().s());
        } else {
            let list = r.list_monthly_cost.filter(|_| !matches!(total, CostCell::Usage));
            let _ = writeln!(
                out,
                " {}  {}{}",
                st.paint(pad_right(line, l.header())).bold().s(),
                cost_cell(st, cur, &total, l.cost),
                public_cell(st, cur, l, &total, list, None)
            );
        }
    }
    if let Some(e) = r.usage_examples.as_ref().filter(|_| st.details || r.usage_profile.is_some()) {
        let label = match &r.usage_profile {
            Some(p) => format!("priced with the {p} usage profile · examples:"),
            None => "≈ with example usage:".to_string(),
        };
        let _ = writeln!(
            out,
            " {} {} {} {} {} {} {}",
            st.paint("   ").s(),
            st.paint(label).dim().italic().s(),
            st.paint(format!("small {}", money(cur, e.small))).dim().s(),
            st.paint("·").dim().s(),
            st.paint(format!("medium {}", money(cur, e.medium))).s(),
            st.paint("·").dim().s(),
            st.paint(format!("large {}", money(cur, e.large))).dim().s()
        );
    }
    let n = r.components.len();
    for (i, c) in r.components.iter().enumerate() {
        component_line(out, cur, c, i + 1 == n, st, l);
    }
    if st.show_warnings {
        for w in &r.warnings {
            for (i, line) in wrap(w, l.total().saturating_sub(6)).iter().enumerate() {
                let mark = if i == 0 { "⚠" } else { " " };
                let _ = writeln!(out, "   {}", st.paint(format!("{mark} {line}")).yellow().s());
            }
        }
    }
    out.push('\n');
}

fn shown<'a>(p: &'a crate::cost::ProjectReport, st: &Style) -> Vec<&'a CostedResource> {
    let mut v: Vec<&CostedResource> = p
        .resources
        .iter()
        .filter(|r| r.status == Status::Priced && (st.show_all || r.monthly_cost > 0.0 || r.has_unpriced_usage || !r.components.is_empty()))
        .collect();
    v.sort_by(|a, b| b.monthly_cost.total_cmp(&a.monthly_cost).then(a.address.cmp(&b.address)));
    v
}

/// Resources to display: identical instances of a counted resource are
/// collapsed into one `addr[*] ×N` entry whose quantities and costs are the
/// sums of the instances (unit prices unchanged).
fn displayed<'a>(p: &'a crate::cost::ProjectReport, st: &Style) -> Vec<std::borrow::Cow<'a, CostedResource>> {
    use std::borrow::Cow;
    let list = shown(p, st);
    if !st.group {
        return list.into_iter().map(Cow::Borrowed).collect();
    }
    let same = |a: &CostedResource, b: &CostedResource| {
        a.rtype == b.rtype
            && a.region == b.region
            && a.components == b.components
            && a.warnings == b.warnings
            && a.usage_examples == b.usage_examples
            && a.has_unpriced_usage == b.has_unpriced_usage
    };
    let mut groups: Vec<(String, Vec<&CostedResource>)> = vec![];
    for r in list {
        let g = group_of(&r.address);
        let counted = g != r.address;
        match groups.iter_mut().find(|(k, v)| counted && *k == g && same(v[0], r)) {
            Some((_, v)) => v.push(r),
            None => groups.push((if counted { g } else { r.address.clone() }, vec![r])),
        }
    }
    let mut out: Vec<Cow<CostedResource>> = groups
        .into_iter()
        .map(|(g, v)| {
            if v.len() < 2 {
                return Cow::Borrowed(v[0]);
            }
            let n = v.len() as f64;
            let mut r = v[0].clone();
            r.address = format!("{g}[*] ×{}", v.len());
            r.monthly_cost = v.iter().map(|x| x.monthly_cost).sum();
            r.list_monthly_cost = r.list_monthly_cost.map(|_| v.iter().map(|x| x.list_monthly_cost.unwrap_or(x.monthly_cost)).sum());
            let scale =
                |e: crate::cost::UsageExamples| crate::cost::UsageExamples { small: e.small * n, medium: e.medium * n, large: e.large * n };
            r.usage_examples = r.usage_examples.map(scale);
            r.list_usage_examples = r.list_usage_examples.map(scale);
            for c in &mut r.components {
                c.quantity = c.quantity.map(|q| q * n);
                c.monthly_cost = c.monthly_cost.map(|q| q * n);
                c.list_monthly_cost = c.list_monthly_cost.map(|q| q * n);
            }
            Cow::Owned(r)
        })
        .collect();
    out.sort_by(|a, b| b.monthly_cost.total_cmp(&a.monthly_cost).then(a.address.cmp(&b.address)));
    out
}

fn project_title(p: &crate::cost::ProjectReport) -> String {
    let folder = || std::path::Path::new(&p.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| ".".into());
    if p.name == "." {
        folder()
    } else if let Some(env) = p.name.strip_prefix(". (") {
        format!("{} ({env}", folder())
    } else {
        p.name.clone()
    }
}

pub fn table(report: &Report, st: &Style) -> String {
    let cur = &report.currency;
    let per_project: Vec<Vec<std::borrow::Cow<CostedResource>>> = report.projects.iter().map(|p| displayed(p, st)).collect();
    let all: Vec<&CostedResource> = per_project.iter().flatten().map(|r| r.as_ref()).collect();
    let contract = report.billing_account.is_some();
    let l = Layout::compute(cur, &all, term_width(), contract);
    let (price_h, cost_h) = if contract { ("Your Price", "Your Cost") } else { ("Unit Price", "Monthly Cost") };
    let mut out = String::new();
    for (p, resources) in report.projects.iter().zip(&per_project) {
        let _ = writeln!(
            out,
            "{} {}",
            st.paint(format!("Project: {}", project_title(p))).bold().cyan().s(),
            st.paint(format!("({})", p.kind)).dim().s()
        );
        out.push('\n');
        let mut header = format!(
            " {}  {}  {}  {}  {}",
            pad_right("Name", l.name),
            pad_left("Monthly Qty", l.qty),
            pad_right("Unit", l.unit),
            pad_left(price_h, l.price),
            pad_left(cost_h, l.cost)
        );
        if contract {
            header.push_str(&format!("  {}", pad_left("Public Cost", l.public)));
        }
        let _ = writeln!(out, "{}", st.paint(header).dim().s());
        let _ = writeln!(out, "{}", st.paint(format!(" {}", "─".repeat(l.total() - 1))).dim().s());
        for r in resources {
            resource_block(&mut out, cur, r, st, &l);
        }
        if resources.is_empty() {
            let free = p.resources.iter().filter(|r| r.status == Status::Free).count();
            let unsupported = p.resources.iter().filter(|r| r.status == Status::Unsupported).count();
            let msg = format!(" No priced resources ({} resources: {free} free, {unsupported} not supported yet)\n", p.resources.len());
            let _ = writeln!(out, "{}", st.paint(msg).dim().s());
        }
        let _ = writeln!(out, "{}", st.paint(format!(" {}", "─".repeat(l.total() - 1))).dim().s());
        let _ = writeln!(
            out,
            " {}  {}{}",
            st.paint(pad_right("Project total", l.header())).bold().s(),
            st.paint(pad_left(&money(cur, p.monthly_cost), l.cost)).bold().green().s(),
            if contract {
                format!("  {}", st.paint(pad_left(&money(cur, p.list_monthly_cost.unwrap_or(p.monthly_cost)), l.public)).bold().s())
            } else {
                String::new()
            }
        );
        if st.show_warnings && !p.warnings.is_empty() {
            out.push('\n');
            for w in p.warnings.iter().take(20) {
                let _ = writeln!(out, " {}", st.paint(format!("⚠ {w}")).yellow().s());
            }
            if p.warnings.len() > 20 {
                let _ = writeln!(out, " {}", st.paint(format!("… {} more warnings", p.warnings.len() - 20)).yellow().s());
            }
        }
        out.push_str("\n\n");
    }
    let has_usage = all.iter().any(|r| r.components.iter().any(|c| c.usage_based && c.monthly_cost.is_none()));
    let legend = match (contract, has_usage) {
        (true, true) => Some(
            "Your Price / Your Cost: your contract prices · Public Cost: Google list price per month, or per unit on \
             usage-based lines · usage-based: depends on usage (costerra usage init, --usage-profile)",
        ),
        (true, false) => Some("Your Price / Your Cost: your contract prices · Public Cost: Google list price per month"),
        (false, true) => Some("usage-based: the cost depends on usage; set estimates with `costerra usage init` or --usage-profile"),
        (false, false) => None,
    };
    if let Some(text) = legend {
        for line in wrap(text, l.total().saturating_sub(2)) {
            let _ = writeln!(out, " {}", st.paint(line).dim().italic().s());
        }
        out.push('\n');
    }
    out.push_str(&summary_with(report, st, l.total(), Some((l.cost, l.public))));
    out
}

pub fn summary(report: &Report, st: &Style) -> String {
    summary_with(report, st, term_width().min(110), None)
}

/// `cols`: (cost, public) column widths of the table above, to align totals.
fn summary_with(report: &Report, st: &Style, width: usize, cols: Option<(usize, usize)>) -> String {
    let cur = &report.currency;
    let mut out = String::new();
    let contract = report.billing_account.is_some();
    let list_of = |p: &crate::cost::ProjectReport| p.list_monthly_cost.unwrap_or(p.monthly_cost);
    let report_list = report.list_monthly_cost.unwrap_or(report.monthly_cost);
    let cost_h = if contract { "Your cost" } else { "Monthly cost" };
    let mut cost_w = report
        .projects
        .iter()
        .map(|p| width_of(&money(cur, p.monthly_cost)))
        .chain([width_of(&money(cur, report.monthly_cost)), cost_h.len()])
        .max()
        .unwrap_or(12);
    let mut public_w = if contract {
        report
            .projects
            .iter()
            .map(|p| width_of(&money(cur, list_of(p))))
            .chain([width_of(&money(cur, report_list)), "Public cost".len()])
            .max()
            .unwrap_or(11)
    } else {
        0
    };
    if let Some((c, p)) = cols {
        cost_w = cost_w.max(c);
        if contract {
            public_w = public_w.max(p);
        }
    }
    let tail_w = cost_w + if contract { public_w + 2 } else { 0 };
    let name_w = width.saturating_sub(tail_w + 3);
    let public = |v: f64, bold: bool| -> String {
        if !contract {
            return String::new();
        }
        let p = st.paint(pad_left(&money(cur, v), public_w));
        format!("  {}", if bold { p.bold().s() } else { p.dim().s() })
    };
    let heavy = st.paint("━".repeat(width)).dim().s();
    let _ = writeln!(out, "{heavy}");
    if report.projects.len() > 1 {
        let mut h = format!(" {}  {}", pad_right("Project", name_w), pad_left(cost_h, cost_w));
        if contract {
            h.push_str(&format!("  {}", pad_left("Public cost", public_w)));
        }
        let _ = writeln!(out, "{}", st.paint(h).dim().s());
        let mut ps: Vec<_> = report.projects.iter().collect();
        ps.sort_by(|a, b| b.monthly_cost.total_cmp(&a.monthly_cost));
        for p in ps {
            let _ = writeln!(
                out,
                " {}  {}{}",
                pad_right(&truncate(&project_title(p), name_w), name_w),
                st.cost(pad_left(&money(cur, p.monthly_cost), cost_w), p.monthly_cost),
                public(list_of(p), false)
            );
        }
        let _ = writeln!(out, "{}", st.paint("─".repeat(width)).dim().s());
    }
    let _ = writeln!(
        out,
        " {}  {}{}",
        st.paint(pad_right("OVERALL TOTAL (monthly)", name_w)).bold().s(),
        st.paint(pad_left(&money(cur, report.monthly_cost), cost_w)).bold().green().s(),
        public(report_list, true)
    );
    let _ = writeln!(out, "{heavy}");

    if let Some(e) = &report.usage_examples {
        match &report.list_usage_examples {
            Some(l) => {
                let w = [e.small, e.medium, e.large, l.small, l.medium, l.large]
                    .iter()
                    .map(|v| width_of(&money(cur, *v)))
                    .max()
                    .unwrap_or(8)
                    .max(8);
                let _ = writeln!(
                    out,
                    " {} {}  {}  {}  {}",
                    st.paint(pad_right("With usage", 25)).dim().s(),
                    st.paint(pad_left("small", w)).dim().s(),
                    st.paint(pad_left("medium", w)).dim().s(),
                    st.paint(pad_left("large", w)).dim().s(),
                    st.paint("(example usage, see `costerra usage profiles`)").dim().s()
                );
                for (label, x, bold) in [("  with your contract", e, true), ("  public list prices", l, false)] {
                    let cell = |v: f64| {
                        let t = pad_left(&money(cur, v), w);
                        if bold { st.cost(t, v) } else { st.paint(t).dim().s() }
                    };
                    let _ = writeln!(out, " {} {}  {}  {}", pad_right(label, 25), cell(x.small), cell(x.medium), cell(x.large));
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    " {} small {} · medium {} · large {}  {}",
                    st.paint("With usage ").dim().s(),
                    st.cost(money(cur, e.small), e.small),
                    st.cost(money(cur, e.medium), e.medium),
                    st.cost(money(cur, e.large), e.large),
                    st.paint("(example usage, see `costerra usage profiles`)").dim().s()
                );
            }
        }
    }
    match (&report.billing_account, report.list_monthly_cost) {
        (Some(ba), Some(list)) => {
            let saved = list - report.monthly_cost;
            let pct = if list > 0.0 { saved / list * 100.0 } else { 0.0 };
            let _ = writeln!(
                out,
                " {} contract prices of billing account {} · {} vs list price {}",
                st.paint("Pricing    ").dim().s(),
                st.paint(ba.as_str()).cyan().s(),
                st.paint(format!("saves {}/month ({pct:.1}%)", money(cur, saved))).green().bold().s(),
                st.paint(format!("({})", money(cur, list))).dim().s()
            );
        }
        _ => {
            let hint = if st.details {
                format!(" {}", st.paint("(use --billing-account or `billing_account:` in costerra.yml for your contract prices)").dim().s())
            } else {
                String::new()
            };
            let _ = writeln!(out, " {} public list prices{hint}", st.paint("Pricing    ").dim().s());
        }
    }
    if let Some(o) = &report.observed {
        let _ = writeln!(
            out,
            " {} observed for {} resource{} over the last {} days ({})",
            st.paint("Usage      ").dim().s(),
            st.paint(o.resources.to_string()).cyan().s(),
            if o.resources == 1 { "" } else { "s" },
            o.window_days,
            o.sources.join(", ")
        );
    }
    let s = &report.summary;
    let unsupported = format!("{} not supported", s.unsupported);
    let _ = writeln!(
        out,
        " {} {} priced · {} free · {}",
        st.paint("Resources  ").dim().s(),
        st.paint(s.priced.to_string()).green().bold().s(),
        s.free,
        if s.unsupported > 0 { st.paint(unsupported).yellow().s() } else { unsupported }
    );
    if !s.unsupported_types.is_empty() {
        let list: Vec<String> = s.unsupported_types.iter().take(8).map(|(t, n)| format!("{t} ×{n}")).collect();
        let text = format!("{}{}", list.join(", "), if s.unsupported_types.len() > 8 { ", …" } else { "" });
        for (i, line) in wrap(&text, width.saturating_sub(14)).iter().enumerate() {
            let label = if i == 0 { "Unsupported" } else { "           " };
            let label = label.to_string();
            let _ = writeln!(out, " {} {}", st.paint(label).yellow().s(), line);
        }
    }
    if let Some(p) = &report.usage_profile {
        let _ = writeln!(
            out,
            " {} usage without estimates is priced with the {} example profile",
            st.paint("Usage      ").dim().s(),
            st.paint(p.as_str()).yellow().bold().s()
        );
    } else if s.usage_based_without_usage > 0 {
        let _ = writeln!(
            out,
            " {} {} resource{} without usage estimates → run {}",
            st.paint("Usage      ").dim().s(),
            st.paint(s.usage_based_without_usage.to_string()).yellow().bold().s(),
            if s.usage_based_without_usage == 1 { "" } else { "s" },
            if st.color { st.paint("costerra usage init").cyan().s() } else { "`costerra usage init`".to_string() }
        );
    }

    if !report.free_tiers.is_empty() {
        let how = if report.free_tier_included {
            "included in these estimates where the price list encodes them"
        } else {
            "per billing account / project, not deducted from these estimates (--include-free-tier applies them)"
        };
        let _ = writeln!(out, "\n{} {}", st.paint("🎁 Free tiers").bold().s(), st.paint(format!("— {how}")).dim().s());
        let name_w = report.free_tiers.iter().map(|f| width_of(&f.service)).max().unwrap_or(10);
        let text_w = width.saturating_sub(name_w + 6).max(40);
        for f in &report.free_tiers {
            for (i, line) in wrap(&f.allowance, text_w).iter().enumerate() {
                let name = if i == 0 { f.service.as_str() } else { "" };
                let _ = writeln!(out, "   {}  {}", st.paint(pad_right(name, name_w)).cyan().s(), line);
            }
        }
    }

    let multi = report.projects.len() > 1;
    let recs: Vec<(&str, &Recommendation)> =
        report.projects.iter().flat_map(|p| p.recommendations.iter().map(move |r| (p.name.as_str(), r))).collect();
    if !recs.is_empty() {
        out.push('\n');
        let mut h = st.paint(format!("💡 Recommendations ({})", recs.len())).bold().s();
        if report.potential_savings > 0.0 {
            h = format!(
                "{h} {} {}",
                st.paint("— potential savings").bold().s(),
                st.paint(format!("{}/month", money(cur, report.potential_savings))).bold().green().s()
            );
            // On its own line when the header would not fit.
            let plain = format!(
                "💡 Recommendations ({}) — potential savings {}/month {SAVINGS_NOTE}",
                recs.len(),
                money(cur, report.potential_savings)
            );
            let sep = if width_of(&plain) + 1 > width.max(term_width().min(110)) { "\n " } else { " " };
            h = format!("{h}{sep}{}", st.paint(SAVINGS_NOTE).dim().s());
        }
        let _ = writeln!(out, "{h}\n");
        let mut sorted = recs.clone();
        sorted.sort_by(|(_, a), (_, b)| {
            b.severity.cmp(&a.severity).then(b.monthly_savings.unwrap_or(0.0).total_cmp(&a.monthly_savings.unwrap_or(0.0)))
        });
        let indent = "          ";
        let width = width.max(term_width().min(110));
        for (project, r) in sorted {
            let label = pad_right(&format!("[{}]", sev_label(r.severity)), 8);
            let sev = match r.severity {
                Severity::High => st.paint(label).red().bold().s(),
                Severity::Medium => st.paint(label).yellow().bold().s(),
                Severity::Low => st.paint(label).blue().bold().s(),
                Severity::Info => st.paint(label).dim().s(),
            };
            rec_title(&mut out, st, &sev, r, cur, width, indent);
            let addr = if multi { format!("{project}: {}", r.address) } else { r.address.clone() };
            for line in wrap_address(&addr, width.saturating_sub(indent.len())) {
                let _ = writeln!(out, "{indent}{}", st.paint(line).cyan().s());
            }
            for line in wrap(&r.detail, width.saturating_sub(indent.len())) {
                let _ = writeln!(out, "{indent}{}", st.paint(line).dim().s());
            }
            source_lines(&mut out, st, r, indent, width);
            out.push('\n');
        }
    }

    if !report.external_recommendations.is_empty() {
        let total: f64 = report.external_recommendations.iter().filter_map(|r| r.monthly_savings).sum();
        let mut h =
            st.paint(format!("☁️  From your cloud usage ({}), not tied to this code", report.external_recommendations.len())).bold().s();
        if total > 0.0 {
            h = format!("{h} {}", st.paint(format!("— potential savings {}/month", money(cur, total))).green().bold().s());
        }
        let _ = writeln!(out, "\n{h}\n");
        let indent = "          ";
        for r in report.external_recommendations.iter().take(15) {
            let label = pad_right(&format!("[{}]", sev_label(r.severity)), 8);
            rec_title(&mut out, st, &st.paint(label).dim().s(), r, cur, width, indent);
            if !r.address.is_empty() {
                let _ = writeln!(out, "{indent}{}", st.paint(r.address.as_str()).cyan().s());
            }
            for line in wrap(&r.detail, width.saturating_sub(indent.len())) {
                let _ = writeln!(out, "{indent}{}", st.paint(line).dim().s());
            }
            source_lines(&mut out, st, r, indent, width);
            out.push('\n');
        }
        if report.external_recommendations.len() > 15 {
            let _ = writeln!(
                out,
                "{indent}{}",
                st.paint(format!("… {} more (see --format json)", report.external_recommendations.len() - 15)).dim().s()
            );
        }
    }
    if st.details {
        for n in &report.insight_notes {
            let _ = writeln!(out, "\n {}", st.paint(format!("ℹ {n}")).dim().s());
        }
    }
    out
}

/// `saves ~$X/mo`, or `costs ~$X/mo more` for a change that costs more
/// (upsizing a saturated resource), and whether it costs more.
fn savings_text(cur: &str, s: f64, approx: &str) -> (String, bool) {
    if s < 0.0 {
        (format!("costs {approx}{}/mo more", money(cur, -s)), true)
    } else {
        (format!("saves {approx}{}/mo", money(cur, s)), false)
    }
}

fn paint_savings(st: &Style, cur: &str, s: f64, approx: &str) -> String {
    match savings_text(cur, s, approx) {
        (t, true) => st.paint(t).yellow().bold().s(),
        (t, false) => st.paint(t).green().bold().s(),
    }
}

/// ` [SEV]    source Title  saves ~$X/mo` on one line when it fits; otherwise
/// the title wraps under itself and the savings get their own line.
fn rec_title(out: &mut String, st: &Style, sev: &str, r: &Recommendation, cur: &str, width: usize, indent: &str) {
    let source = if r.source == "gcp" { "Google" } else { "costerra" };
    let savings = r.monthly_savings.map(|s| savings_text(cur, s, "~").0);
    let head = 1 + 8 + 1 + source.len() + 1;
    let fits = head + width_of(&r.title) + savings.as_ref().map_or(0, |s| 2 + width_of(s)) <= width;
    let painted = r.monthly_savings.map(|s| paint_savings(st, cur, s, "~"));
    if fits {
        let savings = painted.map(|s| format!("  {s}")).unwrap_or_default();
        let _ = writeln!(out, " {sev} {}{}{savings}", source_tag(st, &r.source), st.paint(r.title.as_str()).bold().s());
        return;
    }
    let lines = wrap(&r.title, width.saturating_sub(head).max(20));
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            let _ = writeln!(out, " {sev} {}{}", source_tag(st, &r.source), st.paint(line.as_str()).bold().s());
        } else {
            let _ = writeln!(out, "{}{}", " ".repeat(head), st.paint(line.as_str()).bold().s());
        }
    }
    if let Some(s) = painted {
        let _ = writeln!(out, "{indent}{s}");
    }
}

fn source_tag(st: &Style, source: &str) -> String {
    match source {
        "gcp" => format!("{} ", st.paint("Google").blue().bold().s()),
        _ => format!("{} ", st.paint("costerra").cyan().s()),
    }
}

/// Human description of a recommendation's origin, and an optional command /
/// link to inspect it at the source.
pub fn source_description(r: &Recommendation) -> (String, Option<String>) {
    let (reference, details) =
        match r.source_ref.as_deref().map(|s| s.split_once(" · details: ").map(|(a, b)| (a, Some(b))).unwrap_or((s, None))) {
            Some((a, b)) => (Some(a), b),
            None => (None, None),
        };
    let text = match r.source.as_str() {
        "gcp" => format!("Google Cloud Recommender{}, based on observed usage", reference.map(|x| format!(" ({x})")).unwrap_or_default()),
        _ => format!("costerra{}, {}", reference.map(|x| format!(" {x}")).unwrap_or_default(), r.basis.describe()),
    };
    (text, details.map(str::to_string))
}

fn source_lines(out: &mut String, st: &Style, r: &Recommendation, indent: &str, width: usize) {
    if !st.details {
        return;
    }
    let (text, details) = source_description(r);
    for (i, line) in wrap(&text, width.saturating_sub(indent.len() + 8)).iter().enumerate() {
        let label = if i == 0 { "Source: " } else { "        " };
        let _ = writeln!(out, "{indent}{}{}", st.paint(label).dim().italic().s(), st.paint(line.as_str()).dim().italic().s());
    }
    if let Some(d) = details {
        // Not wrapped: it is a command to copy.
        let _ = writeln!(out, "{indent}{}{}", st.paint("Inspect: ").dim().italic().s(), st.paint(d).dim().s());
    }
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut lines = vec![];
    let mut cur = String::new();
    for w in s.split_whitespace() {
        if width_of(&cur) + width_of(w) + 1 > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// A savings cell: the amount, or `+$X (costs more)` for upsizing.
fn md_savings(cur: &str, s: f64) -> String {
    if s < 0.0 { format!("+{} (costs more)", money(cur, -s)) } else { money(cur, s) }
}

/// The Markdown report (PR comments), with the rightsizing section when a
/// plan is given.
pub fn markdown(
    report: &Report,
    diff: Option<&DiffReport>,
    plan: Option<&crate::rightsize::Plan>,
    billing: Option<&crate::billing::BillingHistory>,
) -> String {
    let cur = &report.currency;
    // Projects are named as in the terminal table (`terraform (prod)`, not `. (prod)`).
    let title = |name: &str| project_label(report, name);
    let mut out = String::new();
    // Hidden marker: CI finds (and updates) its previous comment with it.
    let _ = writeln!(out, "{COMMENT_MARKER}\n### 💰 costerra cost estimate\n");
    match diff {
        Some(d) => {
            let pct = d.diff_percent().map(|p| format!(" ({p:+.1}%)")).unwrap_or_default();
            let arrow = if d.diff > 0.005 {
                "📈"
            } else if d.diff < -0.005 {
                "📉"
            } else {
                "➖"
            };
            let _ = writeln!(
                out,
                "{arrow} Monthly cost will change by **{}**{pct}: {} → **{}**\n",
                signed(cur, d.diff),
                money(cur, d.past),
                money(cur, d.new)
            );
            let _ = writeln!(out, "| Project | Previous | New | Diff |\n|---|---:|---:|---:|");
            for p in d.projects.iter().filter(|p| p.diff.abs() >= 0.005 || !p.resources.is_empty()) {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} |",
                    md_escape(&title(&p.name)),
                    money(cur, p.past),
                    money(cur, p.new),
                    signed(cur, p.diff)
                );
            }
            let changed: Vec<_> = d.projects.iter().flat_map(|p| p.resources.iter().map(move |r| (p, r))).collect();
            if !changed.is_empty() {
                let _ = writeln!(out, "\n<details><summary>Changed resources ({})</summary>\n", changed.len());
                let _ = writeln!(out, "| Project | Resource | Change | Previous | New | Diff |\n|---|---|---|---:|---:|---:|");
                for (p, r) in changed.iter().take(200) {
                    let ch = match r.change {
                        Change::Added => "➕ added",
                        Change::Removed => "➖ removed",
                        Change::Changed => "✏️ changed",
                    };
                    let _ = writeln!(
                        out,
                        "| {} | `{}` | {ch} | {} | {} | {} |",
                        md_escape(&title(&p.name)),
                        md_escape(&r.address),
                        money(cur, r.past),
                        money(cur, r.new),
                        signed(cur, r.diff)
                    );
                }
                let _ = writeln!(out, "\n</details>");
            }
        }
        None => {
            let _ = writeln!(out, "**Total monthly cost: {}**\n", money(cur, report.monthly_cost));
            if let Some(e) = &report.usage_examples {
                let _ = writeln!(
                    out,
                    "With example usage for usage-based resources: small **{}** · medium **{}** · large **{}**{}\n",
                    money(cur, e.small),
                    money(cur, e.medium),
                    money(cur, e.large),
                    report
                        .list_usage_examples
                        .as_ref()
                        .map(|l| format!(
                            " (public list prices: {} · {} · {})",
                            money(cur, l.small),
                            money(cur, l.medium),
                            money(cur, l.large)
                        ))
                        .unwrap_or_default()
                );
            }
            if report.billing_account.is_some() {
                let _ = writeln!(out, "| Project | Your cost | Public cost |\n|---|---:|---:|");
                for p in &report.projects {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} |",
                        md_escape(&project_title(p)),
                        money(cur, p.monthly_cost),
                        money(cur, p.list_monthly_cost.unwrap_or(p.monthly_cost))
                    );
                }
            } else {
                let _ = writeln!(out, "| Project | Monthly cost |\n|---|---:|");
                for p in &report.projects {
                    let _ = writeln!(out, "| {} | {} |", md_escape(&project_title(p)), money(cur, p.monthly_cost));
                }
            }
        }
    }

    // Top resources
    let mut all: Vec<(&str, &CostedResource)> = report
        .projects
        .iter()
        .flat_map(|p| p.resources.iter().map(move |r| (p.name.as_str(), r)))
        .filter(|(_, r)| r.monthly_cost > 0.0)
        .collect();
    all.sort_by(|a, b| b.1.monthly_cost.total_cmp(&a.1.monthly_cost));
    if !all.is_empty() {
        let _ = writeln!(out, "\n<details><summary>Top resources</summary>\n");
        let _ = writeln!(out, "| Project | Resource | Monthly cost |\n|---|---|---:|");
        for (p, r) in all.iter().take(25) {
            let _ = writeln!(out, "| {} | `{}` | {} |", md_escape(&title(p)), md_escape(&r.address), money(cur, r.monthly_cost));
        }
        let _ = writeln!(out, "\n</details>");
    }

    let all_recs: Vec<(&str, &Recommendation)> =
        report.projects.iter().flat_map(|p| p.recommendations.iter().map(move |r| (p.name.as_str(), r))).collect();
    // In diffs, focus on recommendations for resources touched by the change.
    let recs: Vec<(&str, &Recommendation)> = match diff {
        Some(d) => {
            let touched: std::collections::HashSet<(String, String)> = d
                .projects
                .iter()
                .flat_map(|p| {
                    p.resources.iter().filter(|r| r.change != Change::Removed).map(move |r| (p.name.clone(), group_of(&r.address)))
                })
                .collect();
            all_recs.iter().filter(|(p, r)| touched.contains(&(p.to_string(), group_of(&r.address)))).cloned().collect()
        }
        None => all_recs.clone(),
    };
    if !recs.is_empty() {
        let savings = crate::cost::potential_savings(recs.iter().copied());
        let _ = writeln!(out, "\n#### 💡 Recommendations — potential savings {}/month\n", money(cur, savings));
        let _ = writeln!(out, "<sub>{SAVINGS_NOTE}</sub>\n");
        let _ = writeln!(out, "| | Resource | Recommendation | Source | Savings/mo |\n|---|---|---|---|---:|");
        let mut sorted = recs.clone();
        sorted.sort_by(|a, b| {
            b.1.severity.cmp(&a.1.severity).then(b.1.monthly_savings.unwrap_or(0.0).total_cmp(&a.1.monthly_savings.unwrap_or(0.0)))
        });
        for (p, r) in sorted.iter().take(50) {
            let icon = match r.severity {
                Severity::High => "🔴",
                Severity::Medium => "🟠",
                Severity::Low => "🔵",
                Severity::Info => "⚪",
            };
            let (src, inspect) = source_description(r);
            let _ = writeln!(
                out,
                "| {icon} | `{}`<br><sub>{}</sub> | **{}**<br><sub>{}</sub> | <sub>{}{}</sub> | {} |",
                md_escape(&r.address),
                md_escape(&title(p)),
                md_escape(&r.title),
                md_escape(&r.detail),
                md_escape(&src),
                inspect.map(|c| format!("<br>`{}`", md_escape(&c))).unwrap_or_default(),
                r.monthly_savings.map(|s| md_savings(cur, s)).unwrap_or_else(|| "–".into())
            );
        }
    }

    if !report.external_recommendations.is_empty() {
        let _ = writeln!(
            out,
            "\n<details><summary>☁️ Recommendations from your cloud usage, not tied to this code ({})</summary>\n",
            report.external_recommendations.len()
        );
        let _ = writeln!(out, "| Recommendation | Scope | Source | Savings/mo |\n|---|---|---|---:|");
        for r in report.external_recommendations.iter().take(30) {
            let (src, inspect) = source_description(r);
            let _ = writeln!(
                out,
                "| **{}**<br><sub>{}</sub> | `{}` | <sub>{}{}</sub> | {} |",
                md_escape(&r.title),
                md_escape(&r.detail),
                md_escape(&r.address),
                md_escape(&src),
                inspect.map(|c| format!("<br>`{}`", md_escape(&c))).unwrap_or_default(),
                r.monthly_savings.map(|s| md_savings(cur, s)).unwrap_or_else(|| "–".into())
            );
        }
        let _ = writeln!(out, "\n</details>");
    }
    if !report.free_tiers.is_empty() {
        let _ = writeln!(out, "\n<details><summary>🎁 Free tiers of the services used ({})</summary>\n", report.free_tiers.len());
        let _ = writeln!(
            out,
            "Free tiers apply per billing account or project and are {}.\n",
            if report.free_tier_included { "included where the price list encodes them" } else { "not deducted from these estimates" }
        );
        let _ = writeln!(out, "| Service | Free tier |\n|---|---|");
        for f in &report.free_tiers {
            let _ = writeln!(out, "| [{}]({}) | {} |", f.service, f.url, md_escape(&f.allowance));
        }
        let _ = writeln!(out, "\n</details>");
    }
    if all_recs.len() > recs.len() {
        let _ = writeln!(
            out,
            "\n<sub>{} more recommendations for unchanged resources: run `costerra breakdown` to see them.</sub>",
            all_recs.len() - recs.len()
        );
    }
    if let Some(p) = plan {
        out.push_str(&rightsizing_markdown(p, report));
    }
    if let Some(h) = billing {
        out.push_str(&crate::billing::markdown(h, billing_estimate(report, h)));
    }
    let pricing = match (&report.billing_account, report.list_monthly_cost) {
        (Some(ba), Some(list)) => format!(
            "Contract prices of billing account {ba} (saves {}/month vs list price {}).",
            money(cur, list - report.monthly_cost),
            money(cur, list)
        ),
        _ => "Public list prices.".to_string(),
    };
    let _ = writeln!(out, "\n<sub>{pricing}</sub>");
    if let Some(o) = &report.observed {
        let _ = writeln!(
            out,
            "<sub>Usage observed for {} resources over the last {} days ({}).</sub>",
            o.resources,
            o.window_days,
            o.sources.join(", ")
        );
    }
    let s = &report.summary;
    let _ = writeln!(
        out,
        "<sub>{} resources priced, {} free, {} unsupported{}. Generated by costerra {}.</sub>",
        s.priced,
        s.free,
        s.unsupported,
        if s.usage_based_without_usage > 0 {
            format!(", {} with usage-based costs not estimated", s.usage_based_without_usage)
        } else {
            String::new()
        },
        report.version
    );
    out
}

// ---------------------------------------------------------------------------
// Diff (terminal)
// ---------------------------------------------------------------------------

/// The monthly estimate to compare with the billing, when in the same currency.
pub fn billing_estimate(report: &Report, h: &crate::billing::BillingHistory) -> Option<f64> {
    (report.currency == h.currency).then_some(report.monthly_cost)
}

pub fn diff_table(d: &DiffReport, st: &Style) -> String {
    let cur = &d.currency;
    let mut out = String::new();
    for p in &d.projects {
        if p.resources.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}\n", st.paint(format!("Project: {}", p.name)).bold().cyan().s());
        for r in &p.resources {
            let line = match r.change {
                Change::Added => st.paint(format!("+ {}", r.address)).green().bold().s(),
                Change::Removed => st.paint(format!("- {}", r.address)).red().bold().s(),
                Change::Changed => st.paint(format!("~ {}", r.address)).yellow().bold().s(),
            };
            let _ = writeln!(out, " {line}");
            let _ = writeln!(
                out,
                "     {} → {}  ({})",
                st.paint(money(cur, r.past)).dim().s(),
                money(cur, r.new),
                st.delta(signed(cur, r.diff), r.diff)
            );
        }
        let _ = writeln!(
            out,
            "\n Monthly cost change for {}: {} ({} → {})\n",
            st.paint(p.name.as_str()).bold().s(),
            st.delta(signed(cur, p.diff), p.diff),
            money(cur, p.past),
            money(cur, p.new)
        );
    }
    let pct = d.diff_percent().map(|p| format!(" ({p:+.1}%)")).unwrap_or_default();
    let _ = writeln!(
        out,
        "{} {}, {}, {}\n",
        st.paint("Key:").dim().s(),
        st.paint("+ added").green().s(),
        st.paint("- removed").red().s(),
        st.paint("~ changed").yellow().s()
    );
    let _ = writeln!(
        out,
        "{} {}  ({} → {})",
        st.paint("Monthly cost change:").bold().s(),
        st.delta(format!("{}{pct}", signed(cur, d.diff)), d.diff),
        money(cur, d.past),
        st.paint(money(cur, d.new)).bold().s()
    );
    out
}

/// Address without instance key and grouping suffix: `a.b[0]`, `a.b[*] (3 instances)` -> `a.b`.
/// First line of the Markdown report, used by CI to find its previous MR comment.
pub const COMMENT_MARKER: &str = "<!-- costerra-cost-comment -->";

/// How the potential savings total is computed.
const SAVINGS_NOTE: &str = "(best option per resource: alternatives are not added up)";

fn group_of(addr: &str) -> String {
    let a = addr.split(" (").next().unwrap_or(addr);
    match a.rfind('[') {
        Some(i) if a.ends_with(']') && !a[i..].contains('.') => a[..i].to_string(),
        _ => a.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Rightsizing (`--rightsize`)
// ---------------------------------------------------------------------------

/// A path relative to the current directory when it is below it.
pub fn display_path(p: &std::path::Path) -> String {
    let canon = |x: &std::path::Path| x.canonicalize().unwrap_or_else(|_| x.to_path_buf());
    match std::env::current_dir().map(|c| canon(&c)) {
        Ok(cwd) => canon(p).strip_prefix(&cwd).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string()),
        Err(_) => p.display().to_string(),
    }
}

/// A project as named in the table (`terraform (dev)` rather than `. (dev)`).
fn project_label(report: &Report, name: &str) -> String {
    report.projects.iter().find(|p| p.name == name).map(project_title).unwrap_or_else(|| name.to_string())
}

pub fn rightsizing(plan: &crate::rightsize::Plan, report: &Report, st: &Style, notes: &[String]) -> String {
    let cur = report.currency.as_str();
    let mut out = String::new();
    let n = plan.suggestions.len();
    let _ = writeln!(out, "\n{}", st.paint(format!("📐 Rightsizing from observed usage ({n})")).bold().s());
    if n == 0 {
        let _ = writeln!(out, "\n {}", st.paint("No change needed: the observed resources are neither oversized nor saturated.").dim().s());
        // With --details the notes are already under the table.
        for note in notes.iter().filter(|_| !st.details) {
            let _ = writeln!(out, " {}", st.paint(format!("ℹ {note}")).dim().s());
        }
        return out;
    }
    let width = term_width().min(110);
    let indent = "    ";
    let pad = "            ";
    let label = |t: &str| st.paint(format!("{indent}{t:<8}")).dim().s();
    for s in &plan.suggestions {
        let _ = writeln!(out, "\n {}", st.paint(format!("{}: {}", project_label(report, &s.project), s.address)).cyan().bold().s());
        let _ = writeln!(
            out,
            "{indent}{}  {} → {}",
            st.paint(crate::rightsize::display_attribute(&s.change.attribute)).bold().s(),
            st.paint(s.change.from.as_str()).red().s(),
            st.paint(s.change.to.as_str()).green().s()
        );
        for (i, line) in wrap(&s.detail, width.saturating_sub(pad.len())).iter().enumerate() {
            let _ = writeln!(out, "{}{line}", if i == 0 { label("Why") } else { pad.to_string() });
        }
        match s.locations.as_slice() {
            [] => {}
            [l] => {
                let entry = l.entry.as_ref().map(|e| format!("  (the \"{e}\" entry)")).unwrap_or_default();
                let _ = writeln!(out, "{}{}{}", label("Where"), display_path(&l.file) + &format!(":{}", l.line), st.paint(entry).dim().s());
                if l.insert {
                    let _ = writeln!(out, "{pad}{}", st.paint(format!("  {}", l.text)).dim().s());
                    let _ = writeln!(out, "{pad}{}", st.paint(format!("+   {}", crate::rightsize::inserted(&s.change))).green().s());
                } else if let Some(new) = l.changed(&s.change) {
                    let _ = writeln!(out, "{pad}{}", st.paint(format!("- {}", l.text)).red().s());
                    let _ = writeln!(out, "{pad}{}", st.paint(format!("+ {new}")).green().s());
                } else {
                    let _ = writeln!(out, "{pad}{}", st.paint(format!("  {}", l.text)).dim().s());
                }
            }
            many => {
                for (i, l) in many.iter().enumerate() {
                    let _ = writeln!(
                        out,
                        "{}{}  {}",
                        if i == 0 { label("Where") } else { pad.to_string() },
                        display_path(&l.file) + &format!(":{}", l.line),
                        st.paint(l.text.as_str()).dim().s()
                    );
                }
            }
        }
        if let Some(note) = &s.note {
            for (i, line) in wrap(note, width.saturating_sub(pad.len())).iter().enumerate() {
                let _ = writeln!(out, "{}{}", if i == 0 { label("Note") } else { pad.to_string() }, st.paint(line.as_str()).yellow().s());
            }
        }
        if let Some(url) = &s.console_url {
            // Not wrapped: a link to open.
            let _ = writeln!(out, "{}{}", label("Check"), st.paint(url.as_str()).blue().s());
        }
        if s.cost_known {
            let _ = writeln!(
                out,
                "{}{} → {}  {}",
                label("Cost"),
                money(cur, s.monthly_cost),
                st.paint(format!("{}/month", money(cur, s.monthly_cost_after))).bold().s(),
                paint_savings(st, cur, s.savings(), "")
            );
        } else {
            let _ = writeln!(
                out,
                "{}{}",
                label("Cost"),
                st.paint("change not estimated (usage-based: set usage estimates with `costerra usage init`)").dim().s()
            );
        }
    }
    out.push('\n');
    let saved = plan.monthly_cost - plan.monthly_cost_after;
    let unknown = plan.suggestions.iter().filter(|s| !s.cost_known).count();
    let delta = if saved.abs() < 0.005 {
        String::new()
    } else if saved < 0.0 {
        format!("(+{}/month)", money(cur, -saved))
    } else {
        format!("(−{}/month)", money(cur, saved))
    };
    let delta = match unknown {
        0 => delta,
        n => format!("{delta}{}{n} change{} not estimated", if delta.is_empty() { "" } else { " · " }, if n == 1 { "" } else { "s" }),
    };
    let plain =
        format!(" Total after rightsizing {} → {}/month  {delta}", money(cur, plan.monthly_cost), money(cur, plan.monthly_cost_after));
    let _ = writeln!(
        out,
        " {} {} → {}{}{}",
        st.paint("Total after rightsizing").bold().s(),
        money(cur, plan.monthly_cost),
        if saved < 0.0 {
            st.paint(format!("{}/month", money(cur, plan.monthly_cost_after))).bold().yellow().s()
        } else {
            st.paint(format!("{}/month", money(cur, plan.monthly_cost_after))).bold().green().s()
        },
        if width_of(&plain) > width { "\n " } else { "  " },
        if saved < 0.0 { st.paint(delta).yellow().s() } else { st.paint(delta).green().s() }
    );
    let _ = writeln!(
        out,
        " {}",
        st.paint("costerra never edits the code: check the metrics in the console, then change the lines above.").dim().s()
    );
    out
}

pub fn rightsizing_markdown(plan: &crate::rightsize::Plan, report: &Report) -> String {
    let cur = report.currency.as_str();
    let mut out = String::new();
    let _ = writeln!(out, "\n#### 📐 Rightsizing from observed usage ({})\n", plan.suggestions.len());
    if plan.suggestions.is_empty() {
        out.push_str("No change needed: the observed resources are neither oversized nor saturated.\n");
        return out;
    }
    out.push_str("| Resource | Change | Why | Where | Check | Monthly cost |\n|---|---|---|---|---|---:|\n");
    for s in &plan.suggestions {
        let at = match s.locations.as_slice() {
            [] => "–".to_string(),
            ls => ls.iter().map(|l| format!("`{}:{}`", display_path(&l.file), l.line)).collect::<Vec<_>>().join("<br>"),
        };
        let why = match &s.note {
            Some(n) => format!("{}<br><sub>⚠ {}</sub>", md_escape(&s.detail), md_escape(n)),
            None => md_escape(&s.detail),
        };
        let check = s.console_url.as_ref().map(|u| format!("[console]({u})")).unwrap_or_else(|| "–".into());
        let _ = writeln!(
            out,
            "| {}: `{}` | `{}` {} → {} | {why} | {at} | {check} | {} → **{}** |",
            md_escape(&project_label(report, &s.project)),
            md_escape(&s.address),
            crate::rightsize::display_attribute(&s.change.attribute),
            s.change.from,
            s.change.to,
            money(cur, s.monthly_cost),
            if s.cost_known { money(cur, s.monthly_cost_after) } else { "not estimated".into() }
        );
    }
    let _ =
        writeln!(out, "\nTotal after rightsizing: {} → **{}/month**", money(cur, plan.monthly_cost), money(cur, plan.monthly_cost_after));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_that_round_to_zero_have_no_sign() {
        assert_eq!(money("USD", -1.77e-15), "$0.00");
        assert_eq!(money("USD", -0.004), "$0.00");
        assert_eq!(money("USD", -0.005), "-$0.01");
        assert_eq!(money("USD", -1234.5), "-$1,234.50");
        assert_eq!(money("CHF", -1e-12), "0.00 CHF");
    }
}
