//! `batch alloc`: a JSON spec of ips (each with an `out` path and its hosts) in, one daemon call
//! (`POST /batch/alloc`, all or nothing, always idempotent), and a JSON object of their addresses,
//! nested by `out`, out.
//!
//! ```json
//! { "ips": [
//!     { "name": "db", "out": "app.db" },
//!     { "name": "web", "out": "app.web", "hosts": ["web.obd.internal", "*.web.obd.internal"] }
//! ] }
//! ```
//!
//! prints `{ "app": { "db": "10.57.0.1", "web": "10.57.0.2" } }`.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;

use obeliskd::ObeliskdError;
use obeliskd::control::api::{BatchAlloc, BatchAllocIp, SupernetNode};
use obeliskd::control::client::ControlClient;
use obeliskd::model::{Hostname, Name};
use serde::Deserialize;
use serde_json::{Map, Value};

/// The spec, as the user writes it. Unknown fields are refused, so a typo (`host`) isn't
/// silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub ips: Vec<SpecIp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecIp {
    pub name: Name,
    /// Where its address goes in the output: keys separated by `.`.
    pub out: String,
    #[serde(default)]
    pub hosts: Vec<Hostname>,
}

/// Where the spec comes from: a file, or the JSON itself.
#[derive(Clone, Copy)]
pub enum Input<'a> {
    File(&'a Path),
    Json(&'a str),
}

/// Reads and checks the spec, has the daemon allocate it, and prints the addresses by `out`.
///
/// # Errors
///
/// If the spec can't be read, isn't valid, or has clashing `out` paths (all before the daemon is
/// asked); or if the daemon refuses (then nothing changed).
pub async fn alloc(
    socket: &Path,
    supernet: Name,
    subnet: Name,
    tag: Option<Name>,
    input: Input<'_>,
) -> Result<(), ObeliskdError> {
    let spec = read(input)?;
    out_tree(spec.ips.iter().map(|ip| (ip.out.as_str(), Value::Null)))?;
    let request = BatchAlloc {
        supernet,
        subnet,
        tag,
        ips: spec
            .ips
            .iter()
            .map(|ip| BatchAllocIp {
                name: ip.name.clone(),
                hosts: ip.hosts.clone(),
            })
            .collect(),
    };
    let answer = ControlClient::new(socket)?.alloc_batch(&request).await?;
    let addresses = addresses(&answer);
    let mut outs = Vec::new();
    for ip in &spec.ips {
        let address = addresses
            .get(&ip.name)
            .ok_or_else(|| ObeliskdError::Generic {
                message: format!("the daemon's answer has no ip {}", ip.name),
            })?;
        outs.push((ip.out.as_str(), Value::String(address.to_string())));
    }
    let tree = out_tree(outs)?;
    let json = serde_json::to_string_pretty(&tree).expect("JSON values serialize");
    println!("{json}");
    Ok(())
}

fn read(input: Input<'_>) -> Result<Spec, ObeliskdError> {
    let (json, from) = match input {
        Input::File(path) => (
            std::fs::read_to_string(path).map_err(|e| ObeliskdError::Generic {
                message: format!("failed to read {}: {e}", path.display()),
            })?,
            path.display().to_string(),
        ),
        Input::Json(json) => (json.to_string(), "--input".to_string()),
    };
    serde_json::from_str(&json)
        .map_err(|e| ObeliskdError::Invalid(format!("{from} isn't a valid batch spec: {e}")))
}

/// The addresses in the daemon's answer (the subnet's path, holding the batch's ips), by name.
fn addresses(answer: &SupernetNode) -> HashMap<Name, Ipv4Addr> {
    answer
        .subnets
        .iter()
        .flatten()
        .flat_map(|subnet| subnet.ips.iter().flatten())
        .map(|ip| (ip.name.clone(), ip.address))
        .collect()
}

/// Nests each value at its `out` path. Refuses an empty key, the same path twice, and a path
/// inside another's value (`a.b` and `a.b.c`), so it's run on the spec before anything is
/// allocated.
fn out_tree<'a>(
    entries: impl IntoIterator<Item = (&'a str, Value)>,
) -> Result<Value, ObeliskdError> {
    let mut root = Map::new();
    for (out, value) in entries {
        let keys: Vec<&str> = out.split('.').collect();
        if keys.iter().any(|k| k.is_empty()) {
            return Err(ObeliskdError::Invalid(format!(
                "out {out:?} has an empty key"
            )));
        }
        let (last, parents) = keys.split_last().expect("split always yields a key");
        let mut map = &mut root;
        for (depth, key) in parents.iter().enumerate() {
            let child = map
                .entry((*key).to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map = child.as_object_mut().ok_or_else(|| {
                ObeliskdError::Invalid(format!(
                    "out {out:?} is inside out {:?}",
                    keys[..=depth].join(".")
                ))
            })?;
        }
        match map.get(*last) {
            Some(Value::Object(_)) => {
                return Err(ObeliskdError::Invalid(format!(
                    "out {out:?} has other outs inside it"
                )));
            }
            Some(_) => {
                return Err(ObeliskdError::Invalid(format!("out {out:?} is used twice")));
            }
            None => {
                map.insert((*last).to_string(), value);
            }
        }
    }
    Ok(Value::Object(root))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tree(outs: &[(&str, &str)]) -> Result<Value, ObeliskdError> {
        out_tree(outs.iter().map(|(o, v)| (*o, json!(v))))
    }

    fn invalid(result: Result<Value, ObeliskdError>) -> String {
        match result {
            Err(ObeliskdError::Invalid(message)) => message,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn outs_nest_by_dots() {
        let tree = tree(&[
            ("outer.inner.ips.ip-1", "1.2.3.4"),
            ("outer.inner.ips.ip-2", "5.6.7.8"),
            ("outer.inner.ip-3", "9.10.11.12"),
            ("top", "1.1.1.1"),
        ])
        .unwrap();
        assert_eq!(
            tree,
            json!({
                "outer": { "inner": {
                    "ips": { "ip-1": "1.2.3.4", "ip-2": "5.6.7.8" },
                    "ip-3": "9.10.11.12",
                } },
                "top": "1.1.1.1",
            })
        );
    }

    #[test]
    fn clashing_outs_are_refused() {
        assert_eq!(
            invalid(tree(&[("a.b", "x"), ("a.b", "y")])),
            "out \"a.b\" is used twice"
        );
        assert_eq!(
            invalid(tree(&[("a.b", "x"), ("a.b.c", "y")])),
            "out \"a.b.c\" is inside out \"a.b\""
        );
        assert_eq!(
            invalid(tree(&[("a.b.c", "x"), ("a.b", "y")])),
            "out \"a.b\" has other outs inside it"
        );
        for empty in ["", "a..b", ".a", "a."] {
            assert_eq!(
                invalid(tree(&[(empty, "x")])),
                format!("out {empty:?} has an empty key")
            );
        }
    }

    #[test]
    fn specs_parse_strictly() {
        let spec = read(Input::Json(
            r#"{ "ips": [
                { "name": "ip-1", "out": "a.ip-1" },
                { "name": "ip-2", "out": "a.ip-2", "hosts": ["a.b.c.obd.internal", "stuff.*.obd.internal"] }
            ] }"#,
        ))
        .unwrap();
        assert_eq!(spec.ips.len(), 2);
        assert_eq!(spec.ips[0].hosts, Vec::<Hostname>::new());
        assert_eq!(spec.ips[1].hosts.len(), 2);

        let typo = read(Input::Json(
            r#"{ "ips": [ { "name": "ip-1", "out": "a", "host": ["a.obd.internal"] } ] }"#,
        ));
        assert!(
            matches!(typo, Err(ObeliskdError::Invalid(m)) if m.contains("unknown field `host`"))
        );
        let no_out = read(Input::Json(r#"{ "ips": [ { "name": "ip-1" } ] }"#));
        assert!(
            matches!(no_out, Err(ObeliskdError::Invalid(m)) if m.contains("missing field `out`"))
        );
        let bad_name = read(Input::Json(
            r#"{ "ips": [ { "name": "Not A Name", "out": "a" } ] }"#,
        ));
        assert!(matches!(bad_name, Err(ObeliskdError::Invalid(_))));
    }
}
