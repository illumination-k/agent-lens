use std::collections::{HashMap, HashSet};

use lens_rust::{CallKind, CallSite};

use super::{NodeView, Resolution, name_last_segment};

pub(super) struct Resolver {
    qualified: HashMap<String, Vec<String>>,
    last_segment: HashMap<String, Vec<String>>,
}

impl Resolver {
    pub(super) fn new(nodes: &[NodeView]) -> Self {
        let mut qualified: HashMap<String, Vec<String>> = HashMap::new();
        let mut last_segment: HashMap<String, Vec<String>> = HashMap::new();
        for node in nodes {
            qualified
                .entry(node.qualified_name.clone())
                .or_default()
                .push(node.id.clone());
            last_segment
                .entry(name_last_segment(&node.qualified_name).to_owned())
                .or_default()
                .push(node.id.clone());
        }
        Self {
            qualified,
            last_segment,
        }
    }

    pub(super) fn resolve(&self, site: &CallSite) -> (Option<String>, Resolution) {
        let Some(callee_name) = site.callee_name.as_deref() else {
            return (None, Resolution::Anonymous);
        };
        if site.call_kind == CallKind::ReceiverMethod {
            return (None, Resolution::Unresolved);
        }
        for candidate in lexical_candidates(site) {
            if let Some(ids) = self.qualified.get(&candidate) {
                return resolve_ids(ids);
            }
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return (None, Resolution::Unresolved);
        };
        resolve_ids(ids)
    }
}

pub(super) fn lexical_candidates(site: &CallSite) -> Vec<String> {
    let Some(callee_name) = site.callee_name.as_deref() else {
        return Vec::new();
    };
    let Some(callee_path) = site.callee_path.as_deref() else {
        return vec![qualify_module(&site.module, callee_name)];
    };
    let segments: Vec<&str> = callee_path.split("::").collect();
    if segments.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    match segments[0] {
        "crate" => candidates.push(callee_path.to_owned()),
        "self" => {
            if let Some(path) = prefix_with_tail(module_segments(&site.module), &segments, 1) {
                candidates.push(path);
            }
        }
        "super" => {
            if let Some(path) = resolve_super_path(&site.module, &segments) {
                candidates.push(path);
            }
        }
        "Self" => {
            if let Some(owner) = site.caller_impl_owner.as_deref()
                && let Some(tail) = join_tail(&segments, 1)
            {
                candidates.push(qualify_module(&site.module, &format!("{owner}::{tail}")));
            }
        }
        _ => {
            if segments.len() == 1 {
                candidates.push(qualify_module(&site.module, callee_name));
            } else {
                candidates.push(qualify_module(&site.module, callee_path));
            }
            if let Some(alias_target) = alias_target(site, segments[0])
                && let Some(path) = prefix_with_tail(
                    alias_target.split("::").map(ToOwned::to_owned).collect(),
                    &segments,
                    1,
                )
            {
                candidates.push(path);
            }
        }
    }
    if segments.len() == 1
        && let Some(alias_target) = alias_target(site, segments[0])
    {
        candidates.push(alias_target.to_owned());
    }
    dedupe_preserving_order(candidates)
}

pub(super) fn prefix_with_tail(
    mut prefix: Vec<String>,
    segments: &[&str],
    tail_start: usize,
) -> Option<String> {
    if tail_start > segments.len() {
        return None;
    }
    prefix.extend(segments.iter().skip(tail_start).map(|s| (*s).to_owned()));
    Some(prefix.join("::"))
}

pub(super) fn resolve_super_path(module: &str, segments: &[&str]) -> Option<String> {
    let mut absolute = module_segments(module);
    for segment in segments {
        if *segment == "super" {
            if absolute.len() <= 1 {
                return None;
            }
            absolute.pop();
        } else {
            absolute.push((*segment).to_owned());
        }
    }
    Some(absolute.join("::"))
}

pub(super) fn join_tail(segments: &[&str], start: usize) -> Option<String> {
    if start >= segments.len() {
        None
    } else {
        Some(segments[start..].join("::"))
    }
}

fn alias_target<'a>(site: &'a CallSite, alias: &str) -> Option<&'a str> {
    site.visible_aliases
        .iter()
        .rev()
        .find(|entry| entry.alias == alias)
        .map(|entry| entry.target.as_str())
}

fn module_segments(module: &str) -> Vec<String> {
    module.split("::").map(ToOwned::to_owned).collect()
}

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn dedupe_preserving_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn resolve_ids(ids: &[String]) -> (Option<String>, Resolution) {
    if ids.len() == 1 {
        (ids.first().cloned(), Resolution::Resolved)
    } else {
        (None, Resolution::Ambiguous)
    }
}
