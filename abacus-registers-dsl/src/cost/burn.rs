//! Compare state-management strategies across regions that do not use a peripheral.
use super::{
    checking::{CommandSummary, ControlFlowSummary, FunctionAnalysis},
    diagnostics::Diagnostic,
    model::Peripheral,
};
use crate::rational::Rational;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct Path {
    cost: Rational,
    operations: Vec<String>,
}

struct Transition<'a> {
    source: &'a str,
    target: &'a str,
    operation: &'a str,
    cost: Rational,
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
        ControlFlowSummary::Scale(_, summary) => {
            inspect(function, summary, device, model, diagnostics);
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
    inspect_transition_boundaries(function, parts, device, model, diagnostics);

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
        if entry_end == exit_start {
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
        let middle_uses_device = has_device_event(&middle, device);
        let written =
            ControlFlowSummary::Sequence(parts[entry_start..exit_end].to_vec()).scaled_cost();
        let entry_other_background =
            other_background(command(&parts[entry_start]).unwrap(), device);
        let exit_other_background = other_background(command(&parts[exit_start]).unwrap(), device);

        let mut best: Option<(String, Path, Path, Rational)> = None;
        for (candidate, state) in &model.states {
            if state.transient {
                continue;
            }
            // Replaying operations is used to prove that teardown/setup can be
            // removed while retaining the original active state. Choosing a
            // different operating state remains an unused-interval analysis.
            if middle_uses_device && candidate != source {
                continue;
            }
            let Some(before) = shortest_path(
                model,
                source,
                candidate,
                command(&parts[entry_start]).unwrap().speed(),
                entry_other_background,
            ) else {
                continue;
            };
            let Some(after) = shortest_path(
                model,
                candidate,
                required,
                command(&parts[exit_start]).unwrap().speed(),
                exit_other_background,
            ) else {
                continue;
            };
            let Some((middle_cost, middle_states)) =
                alternative_cost(&middle, device, model, BTreeSet::from([candidate.clone()]))
            else {
                continue;
            };
            if middle_states != BTreeSet::from([candidate.clone()]) {
                continue;
            }
            let total = before.cost + middle_cost + after.cost;
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
        if candidate == source
            && required == source
            && alternative_operations.is_empty()
            && model.states[source].cost.concrete().unwrap()
                > model.states[parked].cost.concrete().unwrap()
        {
            diagnostics.push(Diagnostic {
                span: first.span,
                message: format!(
                    "Heliod: {function} contains a batching opportunity for {}. The peripheral is torn down to {parked} and restored to {source} without being used in between. Removing [{written_operations}] and keeping it in {source} costs {alternative} instead of {written}, saving {}. The useful operations on either side may be batched into one active session.",
                    model.registers,
                    written - alternative
                ),
            });
            continue;
        }
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

fn inspect_transition_boundaries(
    function: &str,
    parts: &[ControlFlowSummary],
    device: usize,
    model: &Peripheral,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (index, part) in parts.iter().enumerate() {
        let Some(boundary) = transition(part, device) else {
            continue;
        };
        let (Some(source), Some(target)) =
            (singleton(&boundary.source), singleton(&boundary.target))
        else {
            continue;
        };
        let source_cost = model.states[source].cost.concrete().unwrap();
        let target_cost = model.states[target].cost.concrete().unwrap();

        if target_cost > source_cost {
            let Some(use_index) = parts[index + 1..]
                .iter()
                .position(|part| has_device_event(part, device))
                .map(|offset| index + 1 + offset)
            else {
                continue;
            };
            if use_index == index + 1 || !is_nontransition_use(&parts[use_index], device) {
                continue;
            }
            let gap = ControlFlowSummary::Sequence(parts[index + 1..use_index].to_vec());
            let Some((alternative_gap, output)) =
                alternative_cost(&gap, device, model, BTreeSet::from([source.to_owned()]))
            else {
                continue;
            };
            if output != BTreeSet::from([source.to_owned()]) {
                continue;
            }
            let Some(next_command) = command(&parts[use_index]) else {
                continue;
            };
            let written = part.scaled_cost() + gap.scaled_cost();
            let moved = next_command.scale_cost(boundary.cost)
                + other_background(next_command, device)
                + alternative_gap;
            if moved < written {
                diagnostics.push(Diagnostic {
                    span: boundary.span,
                    message: format!(
                        "Heliod: {function} enables {} too early with `{}`. Moving the transition from {source} to {target} past {} independent command(s), immediately before the first peripheral use, costs {moved} instead of {written}, saving {}.",
                        model.registers,
                        boundary.operation,
                        command_count(&gap),
                        written - moved
                    ),
                });
            }
        }

        if source_cost > target_cost {
            let Some(use_index) = parts[..index]
                .iter()
                .rposition(|part| has_device_event(part, device))
            else {
                continue;
            };
            if use_index + 1 == index || !is_nontransition_use(&parts[use_index], device) {
                continue;
            }
            let gap = ControlFlowSummary::Sequence(parts[use_index + 1..index].to_vec());
            let Some((alternative_gap, output)) =
                alternative_cost(&gap, device, model, BTreeSet::from([target.to_owned()]))
            else {
                continue;
            };
            if output != BTreeSet::from([target.to_owned()]) {
                continue;
            }
            let Some(first_gap_command) = first_command(&gap) else {
                continue;
            };
            let written = gap.scaled_cost() + part.scaled_cost();
            let moved = first_gap_command.scale_cost(boundary.cost)
                + other_background(first_gap_command, device)
                + alternative_gap;
            if moved < written {
                diagnostics.push(Diagnostic {
                    span: boundary.span,
                    message: format!(
                        "Heliod: {function} disables {} too late with `{}`. Moving the transition from {source} to {target} before {} independent command(s), immediately after the last peripheral use, costs {moved} instead of {written}, saving {}.",
                        model.registers,
                        boundary.operation,
                        command_count(&gap),
                        written - moved
                    ),
                });
            }
        }
    }
}

fn command(summary: &ControlFlowSummary) -> Option<&CommandSummary> {
    if let ControlFlowSummary::Command(command) = summary {
        Some(command)
    } else {
        None
    }
}

fn first_command(summary: &ControlFlowSummary) -> Option<&CommandSummary> {
    match summary {
        ControlFlowSummary::Command(command) => Some(command),
        ControlFlowSummary::Sequence(parts) => parts.iter().find_map(first_command),
        ControlFlowSummary::Scale(_, summary) => first_command(summary),
        ControlFlowSummary::Empty | ControlFlowSummary::Branch(_) => None,
    }
}

fn is_nontransition_use(summary: &ControlFlowSummary, device: usize) -> bool {
    let Some(command) = command(summary) else {
        return false;
    };
    let events = command
        .events
        .iter()
        .filter(|event| event.device == device)
        .collect::<Vec<_>>();
    !events.is_empty() && events.iter().all(|event| !event.transition)
}

fn command_count(summary: &ControlFlowSummary) -> usize {
    match summary {
        ControlFlowSummary::Empty => 0,
        ControlFlowSummary::Command(_) => 1,
        ControlFlowSummary::Sequence(parts) => parts.iter().map(command_count).sum(),
        ControlFlowSummary::Branch(arms) => arms.iter().map(command_count).max().unwrap_or(0),
        ControlFlowSummary::Scale(factor, summary) => factor * command_count(summary),
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

fn alternative_cost(
    summary: &ControlFlowSummary,
    device: usize,
    model: &Peripheral,
    states: BTreeSet<String>,
) -> Option<(Rational, BTreeSet<String>)> {
    match summary {
        ControlFlowSummary::Empty => Some((Rational::ONE.scale(0), states)),
        ControlFlowSummary::Command(command) => {
            let mut foreground = command.adjusted_foreground(command.speed());
            let background_states = states.clone();
            let mut next = states;
            for event in command.events.iter().filter(|event| event.device == device) {
                if event.transition {
                    return None;
                }
                foreground = foreground.checked_sub(event.cost)?;
                let mut targets = BTreeSet::new();
                let mut operation_cost = 0;
                for state in &next {
                    let operation = model
                        .operations
                        .get(&(state.clone(), event.operation.clone()))?;
                    if operation.transition {
                        return None;
                    }
                    targets.insert(operation.target.clone());
                    operation_cost = operation_cost.max(operation.cost.concrete()?);
                }
                foreground += operation_cost;
                next = targets;
            }
            let background = command
                .background
                .iter()
                .filter(|charge| charge.device != device)
                .map(|charge| charge.cost)
                .sum::<usize>()
                + background_states
                    .iter()
                    .filter_map(|state| model.states[state].cost.concrete())
                    .max()
                    .unwrap_or(0);
            Some((command.scale_cost(foreground + background), next))
        }
        ControlFlowSummary::Sequence(parts) => {
            let mut cost = Rational::ONE.scale(0);
            let mut current = states;
            for part in parts {
                let (part_cost, output) = alternative_cost(part, device, model, current)?;
                cost += part_cost;
                current = output;
            }
            Some((cost, current))
        }
        ControlFlowSummary::Branch(arms) => {
            let mut cost = Rational::ONE.scale(0);
            let mut outputs = BTreeSet::new();
            for arm in arms {
                let (arm_cost, output) = alternative_cost(arm, device, model, states.clone())?;
                cost = cost.max(arm_cost);
                outputs.extend(output);
            }
            Some((cost, outputs))
        }
        ControlFlowSummary::Scale(factor, summary) => {
            let (cost, output) = alternative_cost(summary, device, model, states)?;
            Some((cost.multiply_usize(*factor), output))
        }
    }
}

fn has_device_event(summary: &ControlFlowSummary, device: usize) -> bool {
    match summary {
        ControlFlowSummary::Empty => false,
        ControlFlowSummary::Command(command) => {
            command.events.iter().any(|event| event.device == device)
        }
        ControlFlowSummary::Sequence(parts) | ControlFlowSummary::Branch(parts) => {
            parts.iter().any(|part| has_device_event(part, device))
        }
        ControlFlowSummary::Scale(_, summary) => has_device_event(summary, device),
    }
}

fn other_background(command: &CommandSummary, device: usize) -> Rational {
    command.scale_cost(
        command
            .background
            .iter()
            .filter(|charge| charge.device != device)
            .map(|charge| charge.cost)
            .sum(),
    )
}

fn shortest_path(
    model: &Peripheral,
    source: &str,
    target: &str,
    speed: Rational,
    other_background: Rational,
) -> Option<Path> {
    let edges = model
        .operations
        .iter()
        .filter(|(_, operation)| operation.transition)
        .map(|((source, name), operation)| Transition {
            source,
            target: &operation.target,
            operation: name,
            cost: speed.scale(operation.cost.concrete().unwrap()) + other_background,
        })
        .collect::<Vec<_>>();
    let mut paths = BTreeMap::from([(
        source.to_owned(),
        Path {
            cost: Rational::ONE.scale(0),
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
