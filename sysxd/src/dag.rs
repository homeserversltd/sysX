//! Pre-reactor DAG — Kahn topological sort, cycle severing, max depth 16 (`15`, `16` §2).
//!
//! On cycle: **`[SYSX] CRITICAL: CYCLIC DEPENDENCY`** — offending nodes are **dropped**; boot continues with
//! the remaining acyclic branch (**`forge_boot_order`**).

use std::collections::{HashMap, HashSet, VecDeque};

use log::{error, info};

use sysx_schema::ServiceSchema;

use crate::SysXError;

/// Maximum allowed dependency chain depth (`16` §2, `15`).
const MAX_DAG_DEPTH: usize = 16;

/// Result of **`forge_boot_order`**: linear boot order + nodes removed as cyclic.
#[derive(Debug, Clone)]
pub struct ForgedDag {
    pub boot_order: Vec<String>,
    pub dropped_cyclic: Vec<String>,
}

/// Runtime DAG view: topological order + adjacency (**`depends_on`**) + reverse (**dependents**).
#[derive(Debug, Clone)]
pub struct DagRuntime {
    pub boot_order: Vec<String>,
    /// For each service: its **`depends_on`** list (only services still in the forged DAG).
    pub graph: HashMap<String, Vec<String>>,
    /// For each service name: services that list it in **`depends_on`** (downstream / cascade edges).
    pub dependents: HashMap<String, Vec<String>>,
}

impl DagRuntime {
    pub fn from_forged(forged: &ForgedDag, schemas: &HashMap<String, ServiceSchema>) -> Self {
        let names: HashSet<String> = forged.boot_order.iter().cloned().collect();
        let active: Vec<ServiceSchema> = forged
            .boot_order
            .iter()
            .filter_map(|n| schemas.get(n).cloned())
            .collect();
        let graph = build_dependency_graph_filtered(&active, &names);
        let dependents = build_dependents(&graph);
        Self {
            boot_order: forged.boot_order.clone(),
            graph,
            dependents,
        }
    }
}

/// Validate enabled services, dependency closure, depth ≤ 16 — **fails** on missing dep or depth.
/// Prefer **`forge_boot_order`** for boot; this is kept for tests that expect strict validation without forging.
pub fn validate(schemas: &[ServiceSchema]) -> Result<(), SysXError> {
    let forged = forge_boot_order(schemas)?;
    if !forged.dropped_cyclic.is_empty() {
        return Err(SysXError::Dag(
            "validate() called but graph contained cycles — use forge_boot_order()".into(),
        ));
    }
    Ok(())
}

/// Load enabled schemas, build the graph, run Kahn with **cycle severing**, return boot order + dropped nodes.
pub fn forge_boot_order(schemas: &[ServiceSchema]) -> Result<ForgedDag, SysXError> {
    let mut active: Vec<ServiceSchema> = schemas.iter().filter(|s| s.enabled).cloned().collect();

    if active.is_empty() {
        info!("No enabled services — empty DAG (15)");
        info!("SYSX_ORACLE_DAG_OK max_depth=0 services=0 dropped_cyclic=0");
        return Ok(ForgedDag {
            boot_order: vec![],
            dropped_cyclic: vec![],
        });
    }

    let mut names: HashSet<String> = active.iter().map(|s| s.service.name.clone()).collect();
    for s in &active {
        for dep in &s.depends_on {
            if !names.contains(dep) {
                return Err(SysXError::Dag(format!(
                    "service {:?} depends_on {:?} which is missing or not enabled",
                    s.service.name, dep
                )));
            }
        }
    }

    let mut dropped_total: Vec<String> = Vec::new();

    loop {
        if active.is_empty() {
            info!("SYSX_ORACLE_DAG_OK max_depth=0 services=0 dropped_cyclic={}", dropped_total.len());
            return Ok(ForgedDag {
                boot_order: vec![],
                dropped_cyclic: dropped_total,
            });
        }

        names = active.iter().map(|s| s.service.name.clone()).collect();
        for s in &mut active {
            s.depends_on.retain(|d| names.contains(d));
        }

        let graph = build_dependency_graph(&active);
        let (order, leftover) = kahn_sort_with_leftover(&graph);

        if leftover.is_empty() {
            let max_depth = max_dependency_depth(&graph)?;
            if max_depth > MAX_DAG_DEPTH {
                return Err(SysXError::Dag(format!(
                    "DAG max depth {} exceeds limit {} (16 §2)",
                    max_depth, MAX_DAG_DEPTH
                )));
            }
            info!(
                "DAG forge: {} services, max_depth={} (≤{}), dropped_cyclic={}",
                order.len(),
                max_depth,
                MAX_DAG_DEPTH,
                dropped_total.len()
            );
            info!(
                "SYSX_ORACLE_DAG_OK max_depth={} services={} dropped_cyclic={}",
                max_depth,
                order.len(),
                dropped_total.len()
            );
            info!("Topological order (forged): {:?}", order);
            if !dropped_total.is_empty() {
                error!(
                    "[SYSX] CRITICAL: CYCLIC DEPENDENCY — severed branch nodes {:?} (boot continues)",
                    dropped_total
                );
            }
            return Ok(ForgedDag {
                boot_order: order,
                dropped_cyclic: dropped_total,
            });
        }

        error!(
            "[SYSX] CRITICAL: CYCLIC DEPENDENCY — dropping blocked nodes {:?}",
            leftover
        );
        dropped_total.extend(leftover.iter().cloned());
        let drop_set: HashSet<String> = leftover.into_iter().collect();
        active.retain(|s| !drop_set.contains(&s.service.name));
    }
}

