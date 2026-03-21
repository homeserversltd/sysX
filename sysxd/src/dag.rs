//! Pre-reactor DAG validation — Kahn topological sort, cycle detection, max depth 16 (`15`, `16` §2).

use std::collections::{HashMap, HashSet, VecDeque};

use log::info;

use sysx_schema::ServiceSchema;

use crate::SysXError;

/// Maximum allowed dependency chain depth (`16` §2, `15`).
const MAX_DAG_DEPTH: usize = 16;

/// Validate enabled services only: dependency closure, topological order, depth ≤ 16.
pub fn validate(schemas: &[ServiceSchema]) -> Result<(), SysXError> {
    let active: Vec<ServiceSchema> = schemas.iter().filter(|s| s.enabled).cloned().collect();

    if active.is_empty() {
        info!("No enabled services — empty DAG is valid (15)");
        info!("SYSX_ORACLE_DAG_OK max_depth=0 services=0");
        return Ok(());
    }

    let names: HashSet<String> = active.iter().map(|s| s.service.name.clone()).collect();
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

    let graph = build_dependency_graph(&active);
    let order = topological_sort(&graph)?;
    let max_depth = max_dependency_depth(&graph)?;

    if max_depth > MAX_DAG_DEPTH {
        return Err(SysXError::Dag(format!(
            "DAG max depth {} exceeds limit {} (16 §2)",
            max_depth, MAX_DAG_DEPTH
        )));
    }

    info!(
        "DAG validation passed: {} services, max_depth={} (≤{})",
        order.len(),
        max_depth,
        MAX_DAG_DEPTH
    );
    info!(
        "SYSX_ORACLE_DAG_OK max_depth={} services={}",
        max_depth,
        order.len()
    );
    info!("Topological order: {:?}", order);
    Ok(())
}

fn build_dependency_graph(schemas: &[ServiceSchema]) -> HashMap<String, Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for schema in schemas {
        graph.insert(schema.service.name.clone(), schema.depends_on.clone());
    }
    graph
}

/// Reverse edges: for each **`dep`**, list services that list **`dep`** in **`depends_on`**.
fn build_dependents(graph: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut rev: HashMap<String, Vec<String>> = HashMap::new();
    for (node, deps) in graph {
        for dep in deps {
            rev.entry(dep.clone()).or_default().push(node.clone());
        }
    }
    rev
}

fn topological_sort(graph: &HashMap<String, Vec<String>>) -> Result<Vec<String>, SysXError> {
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

    if order.len() != in_degree.len() {
        return Err(SysXError::Dag(
            "Cycle detected in service depends_on graph".to_string(),
        ));
    }

    Ok(order)
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
        let active: Vec<_> = schemas.iter().filter(|s| s.enabled).cloned().collect();
        let g = build_dependency_graph(&active);
        let o = topological_sort(&g).unwrap();
        assert_eq!(o, vec!["b", "a"]);
    }

    #[test]
    fn cycle_fails() {
        let schemas = vec![
            mk("a", true, vec!["b".to_string()]),
            mk("b", true, vec!["a".to_string()]),
        ];
        let active: Vec<_> = schemas.iter().filter(|s| s.enabled).cloned().collect();
        let g = build_dependency_graph(&active);
        assert!(topological_sort(&g).is_err());
    }

    /// Linear chain length 17: depths 0..=16 → OK (`MAX_DAG_DEPTH` = 16).
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
        assert!(validate(&schemas).is_ok());
    }

    /// Linear chain length 18: max depth 17 → fail.
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
        assert!(validate(&schemas).is_err());
    }
}
