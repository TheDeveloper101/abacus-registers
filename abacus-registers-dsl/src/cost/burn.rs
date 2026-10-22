//! Compare state-management strategies across regions that do not use a peripheral.
use super::{
    checking::{CommandSummary, ControlFlowSummary, FunctionAnalysis},
    diagnostics::Diagnostic,
    model::Peripheral,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct Path {
    cost: usize,
    operations: Vec<String>,
}

struct Transition<'a> {
    source: &'a str,
    target: &'a str,
    operation: &'a str,
    cost: usize,
}

pub fn analyze(
    function: &str,
    analysis: &FunctionAnalysis,
    models: &[Peripheral],
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (device, info) in analysis.initial_devices.iter().enumerate() {
        inspect(
            function,
            &analysis.summary,
            device,
            &models[info.model],
            &mut diagnostics,
        );
    }
    diagnostics
}

fn inspect(
    function: &str,
    summary: &ControlFlowSummary,
    device: usize,
    model: &Peripheral,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match summary {
        ControlFlowSummary::Sequence(parts) => {
            inspect_sequence(function, parts, device, model, diagnostics);
            for part in parts {
                inspect(function, part, device, model, diagnostics);
            }
        }
        ControlFlowSummary::Branch(arms) => {
            for arm in arms {
                inspect(function, arm, device, model, diagnostics);
            }
        }
        ControlFlowSummary::Empty | ControlFlowSummary::Command(_) => {}
    }
}

fn inspect_sequence(
    function: &str,
    parts: &[ControlFlowSummary],
    device: usize,
    model: &Peripheral,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut clusters = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        if transition(&parts[index], device).is_none() {
            index += 1;
            continue;
        }
        let start = index;
        while index < parts.len() && transition(&parts[index], device).is_some() {
            index += 1;
        }
        clusters.push((start, index));
    }

    for pair in clusters.windows(2) {
        let (entry_start, entry_end) = pair[0];
        let (exit_start, exit_end) = pair[1];
        if entry_end == exit_start
            || parts[entry_end..exit_start]
                .iter()
                .any(|part| uses_device(part, device))
        {
            continue;
        }
        let entry = transitions(&parts[entry_start..entry_end], device);
        let exit = transitions(&parts[exit_start..exit_end], device);
        let Some(first) = entry.first() else {
            continue;
        };
        let Some(source) = singleton(&first.source) else {
            continue;
        };
        let Some(parked) = entry.last().and_then(|event| singleton(&event.target)) else {
            continue;
        };
        let Some(required) = exit.last().and_then(|event| singleton(&event.target)) else {
            continue;
        };
        if entry
            .windows(2)
            .chain(exit.windows(2))
            .any(|events| events[0].target != events[1].source)
            || exit
                .first()
                .map_or(true, |event| event.source != entry.last().unwrap().target)
        {
            continue;
        }

        let middle = ControlFlowSummary::Sequence(parts[entry_end..exit_start].to_vec());
        let written = ControlFlowSummary::Sequence(parts[entry_start..exit_end].to_vec()).cost();
        let entry_other_background =
            other_background(command(&parts[entry_start]).unwrap(), device);
        let exit_other_background = other_background(command(&parts[exit_start]).unwrap(), device);

        let mut best: Option<(String, Path, Path, usize)> = None;
        for (candidate, state) in &model.states {
            if state.transient {
                continue;
            }
            let Some(before) = shortest_path(model, source, candidate, entry_other_background)
            else {
                continue;
            };
            let Some(after) = shortest_path(model, candidate, required, exit_other_background)
            else {
                continue;
            };
            let total = before.cost
                + parked_cost(&middle, device, state.cost.concrete().unwrap())
                + after.cost;
            if total < written
                && best
                    .as_ref()
                    .map_or(true, |(_, _, _, best_cost)| total < *best_cost)
            {
                best = Some((candidate.clone(), before, after, total));
            }
        }

        let Some((candidate, before, after, alternative)) = best else {
            continue;
        };
        let written_operations = entry
            .iter()
            .chain(&exit)
            .map(|event| event.operation.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        let alternative_operations = before
            .operations
            .iter()
            .chain(&after.operations)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" -> ");
        diagnostics.push(Diagnostic {
            span: first.span,
            message: format!(
                "Heliod: {function} contains a burn bug for {}. The peripheral is unused between transitions while parked in {parked}. Current strategy [{written_operations}] costs {written}; parking in {candidate} with transitions [{alternative_operations}] costs {alternative}, saving {}.",
                model.registers,
                written - alternative
            ),
        });
    }
}

