//! `costerra completions`: shell completion scripts.

use super::args::{Cli, Shell};
use anyhow::Result;
use clap::CommandFactory;
use serde_json::{Map, Value, json};

pub fn print(shell: Shell) -> Result<i32> {
    let mut cmd = Cli::command();
    let mut out = std::io::stdout();
    let name = "costerra";
    match shell {
        Shell::Bash => clap_complete::generate(clap_complete::Shell::Bash, &mut cmd, name, &mut out),
        Shell::Zsh => clap_complete::generate(clap_complete::Shell::Zsh, &mut cmd, name, &mut out),
        Shell::Fish => clap_complete::generate(clap_complete::Shell::Fish, &mut cmd, name, &mut out),
        Shell::Elvish => clap_complete::generate(clap_complete::Shell::Elvish, &mut cmd, name, &mut out),
        Shell::Powershell => clap_complete::generate(clap_complete::Shell::PowerShell, &mut cmd, name, &mut out),
        Shell::Nu => clap_complete::generate(clap_complete_nushell::Nushell, &mut cmd, name, &mut out),
        Shell::Xonsh => {
            cmd.build();
            print!("{}", xonsh(&cmd));
        }
    }
    Ok(0)
}

fn help(s: Option<&clap::builder::StyledStr>) -> String {
    s.map(|h| h.to_string().lines().next().unwrap_or_default().to_string()).unwrap_or_default()
}

fn values(a: &clap::Arg) -> Vec<Value> {
    a.get_possible_values().iter().filter(|v| !v.is_hide_set()).map(|v| json!([v.get_name(), help(v.get_help())])).collect()
}

/// The command tree read by the xonsh completer: subcommands, options, the
/// values of options and positionals, and the options that take a free value
/// (a path: left to the other completers).
fn tree(cmd: &clap::Command) -> Value {
    let mut subcommands = Map::new();
    for s in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        subcommands.insert(s.get_name().to_string(), tree(s));
    }
    let (mut options, mut vals, mut takes, mut positionals) = (vec![], Map::new(), vec![], vec![]);
    for a in cmd.get_arguments().filter(|a| !a.is_hide_set()) {
        if a.is_positional() {
            positionals.extend(values(a));
            continue;
        }
        let flags: Vec<String> = a
            .get_long_and_visible_aliases()
            .into_iter()
            .flatten()
            .map(|l| format!("--{l}"))
            .chain(a.get_short().map(|s| format!("-{s}")))
            .collect();
        let v = values(a);
        for f in &flags {
            options.push(json!([f, help(a.get_help())]));
            if !a.get_action().takes_values() {
                continue;
            }
            if v.is_empty() {
                takes.push(json!(f));
            } else {
                vals.insert(f.clone(), json!(v));
            }
        }
    }
    json!({
        "about": help(cmd.get_about()),
        "subcommands": subcommands,
        "options": options,
        "values": vals,
        "takes": takes,
        "positionals": positionals,
    })
}

fn xonsh(cmd: &clap::Command) -> String {
    format!(
        r#"# costerra completions for xonsh. In ~/.xonshrc:
#   execx($(costerra completions xonsh))
from xonsh.completers.completer import add_one_completer
from xonsh.completers.tools import RichCompletion, contextual_command_completer_for

_COSTERRA_TREE = {tree}


@contextual_command_completer_for("costerra")
def _costerra_complete(ctx):
    node, prev = _COSTERRA_TREE, None
    for a in ctx.args[1 : ctx.arg_index]:
        w = a.value
        if prev in node["values"] or prev in node["takes"]:
            prev = None  # the value of the previous option
            continue
        if w in node["subcommands"]:
            node = node["subcommands"][w]
        prev = w if w.startswith("-") and "=" not in w else None
    prefix = ctx.prefix
    if prev in node["values"]:
        # `--format table,markdown`: complete the last item.
        head, comma, last = prefix.rpartition(",")
        return {{
            RichCompletion(head + comma + v, display=v, description=d)
            for v, d in node["values"][prev]
            if v.startswith(last)
        }} or None
    if prev in node["takes"]:
        return None  # a path or a free value
    if prefix.startswith("-"):
        return {{RichCompletion(f, description=d) for f, d in node["options"] if f.startswith(prefix)}} or None
    out = {{RichCompletion(n, description=s["about"]) for n, s in node["subcommands"].items() if n.startswith(prefix)}}
    out |= {{RichCompletion(v, description=d) for v, d in node["positionals"] if v.startswith(prefix)}}
    return out or None


add_one_completer("costerra", _costerra_complete, "start")
"#,
        tree = serde_json::to_string(&tree(cmd)).expect("serializable")
    )
}
