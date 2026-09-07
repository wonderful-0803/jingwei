use std::collections::{BTreeMap, BTreeSet};

use crate::{PluginDescriptor, PluginError, PluginId};

pub(super) fn validate_duplicate_plugins(
    descriptors: &[PluginDescriptor],
) -> Result<(), PluginError> {
    let mut occurrences = BTreeMap::<PluginId, usize>::new();
    for descriptor in descriptors {
        *occurrences.entry(descriptor.id()).or_default() += 1;
    }

    if let Some((plugin, _)) = occurrences.iter().find(|(_, count)| **count > 1) {
        return Err(PluginError::DuplicatePlugin { plugin: *plugin });
    }
    Ok(())
}

pub(super) fn validate_missing_dependencies(
    descriptors: &[PluginDescriptor],
) -> Result<(), PluginError> {
    let known = descriptors
        .iter()
        .map(|descriptor| descriptor.id())
        .collect::<BTreeSet<_>>();
    let mut missing = BTreeSet::new();
    for descriptor in descriptors {
        for dependency in descriptor.plugin_dependencies() {
            if !known.contains(dependency) {
                missing.insert((descriptor.id(), *dependency));
            }
        }
    }
    if let Some((plugin, dependency)) = missing.first().copied() {
        return Err(PluginError::MissingDependency { plugin, dependency });
    }
    Ok(())
}

pub(super) fn resolve_exact(descriptors: &[PluginDescriptor]) -> Result<Vec<usize>, PluginError> {
    let indices = descriptors
        .iter()
        .enumerate()
        .map(|(index, descriptor)| (descriptor.id(), vec![index]))
        .collect::<BTreeMap<_, _>>();
    let dependencies_by_plugin = descriptors
        .iter()
        .map(|descriptor| {
            (
                descriptor.id(),
                descriptor
                    .plugin_dependencies()
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    resolve_order(descriptors.len(), &indices, &dependencies_by_plugin)
}

pub(super) fn resolve_active(
    descriptors: &[PluginDescriptor],
    active: &BTreeSet<PluginId>,
    additional_dependencies: &BTreeMap<PluginId, BTreeSet<PluginId>>,
) -> Result<Vec<usize>, PluginError> {
    let indices = descriptors
        .iter()
        .enumerate()
        .filter_map(|(index, descriptor)| {
            active
                .contains(&descriptor.id())
                .then_some((descriptor.id(), vec![index]))
        })
        .collect::<BTreeMap<_, _>>();
    let dependencies_by_plugin = descriptors
        .iter()
        .filter(|descriptor| active.contains(&descriptor.id()))
        .map(|descriptor| {
            let mut dependencies = descriptor
                .plugin_dependencies()
                .iter()
                .copied()
                .filter(|dependency| active.contains(dependency))
                .collect::<BTreeSet<_>>();
            if let Some(additional) = additional_dependencies.get(&descriptor.id()) {
                dependencies.extend(additional);
            }
            (descriptor.id(), dependencies)
        })
        .collect::<BTreeMap<_, _>>();

    resolve_order(active.len(), &indices, &dependencies_by_plugin)
}

fn resolve_order(
    node_count: usize,
    indices: &BTreeMap<PluginId, Vec<usize>>,
    dependencies_by_plugin: &BTreeMap<PluginId, BTreeSet<PluginId>>,
) -> Result<Vec<usize>, PluginError> {
    let mut indegrees = BTreeMap::<PluginId, usize>::new();
    let mut dependents = BTreeMap::<PluginId, BTreeSet<PluginId>>::new();
    for (plugin, dependencies) in dependencies_by_plugin {
        indegrees.insert(*plugin, dependencies.len());
        for dependency in dependencies {
            dependents.entry(*dependency).or_default().insert(*plugin);
        }
    }

    let mut ready = indegrees
        .iter()
        .filter_map(|(plugin, indegree)| (*indegree == 0).then_some(*plugin))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(node_count);

    while let Some(plugin) = ready.pop_first() {
        let index = indices
            .get(&plugin)
            .and_then(|entries| entries.first())
            .copied()
            .expect("every resolved plugin id must have one submitted plugin");
        order.push(index);

        if let Some(entries) = dependents.get(&plugin) {
            for dependent in entries {
                let indegree = indegrees
                    .get_mut(dependent)
                    .expect("every dependent must have an indegree");
                *indegree -= 1;
                if *indegree == 0 {
                    ready.insert(*dependent);
                }
            }
        }
    }

    if order.len() != node_count {
        let plugins = smallest_cyclic_component(dependencies_by_plugin)
            .expect("an incomplete topological order must contain a cyclic component");
        return Err(PluginError::DependencyCycle { plugins });
    }

    Ok(order)
}

fn smallest_cyclic_component(
    dependencies: &BTreeMap<PluginId, BTreeSet<PluginId>>,
) -> Option<Vec<PluginId>> {
    let mut visited = BTreeSet::new();
    let mut finish_order = Vec::with_capacity(dependencies.len());
    for plugin in dependencies.keys().copied() {
        visit_dependencies(plugin, dependencies, &mut visited, &mut finish_order);
    }

    let mut reverse = dependencies
        .keys()
        .copied()
        .map(|plugin| (plugin, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for (plugin, required) in dependencies {
        for dependency in required {
            reverse
                .get_mut(dependency)
                .expect("every dependency was validated before cycle detection")
                .insert(*plugin);
        }
    }

    visited.clear();
    let mut cyclic_components = Vec::new();
    for plugin in finish_order.into_iter().rev() {
        if visited.contains(&plugin) {
            continue;
        }
        let mut component = Vec::new();
        collect_component(plugin, &reverse, &mut visited, &mut component);
        component.sort_unstable();

        let is_self_cycle = component.len() == 1
            && dependencies
                .get(&component[0])
                .is_some_and(|required| required.contains(&component[0]));
        if component.len() > 1 || is_self_cycle {
            cyclic_components.push(component);
        }
    }

    cyclic_components.into_iter().min()
}

fn visit_dependencies(
    plugin: PluginId,
    dependencies: &BTreeMap<PluginId, BTreeSet<PluginId>>,
    visited: &mut BTreeSet<PluginId>,
    finish_order: &mut Vec<PluginId>,
) {
    if !visited.insert(plugin) {
        return;
    }
    if let Some(required) = dependencies.get(&plugin) {
        for dependency in required {
            visit_dependencies(*dependency, dependencies, visited, finish_order);
        }
    }
    finish_order.push(plugin);
}

fn collect_component(
    plugin: PluginId,
    reverse: &BTreeMap<PluginId, BTreeSet<PluginId>>,
    visited: &mut BTreeSet<PluginId>,
    component: &mut Vec<PluginId>,
) {
    if !visited.insert(plugin) {
        return;
    }
    component.push(plugin);
    if let Some(dependents) = reverse.get(&plugin) {
        for dependent in dependents {
            collect_component(*dependent, reverse, visited, component);
        }
    }
}