fn command(summary: &ControlFlowSummary) -> Option<&CommandSummary> {
    if let ControlFlowSummary::Command(command) = summary {
        Some(command)
    } else {
        None
    }
}

fn transition(
    summary: &ControlFlowSummary,
    device: usize,
) -> Option<&super::checking::PeripheralEvent> {
    let command = command(summary)?;
    if command.events.len() != 1 {
        return None;
    }
    let event = &command.events[0];
    (event.device == device && event.transition).then_some(event)
}

fn transitions(
    summaries: &[ControlFlowSummary],
    device: usize,
) -> Vec<&super::checking::PeripheralEvent> {
    summaries
        .iter()
        .filter_map(|summary| transition(summary, device))
        .collect()
}

fn singleton(states: &BTreeSet<String>) -> Option<&str> {
    (states.len() == 1).then(|| states.iter().next().unwrap().as_str())
}

fn uses_device(summary: &ControlFlowSummary, device: usize) -> bool {
    match summary {
        ControlFlowSummary::Empty => false,
        ControlFlowSummary::Command(command) => {
            command.events.iter().any(|event| event.device == device)
        }
        ControlFlowSummary::Sequence(parts) | ControlFlowSummary::Branch(parts) => {
            parts.iter().any(|part| uses_device(part, device))
        }
    }
}

fn other_background(command: &CommandSummary, device: usize) -> usize {
    command
        .background
        .iter()
        .filter(|charge| charge.device != device)
        .map(|charge| charge.cost)
        .sum()
}

fn parked_cost(summary: &ControlFlowSummary, device: usize, background: usize) -> usize {
    match summary {
        ControlFlowSummary::Empty => 0,
        ControlFlowSummary::Command(command) => {
            command.foreground
                + command
                    .background
                    .iter()
                    .filter(|charge| charge.device != device)
                    .map(|charge| charge.cost)
                    .sum::<usize>()
                + background
        }
        ControlFlowSummary::Sequence(parts) => parts
            .iter()
            .map(|part| parked_cost(part, device, background))
            .sum(),
        ControlFlowSummary::Branch(arms) => arms
            .iter()
            .map(|arm| parked_cost(arm, device, background))
            .max()
            .unwrap_or(0),
    }
}

fn shortest_path(
    model: &Peripheral,
    source: &str,
    target: &str,
    other_background: usize,
) -> Option<Path> {
    let edges = model
        .operations
        .iter()
        .filter(|(_, operation)| operation.transition)
        .map(|((source, name), operation)| Transition {
            source,
            target: &operation.target,
            operation: name,
            cost: operation.cost.concrete().unwrap() + other_background,
        })
        .collect::<Vec<_>>();
    let mut paths = BTreeMap::from([(
        source.to_owned(),
        Path {
            cost: 0,
            operations: Vec::new(),
        },
    )]);
    let mut visited = BTreeSet::new();
    loop {
        let next = paths
            .iter()
            .filter(|(state, _)| !visited.contains(*state))
            .min_by_key(|(_, path)| path.cost)
            .map(|(state, path)| (state.clone(), path.clone()))?;
        if next.0 == target {
            return Some(next.1);
        }
        visited.insert(next.0.clone());
        for edge in edges.iter().filter(|edge| edge.source == next.0) {
            let cost = next.1.cost + edge.cost;
            if paths
                .get(edge.target)
                .is_some_and(|existing| existing.cost <= cost)
            {
                continue;
            }
            let mut operations = next.1.operations.clone();
            operations.push(edge.operation.to_owned());
            paths.insert(edge.target.to_owned(), Path { cost, operations });
        }
    }
}
