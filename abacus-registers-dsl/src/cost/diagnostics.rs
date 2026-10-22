//! Simple explanations of expensive states already found by the checker.
use proc_macro2::{Span, TokenStream};
use quote::quote_spanned;
use std::collections::BTreeSet;

pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

pub fn operation(
    function: &str,
    span: Span,
    operation: &str,
    peripheral: &str,
    current_states: &[(String, usize)],
) -> Option<Diagnostic> {
    if current_states.len() < 2 {
        return None;
    }
    let cheapest = current_states.iter().map(|(_, cost)| *cost).min()?;
    let current_cost = current_states.iter().map(|(_, cost)| *cost).max()?;
    if current_cost <= cheapest {
        return None;
    }

    let possible = current_states
        .iter()
        .map(|(state, cost)| format!("{state} ({cost})"))
        .collect::<Vec<_>>()
        .join(", ");
    let cheaper_states = current_states
        .iter()
        .filter(|(_, cost)| *cost == cheapest)
        .map(|(state, _)| state.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    Some(Diagnostic {
        span,
        message: format!(
            "Heliod: `{operation}` in {function} may occur while {peripheral} is in an unnecessarily expensive state. Possible states reaching this operation: {possible}. The operation is valid in every possible state, including cheaper state {cheaper_states}. Choosing only the cheaper state could lower background cost by {} for this command.",
            current_cost - cheapest
        ),
    })
}

pub fn warnings(diagnostics: Vec<Diagnostic>) -> TokenStream {
    let mut seen = BTreeSet::new();
    diagnostics
        .into_iter()
        .filter_map(|diagnostic| {
            if !seen.insert((diagnostic.message.clone(), format!("{:?}", diagnostic.span))) {
                return None;
            }
            let span = diagnostic.span;
            let message = diagnostic.message;
            Some(quote_spanned! {span=>
                const _: () = {
                    #[deprecated(note = #message)]
                    const HELIOD_DIAGNOSTIC: () = ();
                    let _ = HELIOD_DIAGNOSTIC;
                };
            })
        })
        .collect()
}
