#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{Read, Take};
use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

const MAX_COMPOSE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub message: &'static str,
}

pub fn check_path(path: &Path) -> Result<Option<Violation>, CheckError> {
    let file = File::open(path).map_err(|_| CheckError::Read)?;
    let mut input = String::new();
    let mut bounded: Take<File> = file.take(MAX_COMPOSE_BYTES + 1);
    bounded
        .read_to_string(&mut input)
        .map_err(|_| CheckError::Read)?;
    if input.len() as u64 > MAX_COMPOSE_BYTES {
        return Err(CheckError::TooLarge);
    }
    let document: Value = serde_yaml_ng::from_str(&input).map_err(|_| CheckError::Yaml)?;
    check_document(&document)
}

pub fn check_document(document: &Value) -> Result<Option<Violation>, CheckError> {
    let root = document.as_mapping().ok_or(CheckError::Shape)?;
    let services = get(root, "services")
        .and_then(Value::as_mapping)
        .ok_or(CheckError::Shape)?;
    let mut entries: Vec<(String, Mapping)> = services
        .iter()
        .map(|(name, service)| {
            Ok((
                name.as_str().ok_or(CheckError::Shape)?.to_owned(),
                merge_mapping(service.as_mapping().ok_or(CheckError::Shape)?)?,
            ))
        })
        .collect::<Result<_, CheckError>>()?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, service) in entries {
        if bool_field(&service, "privileged") == Some(true) {
            return Ok(Some(v(
                "NO_PRIVILEGED",
                "privileged containers are forbidden",
            )));
        }
        if string_field(&service, "network_mode") == Some("host") {
            return Ok(Some(v("NO_HOST_NETWORK", "host networking is forbidden")));
        }
        if !non_root(&service) {
            return Ok(Some(v(
                "NON_ROOT_USER",
                "every service must set a numeric non-root user",
            )));
        }
        if bool_field(&service, "read_only") != Some(true) {
            return Ok(Some(v(
                "READ_ONLY_ROOTFS",
                "every root filesystem must be read-only",
            )));
        }
        if !sequence_contains(&service, "cap_drop", "ALL") {
            return Ok(Some(v(
                "CAP_DROP_ALL",
                "every service must drop all capabilities",
            )));
        }
        if !sequence_contains(&service, "security_opt", "no-new-privileges:true") {
            return Ok(Some(v(
                "NO_NEW_PRIVILEGES",
                "every service must enable no-new-privileges",
            )));
        }
        if !has_resources(&service) {
            return Ok(Some(v(
                "RESOURCE_LIMITS",
                "memory, CPU, pids and a healthcheck are required",
            )));
        }
        if mounts_runtime_socket(&service) {
            return Ok(Some(v(
                "NO_RUNTIME_SOCKET",
                "container runtime sockets are forbidden",
            )));
        }
        if get(&service, "build").is_none() && !image_is_pinned(&service) {
            return Ok(Some(v(
                "IMAGE_PINNED",
                "images must be pinned by sha256 digest",
            )));
        }
        if name != "edge" && get(&service, "ports").is_some() {
            return Ok(Some(v(
                "EDGE_ONLY_PORTS",
                "only the edge service may publish ports",
            )));
        }
        if name == "edge" && publishes_stats_port(&service) {
            return Ok(Some(v(
                "NO_STATS_PORT",
                "HAProxy stats/admin ports must not be published",
            )));
        }
    }
    Ok(None)
}

fn merge_mapping(source: &Mapping) -> Result<Mapping, CheckError> {
    let mut merged = Mapping::new();
    if let Some(base) = get(source, "<<") {
        match base {
            Value::Mapping(map) => merge_into(&mut merged, map),
            Value::Sequence(maps) => {
                // YAML merge precedence is first mapping wins. Insert in reverse so earlier
                // maps overwrite later ones, then explicit service keys overwrite all bases.
                for map in maps.iter().rev() {
                    merge_into(&mut merged, map.as_mapping().ok_or(CheckError::Shape)?);
                }
            }
            _ => return Err(CheckError::Shape),
        }
    }
    merge_into(&mut merged, source);
    merged.remove(Value::String("<<".to_owned()));
    Ok(merged)
}

fn merge_into(target: &mut Mapping, source: &Mapping) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    Read,
    TooLarge,
    Yaml,
    Shape,
}

fn v(rule: &'static str, message: &'static str) -> Violation {
    Violation { rule, message }
}

fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.to_owned()))
}

fn bool_field(map: &Mapping, key: &str) -> Option<bool> {
    get(map, key).and_then(Value::as_bool)
}

fn string_field<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    get(map, key).and_then(Value::as_str)
}

fn non_root(service: &Mapping) -> bool {
    let Some(user) = string_field(service, "user") else {
        return false;
    };
    let uid = user.split(':').next().unwrap_or_default();
    !matches!(uid, "" | "0" | "root") && uid.bytes().all(|b| b.is_ascii_digit())
}

fn sequence_contains(service: &Mapping, key: &str, wanted: &str) -> bool {
    get(service, key)
        .and_then(Value::as_sequence)
        .is_some_and(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value.eq_ignore_ascii_case(wanted))
        })
}

fn has_resources(service: &Mapping) -> bool {
    ["mem_limit", "cpus", "pids_limit", "healthcheck"]
        .iter()
        .all(|key| get(service, key).is_some())
        && get(service, "healthcheck")
            .and_then(Value::as_mapping)
            .and_then(|health| get(health, "test"))
            .is_some()
}

fn mounts_runtime_socket(service: &Mapping) -> bool {
    get(service, "volumes")
        .and_then(Value::as_sequence)
        .is_some_and(|volumes| {
            volumes.iter().any(|volume| match volume {
                Value::String(spec) => runtime_socket(spec.split(':').next().unwrap_or_default()),
                Value::Mapping(spec) => get(spec, "source")
                    .and_then(Value::as_str)
                    .is_some_and(runtime_socket),
                _ => false,
            })
        })
}

fn runtime_socket(source: &str) -> bool {
    matches!(
        source,
        "/var/run/docker.sock"
            | "/run/docker.sock"
            | "/run/containerd/containerd.sock"
            | "/var/run/podman/podman.sock"
            | "/run/podman/podman.sock"
    )
}

fn image_is_pinned(service: &Mapping) -> bool {
    let Some(image) = string_field(service, "image") else {
        return false;
    };
    let Some((_, digest)) = image.rsplit_once("@sha256:") else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn publishes_stats_port(service: &Mapping) -> bool {
    get(service, "ports")
        .and_then(Value::as_sequence)
        .is_some_and(|ports| ports.iter().any(port_is_stats))
}

fn port_is_stats(port: &Value) -> bool {
    match port {
        Value::String(value) => value
            .split(':')
            .any(|part| part.split('/').next() == Some("8404")),
        Value::Mapping(value) => ["target", "published"]
            .iter()
            .filter_map(|key| get(value, key))
            .any(|value| value.as_u64() == Some(8404) || value.as_str() == Some("8404")),
        _ => false,
    }
}
