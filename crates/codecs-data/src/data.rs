//! JSON, YAML, TOML, XML and CSV through one tree (`serde_json::Value`, keys in file order).
//! Where a format can't say something another can (TOML has no null, CSV is flat, XML has
//! attributes and repeated elements) the converter picks a convention and tells the user.

use dotty_core::{ConvertCtx, Error, Result};
use serde_json::{Map, Number, Value};

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not valid {what}: {e}"))
}

// ---- reading ----

pub fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| bad("JSON", e))
}

pub fn parse_yaml(text: &str, ctx: &ConvertCtx) -> Result<Value> {
    use serde::Deserialize;
    let mut docs = Vec::new();
    for doc in serde_yaml_ng::Deserializer::from_str(text) {
        docs.push(Value::deserialize(doc).map_err(|e| bad("YAML", e))?);
    }
    match docs.len() {
        0 => Ok(Value::Null),
        1 => Ok(docs.remove(0)),
        n => {
            ctx.note(format!("The file holds {n} YAML documents; they became a list"));
            Ok(Value::Array(docs))
        }
    }
}

pub fn parse_toml(text: &str) -> Result<Value> {
    fn conv(v: toml::Value) -> Value {
        match v {
            toml::Value::String(s) => Value::String(s),
            toml::Value::Integer(i) => Value::Number(i.into()),
            toml::Value::Float(f) => Number::from_f64(f).map_or_else(|| Value::String(f.to_string()), Value::Number),
            toml::Value::Boolean(b) => Value::Bool(b),
            toml::Value::Datetime(d) => Value::String(d.to_string()),
            toml::Value::Array(a) => Value::Array(a.into_iter().map(conv).collect()),
            toml::Value::Table(t) => Value::Object(t.into_iter().map(|(k, v)| (k, conv(v))).collect()),
        }
    }
    let table: toml::Table = toml::from_str(text).map_err(|e| bad("TOML", e))?;
    Ok(conv(toml::Value::Table(table)))
}

