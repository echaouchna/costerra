use costerra::hcl::eval::Evaluator;
use costerra::hcl::types::TypeSpec;
use costerra::hcl::value::{Map, Value};
use std::path::Path;

fn eval_with(src: &str, root: &Map) -> Value {
    let body = hcl::parse(&format!("x = {src}")).expect("parse");
    let expr = body.attributes().next().unwrap().expr.clone();
    Evaluator::new(root, Path::new(".")).eval_lenient(&expr)
}

fn eval(src: &str) -> Value {
    let mut root = Map::new();
    let mut var = Map::new();
    var.insert("env".into(), Value::from("dev"));
    var.insert("n".into(), Value::Number(3.0));
    var.insert("unknown".into(), Value::Unknown);
    let mut obj = Map::new();
    obj.insert("a".into(), Value::Number(1.0));
    obj.insert("b".into(), Value::Number(2.0));
    var.insert("obj".into(), Value::Map(obj));
    var.insert(
        "items".into(),
        Value::List(vec![
            Value::Map([("name".to_string(), Value::from("x")), ("size".to_string(), Value::Number(10.0))].into_iter().collect()),
            Value::Map([("name".to_string(), Value::from("y")), ("size".to_string(), Value::Number(20.0))].into_iter().collect()),
        ]),
    );
    root.insert("var".into(), Value::Map(var));
    eval_with(src, &root)
}

fn s(v: &str) -> Value {
    Value::from(v)
}

#[test]
fn templates_and_conditionals() {
    assert_eq!(eval(r#""${var.env}-app""#), s("dev-app"));
    assert_eq!(eval(r#"var.env == "prod" ? "n2-standard-8" : "e2-medium""#), s("e2-medium"));
    assert_eq!(eval(r#""%{if var.env == "dev"}small%{else}big%{endif}""#), s("small"));
    assert_eq!(eval(r#""${var.unknown}-x""#), Value::Unknown);
    // Unknown condition with identical branches is still known.
    assert_eq!(eval(r#"var.unknown ? "a" : "a""#), s("a"));
}

#[test]
fn arithmetic_and_logic() {
    assert_eq!(eval("var.n * 2 + 1"), Value::Number(7.0));
    assert_eq!(eval("var.n > 2 && true"), Value::Bool(true));
    assert_eq!(eval("false && var.unknown"), Value::Bool(false));
    assert_eq!(eval("var.unknown + 1"), Value::Unknown);
}

#[test]
fn for_and_splat() {
    assert_eq!(eval("[for i in var.items : i.size * 2]"), Value::List(vec![Value::Number(20.0), Value::Number(40.0)]));
    assert_eq!(eval("var.items[*].name"), Value::List(vec![s("x"), s("y")]));
    assert_eq!(eval("{ for i in var.items : i.name => i.size if i.size > 10 }").get("y"), Some(&Value::Number(20.0)));
    assert_eq!(eval("length({ for k, v in var.obj : k => v })"), Value::Number(2.0));
}

#[test]
fn functions() {
    assert_eq!(eval(r#"lookup(var.obj, "c", 42)"#), Value::Number(42.0));
    assert_eq!(eval(r#"merge(var.obj, { c = 3 })"#).len(), Some(3));
    assert_eq!(eval(r#"format("%s-%03d", "vm", 7)"#), s("vm-007"));
    assert_eq!(eval(r#"join(",", ["a", "b"])"#), s("a,b"));
    assert_eq!(eval(r#"split("-", "europe-west1-b")[1]"#), s("west1"));
    assert_eq!(eval(r#"regex("^([a-z]+)-([a-z0-9]+)", "europe-west1")[1]"#), s("west1"));
    assert_eq!(eval(r#"replace("europe-west1-b", "/-[a-z]$/", "")"#), s("europe-west1"));
    assert_eq!(eval(r#"coalesce("", null, "x")"#), s("x"));
    assert_eq!(eval(r#"try(var.obj.missing.deep, "fallback")"#), s("fallback"));
    assert_eq!(eval(r#"can(var.obj.a)"#), Value::Bool(true));
    assert_eq!(eval(r#"max(1, 5, 3)"#), Value::Number(5.0));
    assert_eq!(eval(r#"flatten([[1], [2, [3]]])"#).len(), Some(3));
    assert_eq!(eval(r#"yamldecode("a: 1\nb: [x, y]").b[1]"#), s("y"));
    // YAML 1.2 like Terraform: only true/false are booleans.
    assert_eq!(eval(r#"yamldecode("a: yes\nb: on\nc: true").a"#), s("yes"));
    assert_eq!(eval(r#"yamldecode("a: yes\nb: on\nc: true").c"#), Value::Bool(true));
    assert_eq!(eval(r#"yamldecode("1: one")["1"]"#), s("one"));
    assert_eq!(eval(r#"yamldecode("")"#), Value::Null);
    assert_eq!(eval(r#"jsondecode("{\"k\": true}").k"#), Value::Bool(true));
    assert_eq!(eval(r#"contains(["a", "b"], "b")"#), Value::Bool(true));
    assert_eq!(eval(r#"element(["a", "b"], 3)"#), s("b"));
    assert_eq!(eval(r#"upper(substr("europe", 0, 2))"#), s("EU"));
    assert_eq!(eval(r#"tonumber("12") + 1"#), Value::Number(13.0));
    assert_eq!(eval(r#"zipmap(["a", "b"], [1, 2]).b"#), Value::Number(2.0));
    assert_eq!(eval(r#"range(3)"#).len(), Some(3));
    assert_eq!(eval(r#"sum([1, 2, 3.5])"#), Value::Number(6.5));
}

#[test]
fn optional_object_defaults() {
    let body =
        hcl::parse(r#"t = map(object({ machine = optional(string, "e2-medium"), disks = optional(list(number), [10]), name = string }))"#)
            .unwrap();
    let ty = TypeSpec::parse(&body.attributes().next().unwrap().expr);
    let input = eval(r#"{ a = { name = "x" }, b = { name = "y", machine = "n2-standard-2" } }"#);
    let out = ty.convert(input);
    assert_eq!(out.path("a.machine"), Some(&s("e2-medium")));
    assert_eq!(out.path("a.disks.0"), Some(&Value::Number(10.0)));
    assert_eq!(out.path("b.machine"), Some(&s("n2-standard-2")));
}

#[test]
fn heredoc() {
    let v = eval("<<-EOT\n    hello ${var.env}\n  EOT\n");
    assert_eq!(v, s("hello dev\n"));
}