fn build_dependency_graph(schemas: &[ServiceSchema]) -> HashMap<String, Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for schema in schemas {
        graph.insert(schema.service.name.clone(), schema.depends_on.clone());
    }
    graph
}

fn build_dependency_graph_filtered(
    schemas: &[ServiceSchema],
    allowed: &HashSet<String>,
) -> HashMap<String, Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for schema in schemas {
        if !allowed.contains(&schema.service.name) {
            continue;
        }
        let deps: Vec<String> = schema
            .depends_on
            .iter()
            .filter(|d| allowed.contains(*d))
            .cloned()
            .collect();
        graph.insert(schema.service.name.clone(), deps);
    }
    graph
}

/// Reverse edges: for each **`dep`**, list services that list **`dep`** in **`depends_on`**.
pub fn build_dependents(graph: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut rev: HashMap<String, Vec<String>> = HashMap::new();
    for (node, deps) in graph {
        for dep in deps {
            rev.entry(dep.clone()).or_default().push(node.clone());
        }
    }
    rev
}

/// Kahn's algorithm: returns **`(topological_order, leftover)`** where **`leftover`** is non-empty iff a cycle remains.
fn kahn_sort_with_leftover(
    graph: &HashMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let dependents = build_dependents(graph);
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for (node, deps) in graph {
        in_degree.insert(node.clone(), deps.len());
    }

    let mut queue: VecDeque<String> = VecDeque::new();
    for (node, deg) in &in_degree {
        if *deg == 0 {
            queue.push_back(node.clone());
        }
    }

    let mut order: Vec<String> = Vec::new();
    while let Some(u) = queue.pop_front() {
        order.push(u.clone());
        if let Some(sucs) = dependents.get(&u) {
            for v in sucs {
                if let Some(d) = in_degree.get_mut(v) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(v.clone());
                    }
                }
            }
        }
    }

    if order.len() == in_degree.len() {
        return (order, vec![]);
    }

    let placed: HashSet<String> = order.iter().cloned().collect();
    let leftover: Vec<String> = graph
        .keys()
        .filter(|k| !placed.contains(*k))
        .cloned()
        .collect();
    (order, leftover)
}

/// Longest chain: **`depth(node) = 0`** if **`depends_on`** empty, else **`1 + max(depth(dep))`**.
fn max_dependency_depth(graph: &HashMap<String, Vec<String>>) -> Result<usize, SysXError> {
    let mut memo: HashMap<String, usize> = HashMap::new();
    let mut global_max: usize = 0;
    for node in graph.keys().cloned().collect::<Vec<_>>() {
        let d = depth_node(graph, &mut memo, &node)?;
        global_max = global_max.max(d);
    }
    Ok(global_max)
}

fn depth_node(
    graph: &HashMap<String, Vec<String>>,
    memo: &mut HashMap<String, usize>,
    node: &str,
) -> Result<usize, SysXError> {
    if let Some(&d) = memo.get(node) {
        return Ok(d);
    }
    let deps = graph.get(node).cloned().unwrap_or_default();
    if deps.is_empty() {
        memo.insert(node.to_string(), 0);
        return Ok(0);
    }
    let mut m = 0usize;
    for dep in &deps {
        if !graph.contains_key(dep) {
            return Err(SysXError::Dag(format!(
                "internal: missing dependency node {:?}",
                dep
            )));
        }
        m = m.max(depth_node(graph, memo, dep)?);
    }
    let d = m + 1;
    memo.insert(node.to_string(), d);
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use sysx_schema::{ServiceBlock, ServiceSchema, ServiceType};

    fn mk(name: &str, enabled: bool, depends_on: Vec<String>) -> ServiceSchema {
        ServiceSchema {
            sysx_version: 1,
            enabled,
            service: ServiceBlock {
                name: name.to_string(),
                exec: "/bin/sh".to_string(),
                args: vec![],
                env: HashMap::new(),
                user: "0".to_string(),
            },
            r#type: ServiceType::Simple,
            timeout_sec: None,
            depends_on,
            recovery: None,
            pids: None,
            memory: None,
            cpu: None,
            namespaces: None,
            capabilities: None,
            rootfs: None,
            logging: None,
        }
    }

    #[test]
    fn topo_chain_order() {
        let schemas = vec![
            mk("a", true, vec!["b".to_string()]),
            mk("b", true, vec![]),
        ];
        let forged = forge_boot_order(&schemas).unwrap();
        assert_eq!(forged.boot_order, vec!["b", "a"]);
        assert!(forged.dropped_cyclic.is_empty());
    }

    #[test]
    fn cycle_severed_not_fatal() {
        let schemas = vec![
            mk("a", true, vec!["b".to_string()]),
            mk("b", true, vec!["a".to_string()]),
        ];
        let forged = forge_boot_order(&schemas).unwrap();
        assert!(forged.boot_order.is_empty());
        assert_eq!(forged.dropped_cyclic.len(), 2);
    }

    /// Linear chain length 17: max depth 17 → fail (forged DAG without cycles).
    #[test]
    fn depth_17_fails() {
        let mut schemas = Vec::new();
        for i in 0..=17 {
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("s{}", i - 1)]
            };
            schemas.push(mk(&format!("s{}", i), true, deps));
        }
        assert!(forge_boot_order(&schemas).is_err());
    }

    /// Linear chain length 16: depths 0..=15 edges → OK.
    #[test]
    fn depth_16_ok() {
        let mut schemas = Vec::new();
        for i in 0..=16 {
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("s{}", i - 1)]
            };
            schemas.push(mk(&format!("s{}", i), true, deps));
        }
        assert!(forge_boot_order(&schemas).is_ok());
    }
}