/// Elements become objects: attributes as `@name`, text as `#text` (a text-only element is
/// just a string), repeated elements as a list.
pub fn parse_xml(text: &str) -> Result<Value> {
    use quick_xml::{Reader, XmlVersion, events::Event};
    struct Frame {
        name: String,
        map: Map<String, Value>,
        text: String,
    }
    fn finish(f: Frame) -> (String, Value) {
        let text = f.text.trim().to_string();
        let value = if f.map.is_empty() {
            if text.is_empty() { Value::Null } else { Value::String(text) }
        } else {
            let mut map = f.map;
            if !text.is_empty() {
                map.insert("#text".into(), Value::String(text));
            }
            Value::Object(map)
        };
        (f.name, value)
    }
    fn insert(map: &mut Map<String, Value>, key: String, value: Value) {
        match map.get_mut(&key) {
            None => {
                map.insert(key, value);
            }
            Some(Value::Array(a)) if !key.starts_with('@') => a.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, value]);
            }
        }
    }
    let open = |e: &quick_xml::events::BytesStart| -> Result<Frame> {
        let mut map = Map::new();
        for a in e.attributes() {
            let a = a.map_err(|e| bad("XML", e))?;
            let key = format!("@{}", a.key.as_ref());
            let value = a.normalized_value(XmlVersion::Implicit1_0).map_err(|e| bad("XML", e))?;
            map.insert(key, Value::String(value.into_owned()));
        }
        Ok(Frame { name: e.name().as_ref().to_string(), map, text: String::new() })
    };
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<(String, Value)> = None;
    let close = |stack: &mut Vec<Frame>, f: Frame, root: &mut Option<(String, Value)>| {
        let (name, value) = finish(f);
        match stack.last_mut() {
            Some(parent) => insert(&mut parent.map, name, value),
            None => *root = Some((name, value)),
        }
    };
    loop {
        match reader.read_event().map_err(|e| bad("XML", e))? {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let f = open(&e)?;
                close(&mut stack, f, &mut root);
            }
            Event::End(_) => {
                if let Some(f) = stack.pop() {
                    close(&mut stack, f, &mut root);
                }
            }
            Event::Text(t) => {
                if let Some(f) = stack.last_mut() {
                    f.text.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::CData(c) => {
                if let Some(f) = stack.last_mut() {
                    f.text.push_str(&c.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(r) => {
                let name: &str = &r;
                let ch = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => r.resolve_char_ref().map_err(|e| bad("XML", e))?,
                };
                if let (Some(f), Some(ch)) = (stack.last_mut(), ch) {
                    f.text.push(ch);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (name, value) = root.ok_or_else(|| Error::Codec("not valid XML: no root element".into()))?;
    Ok(Value::Object(Map::from_iter([(name, value)])))
}

/// Which separator a CSV file uses, judged from its first line.
fn delimiter(text: &str) -> u8 {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    b",;\t|".iter().copied().max_by_key(|&d| line.bytes().filter(|&b| b == d).count()).filter(|&d| line.bytes().any(|b| b == d)).unwrap_or(b',')
}

/// `true`/`false`, and numbers that print back exactly as written (so `007` and `1e3` stay text).
fn guess(s: &str) -> Value {
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = s.parse::<i64>()
        && i.to_string() == s
    {
        return Value::Number(i.into());
    }
    if s.contains('.')
        && let Ok(f) = s.parse::<f64>()
        && let Some(n) = Number::from_f64(f)
        && n.to_string() == s
    {
        return Value::Number(n);
    }
    Value::String(s.to_string())
}

/// A list of records keyed by the header row.
pub fn parse_csv(text: &str, guess_types: bool) -> Result<Value> {
    let mut reader = csv::ReaderBuilder::new().delimiter(delimiter(text)).flexible(true).has_headers(false).from_reader(text.as_bytes());
    let mut rows = reader.records();
    let Some(header) = rows.next() else { return Ok(Value::Array(Vec::new())) };
    let mut names: Vec<String> = Vec::new();
    for (i, h) in header.map_err(|e| bad("CSV", e))?.iter().enumerate() {
        let mut name = if h.trim().is_empty() { format!("column {}", i + 1) } else { h.trim().to_string() };
        while names.contains(&name) {
            name.push('_');
        }
        names.push(name);
    }
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| bad("CSV", e))?;
        let mut obj = Map::new();
        for (i, cell) in row.iter().enumerate() {
            let key = names.get(i).cloned().unwrap_or_else(|| format!("column {}", i + 1));
            obj.insert(key, if guess_types { guess(cell) } else { Value::String(cell.to_string()) });
        }
        for name in names.iter().skip(row.len()) {
            obj.insert(name.clone(), Value::String(String::new()));
        }
        out.push(Value::Object(obj));
    }
    Ok(Value::Array(out))
}

// ---- writing ----

pub fn write_json(v: &Value, pretty: bool) -> String {
    let mut s = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) }.unwrap_or_default();
    s.push('\n');
    s
}

pub fn write_yaml(v: &Value) -> Result<String> {
    serde_yaml_ng::to_string(v).map_err(|e| Error::Codec(format!("YAML: {e}")))
}

/// TOML wants a table on top, has no null and no mixed lists of tables and values.
pub fn write_toml(v: &Value, ctx: &ConvertCtx) -> Result<String> {
    fn conv(v: &Value, dropped: &mut usize) -> Option<toml::Value> {
        Some(match v {
            Value::Null => {
                *dropped += 1;
                return None;
            }
            Value::Bool(b) => toml::Value::Boolean(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => toml::Value::Integer(i),
                None => toml::Value::Float(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => toml::Value::String(s.clone()),
            Value::Array(a) => toml::Value::Array(a.iter().filter_map(|x| conv(x, dropped)).collect()),
            Value::Object(o) => toml::Value::Table(o.iter().filter_map(|(k, x)| conv(x, dropped).map(|x| (k.clone(), x))).collect()),
        })
    }
    let wrapped;
    let top = if let Value::Object(_) = v {
        v
    } else {
        ctx.note("TOML needs a table at the top, so the data was put under the key \"items\"");
        wrapped = Value::Object(Map::from_iter([("items".to_string(), v.clone())]));
        &wrapped
    };
    let mut dropped = 0;
    let value = conv(top, &mut dropped).unwrap_or(toml::Value::Table(toml::Table::new()));
    if dropped > 0 {
        ctx.note(format!("TOML has no null: {dropped} empty value(s) were left out"));
    }
    toml::to_string_pretty(&value).map_err(|e| Error::Codec(format!("TOML: {e}")))
}

fn xml_name(s: &str) -> String {
    let mut name: String = s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
    if name.chars().next().is_none_or(|c| !(c.is_alphabetic() || c == '_')) {
        name.insert(0, '_');
    }
    name
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn write_element(out: &mut String, name: &str, v: &Value, depth: usize) {
    let pad = "  ".repeat(depth);
    let name = xml_name(name);
    match v {
        Value::Array(items) => {
            for item in items {
                write_element(out, &name, item, depth);
            }
        }
        Value::Object(map) => {
            out.push_str(&format!("{pad}<{name}"));
            for (k, a) in map.iter().filter(|(k, a)| k.starts_with('@') && !matches!(a, Value::Object(_) | Value::Array(_))) {
                out.push_str(&format!(" {}=\"{}\"", xml_name(&k[1..]), escape(&scalar_text(a), true)));
            }
            let text = map.get("#text").map(scalar_text);
            let children: Vec<_> = map.iter().filter(|(k, _)| !k.starts_with('@') && *k != "#text").collect();
            if children.is_empty() {
                match text {
                    Some(t) => out.push_str(&format!(">{}</{name}>\n", escape(&t, false))),
                    None => out.push_str("/>\n"),
                }
            } else {
                out.push_str(">\n");
                if let Some(t) = text {
                    out.push_str(&format!("{pad}  {}\n", escape(&t, false)));
                }
                for (k, child) in children {
                    write_element(out, k, child, depth + 1);
                }
                out.push_str(&format!("{pad}</{name}>\n"));
            }
        }
        Value::Null => out.push_str(&format!("{pad}<{name}/>\n")),
        scalar => out.push_str(&format!("{pad}<{name}>{}</{name}>\n", escape(&scalar_text(scalar), false))),
    }
}

pub fn write_xml(v: &Value, ctx: &ConvertCtx) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    match v {
        Value::Object(m) if m.len() == 1 && !m.keys().next().is_some_and(|k| k.starts_with('@') || k == "#text") => {
            let (k, child) = m.iter().next().unwrap();
            write_element(&mut out, k, child, 0);
        }
        _ => {
            ctx.note("XML needs one root element; the data was put inside <root>");
            write_element(&mut out, "root", v, 0);
        }
    }
    out
}

/// CSV from a list of records (their keys are the columns), a list of lists, a list of
/// plain values, or one record. Nested values are written as JSON text.
pub fn write_csv(v: &Value, delim: u8, ctx: &ConvertCtx) -> Result<String> {
    let single;
    let rows: &[Value] = match v {
        Value::Array(a) => a,
        Value::Object(_) => {
            ctx.note("A single record was written as a one-row table");
            single = [v.clone()];
            &single
        }
        _ => return Err(Error::Codec("CSV needs a list of records, but this holds a single value".into())),
    };
    let mut nested = false;
    let mut cell = |x: &Value| -> String {
        match x {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            Value::Array(_) | Value::Object(_) => {
                nested = true;
                x.to_string()
            }
            other => other.to_string(),
        }
    };
    let mut w = csv::WriterBuilder::new().delimiter(delim).flexible(true).from_writer(Vec::new());
    let io = |e: csv::Error| Error::Codec(format!("CSV: {e}"));
    if rows.iter().all(|r| r.is_object()) {
        let mut columns: Vec<&String> = Vec::new();
        for r in rows {
            for k in r.as_object().into_iter().flat_map(|o| o.keys()) {
                if !columns.contains(&k) {
                    columns.push(k);
                }
            }
        }
        w.write_record(columns.iter().map(|c| c.as_str())).map_err(io)?;
        for r in rows {
            let o = r.as_object().unwrap();
            w.write_record(columns.iter().map(|c| o.get(*c).map(&mut cell).unwrap_or_default())).map_err(io)?;
        }
    } else if rows.iter().all(|r| r.is_array()) {
        for r in rows {
            w.write_record(r.as_array().unwrap().iter().map(&mut cell)).map_err(io)?;
        }
    } else {
        w.write_record(["value"]).map_err(io)?;
        for r in rows {
            w.write_record([cell(r)]).map_err(io)?;
        }
    }
    let bytes = w.into_inner().map_err(|e| Error::Codec(format!("CSV: {e}")))?;
    if nested {
        ctx.note("Nested lists and records were written as JSON text inside their cells");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    fn with_ctx<R>(f: impl FnOnce(&ConvertCtx) -> R) -> (R, Vec<String>) {
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let r = f(&ctx);
        (r, ctx.take_notes())
    }

    #[test]
    fn json_yaml_and_toml_carry_the_same_data_in_the_same_order() {
        let v = json!({"name": "dotty", "zeta": 1, "alpha": [1, 2.5, true], "nested": {"b": "x", "a": "y"}});
        let (y, _) = with_ctx(|_| write_yaml(&v).unwrap());
        assert!(y.find("zeta").unwrap() < y.find("alpha").unwrap(), "key order kept: {y}");
        assert_eq!(with_ctx(|c| parse_yaml(&y, c).unwrap()).0, v);
        let (t, _) = with_ctx(|c| write_toml(&v, c).unwrap());
        assert_eq!(parse_toml(&t).unwrap(), v);
        assert_eq!(parse_json(&write_json(&v, false)).unwrap(), v);
        assert!(write_json(&v, true).contains("\n  \"name\""));
    }

    #[test]
    fn toml_limits_are_explained() {
        let (t, notes) = with_ctx(|c| write_toml(&json!({"a": null, "b": 1}), c).unwrap());
        assert_eq!(parse_toml(&t).unwrap(), json!({"b": 1}));
        assert!(notes.iter().any(|n| n.contains("no null")));
        let (t, notes) = with_ctx(|c| write_toml(&json!([1, 2]), c).unwrap());
        assert_eq!(parse_toml(&t).unwrap(), json!({"items": [1, 2]}));
        assert!(notes.iter().any(|n| n.contains("\"items\"")));
        assert_eq!(parse_toml("when = 1979-05-27T07:32:00Z").unwrap(), json!({"when": "1979-05-27T07:32:00Z"}));
    }

    #[test]
    fn yaml_with_several_documents_becomes_a_list() {
        let (v, notes) = with_ctx(|c| parse_yaml("a: 1\n---\nb: 2\n", c).unwrap());
        assert_eq!(v, json!([{"a": 1}, {"b": 2}]));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn xml_maps_attributes_text_and_repeats_and_comes_back() {
        let xml = r#"<?xml version="1.0"?><!-- c --><library name="main"><book id="1">A &amp; B</book><book id="2"><title>C</title></book><empty/><note><![CDATA[<raw>]]></note></library>"#;
        let v = parse_xml(xml).unwrap();
        assert_eq!(
            v,
            json!({"library": {"@name": "main", "book": [{"@id": "1", "#text": "A & B"}, {"@id": "2", "title": "C"}], "empty": null, "note": "<raw>"}})
        );
        let (out, notes) = with_ctx(|c| write_xml(&v, c));
        assert!(notes.is_empty());
        assert_eq!(parse_xml(&out).unwrap(), v, "{out}");
        // Data without a single root gets one.
        let (out, notes) = with_ctx(|c| write_xml(&json!({"a": 1, "b": [2, 3]}), c));
        assert!(out.contains("<root>") && out.contains("<b>2</b>\n  <b>3</b>"), "{out}");
        assert_eq!(notes.len(), 1);
        assert!(parse_xml("<a><b></a>").is_err());
        assert!(with_ctx(|c| write_xml(&json!({"1bad name": "x"}), c)).0.contains("<_1bad_name>"));
    }

    #[test]
    fn csv_reads_records_guesses_types_carefully_and_writes_them_back() {
        let csv = "id;name;zip;price;ok\n1;Ann;00123;2.50;true\n2;\"Bo; B\";99999;3;false\n";
        let v = parse_csv(csv, true).unwrap();
        assert_eq!(v[0], json!({"id": 1, "name": "Ann", "zip": "00123", "price": "2.50", "ok": true}));
        assert_eq!(v[1]["price"], 3, "a number that prints back as written becomes one; 2.50 would lose its zero, so it stays text");
        assert_eq!(v[1]["name"], "Bo; B");
        assert_eq!(parse_csv(csv, false).unwrap()[0]["id"], "1", "kept as text");
        let (out, _) = with_ctx(|c| write_csv(&v, b',', c).unwrap());
        assert!(out.starts_with("id,name,zip,price,ok\n1,Ann,00123,2.50,true\n"), "{out}");
        assert!(out.contains("Bo; B"), "no quotes needed with a comma separator");
        assert_eq!(parse_csv(&out, true).unwrap(), v);
        assert_eq!(parse_csv("a\tb\n1\t2\n", true).unwrap(), json!([{"a": 1, "b": 2}]));
        // Short rows, long rows, duplicate and empty headers.
        assert_eq!(parse_csv("a,a,\n1\n1,2,3,4\n", true).unwrap(), json!([{"a": 1, "a_": "", "column 3": ""}, {"a": 1, "a_": 2, "column 3": 3, "column 4": 4}]));
    }

    #[test]
    fn csv_output_handles_other_shapes() {
        let (out, notes) = with_ctx(|c| write_csv(&json!([{"a": 1, "b": {"x": 1}}, {"a": 2, "c": null}]), b'\t', c).unwrap());
        assert_eq!(out, "a\tb\tc\n1\t\"{\"\"x\"\":1}\"\t\n2\t\t\n");
        assert!(notes.iter().any(|n| n.contains("JSON text")));
        assert_eq!(with_ctx(|c| write_csv(&json!([[1, 2], [3]]), b',', c).unwrap()).0, "1,2\n3\n");
        assert_eq!(with_ctx(|c| write_csv(&json!([1, "x"]), b',', c).unwrap()).0, "value\n1\nx\n");
        assert!(with_ctx(|c| write_csv(&json!(5), b',', c)).0.is_err());
        assert!(with_ctx(|c| write_csv(&json!({"a": 1}), b',', c)).1.len() == 1);
    }
}
