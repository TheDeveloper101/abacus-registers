use super::{checking, model::Peripheral};
use crate::{
    macro_input::MacroInput,
    parsing::{RegisterAttributes, StateDefinition},
};
use std::collections::BTreeSet;

const STATES: &str = "(Off, cost: 1), (On, cost: 5)";
const REGISTERS: &str = r#"struct Registers {
    #[RegAttributes([Off], StateChange(On, [Task::SET], start), cost = 3)]
    start: WriteOnly<u32, Task::Register>,
    #[RegAttributes([On], StateChange(Off, [Task::SET]), cost = 1)]
    stop: WriteOnly<u32, Task::Register>,
    #[RegAttributes([Off, On], ReadOnly, cost = 2)]
    data: ReadOnly<u32, Task::Register>,
}"#;
fn model(states: &str, registers: &str) -> syn::Result<Peripheral> {
    let input: MacroInput = syn::parse_str(&format!(
        "peripheral_name=\"Test\", register_base_addr=0, states=[{states}]"
    ))?;
    Peripheral::new(input, syn::parse_str(registers)?)
}

fn parking_model(states: &str, ready_off: usize, off_idle: usize) -> Peripheral {
    model(
        states,
        &format!(
            r#"struct Registers {{
                #[RegAttributes([Ready], StateChange(Idle, [Task::SET], idle), cost = 1)]
                idle: WriteOnly<u32, Task::Register>,
                #[RegAttributes([Ready], StateChange(Off, [Task::SET], off), cost = {ready_off})]
                off: WriteOnly<u32, Task::Register>,
                #[RegAttributes([Off], StateChange(Idle, [Task::SET], idle), cost = {off_idle})]
                wake: WriteOnly<u32, Task::Register>,
                #[RegAttributes([Idle], StateChange(Ready, [Task::SET], ready), cost = 1)]
                ready: WriteOnly<u32, Task::Register>,
                #[RegAttributes([Off, Idle, Ready], ReadOnly, cost = 0)]
                data: ReadOnly<u32, Task::Register>,
            }}"#
        ),
    )
    .unwrap()
}
fn check(source: &str) -> syn::Result<Vec<(String, usize)>> {
    checking::check(&syn::parse_str(source)?, &[model(STATES, REGISTERS)?])
}
fn cost(source: &str) -> usize {
    check(source).unwrap()[0].1
}

#[test]
fn state_cost_parses_with_and_without_transient_marker() {
    for (text, transient) in [
        ("(Off, cost: 7)", false),
        ("(On, *T*, cost: 7)", true),
        ("(Active(Rx, Tx), Both, cost: 7)", false),
    ] {
        let state: StateDefinition = syn::parse_str(text).unwrap();
        assert_eq!(state.cost, 7);
        assert_eq!(state.transient, transient);
    }
    for text in [
        "(Off)",
        "(Off, cost: -1)",
        "(Off, cost: 1, cost: 2)",
        "(On, *X*, cost: 7)",
    ] {
        assert!(syn::parse_str::<StateDefinition>(text).is_err());
    }
}
#[test]
fn operation_cost_defaults_to_zero_as_in_the_user_implementation() {
    assert_eq!(
        syn::parse_str::<RegisterAttributes>("[Off], ReadOnly")
            .unwrap()
            .cost,
        0
    );
    assert_eq!(
        syn::parse_str::<RegisterAttributes>("[Off], ReadOnly, cost = 9")
            .unwrap()
            .cost,
        9
    );
    assert_eq!(
        syn::parse_str::<RegisterAttributes>("[Off], ReadOnly, cost = read_cost")
            .unwrap()
            .cost,
        crate::parsing::Cost::Symbolic("read_cost".into())
    );
}

#[test]
fn symbolic_costs_report_state_sums_and_transitions() {
    let peripheral = model(
        "(Off, cost: off_cost), (On, cost: on_cost)",
        r#"struct Registers {
            #[RegAttributes([Off], StateChange(On, [Task::SET], start), cost = start_cost)]
            start: WriteOnly<u32, Task::Register>,
            #[RegAttributes([On], StateChange(Off, [Task::SET]), cost = stop_cost)]
            stop: WriteOnly<u32, Task::Register>,
            #[RegAttributes([On], ReadOnly, cost = read_cost)]
            data: ReadOnly<u32, Task::Register>,
        }"#,
    )
    .unwrap();
    let item = syn::parse_str(
        "fn sample(p:TestRegisters<Off>)->TestRegisters<Off>{let p=p.into_start();let _=p.data.get();p.into_off()}",
    )
    .unwrap();
    let report = checking::analyze(&item, &[peripheral]).unwrap();
    let cost = &report.reported_costs[0].1;
    assert!(cost.contains("off_cost -> on_cost"), "{cost}");
    assert!(cost.contains("on_cost -> off_cost"), "{cost}");
    assert!(cost.contains("on_cost"), "{cost}");
    assert!(cost.contains("read_cost"), "{cost}");
    assert!(report.diagnostics.is_empty());
}

#[test]
fn symbolic_branches_report_the_maximum_of_the_arm_expressions() {
    let peripheral = model("(Off, cost: off_cost), (On, cost: on_cost)", REGISTERS).unwrap();
    let item =
        syn::parse_str("fn sample(p:TestRegisters<Off>,b:bool){if b{p.data.get();}else{let _=0;}}")
            .unwrap();
    let report = checking::analyze(&item, &[peripheral]).unwrap();
    assert!(report.reported_costs[0].1.contains("max("));
}

#[test]
fn state_inference_finds_the_minimal_function_input() {
    assert_eq!(
        cost("fn sample(p:&TestRegisters){p.into_start();p.data.get();p.into_off();}"),
        11
    );
    assert_eq!(cost("fn read(p:&TestRegisters){p.data.get();}"), 7);
}

#[test]
fn state_inference_propagates_function_contracts() {
    let reports = check(
        "mod api{fn activate(p:&TestRegisters){p.into_start();}fn sample(p:&TestRegisters){activate(p);p.data.get();}}",
    )
    .unwrap();
    assert!(reports.contains(&("api::activate".into(), 3)));
    assert!(reports.contains(&("api::sample".into(), 10)));
}

#[test]
fn inferred_function_costs_are_concretized_at_call_sites() {
    let reports = check(
        "mod api{fn read(p:&TestRegisters){p.data.get();}fn caller(p:&TestRegisters<Off>){read(p);}}",
    )
    .unwrap();
    assert!(reports.contains(&("api::read".into(), 7)));
    assert!(reports.contains(&("api::caller".into(), 3)));
}

#[test]
fn state_inference_conjoins_branch_requirements() {
    let error =
        check("fn impossible(p:&TestRegisters,b:bool){if b{p.into_start();}else{p.into_off();}}")
            .unwrap_err();
    assert!(error.to_string().contains("unsatisfiable"));
}

#[test]
fn inferred_driver_fields_do_not_need_an_abacus_cell() {
    let reports = check(
        "mod api{struct Driver{regs:TestRegisters}impl Driver{fn read(&self){self.regs.into_start();self.regs.data.get();self.regs.into_off();}}}",
    )
    .unwrap();
    assert_eq!(reports, vec![("api::Driver::read".into(), 11)]);
}
#[test]
fn actual_rust_api_costs_follow_state_changes_and_rebinding() {
    assert_eq!(cost("fn sample(p:TestRegisters<Off>)->TestRegisters<Off>{let p=p.into_start();let x=p.data.get();p.into_off()}"),11);
    assert_eq!(cost("fn read(p:&TestRegisters<On>)->u32{p.data.get()}"), 7);
    assert_eq!(cost("fn cycle(p:TestRegisters<Off>)->TestRegisters<On>{let p=p.into_start();let p=p.into_off();p.into_start()}"),7);
}
#[test]
fn branch_costs_use_maximum_and_state_sets_use_union() {
    assert_eq!(cost("fn sample(p:TestRegisters<Off>,b:bool)->TestRegisters<Off>{if b{let p=p.into_start();p.into_off()}else{p}}"),5);
    assert!(check("fn bad(p:TestRegisters<Off>,b:bool){let p=if b{p.into_start().into()}else{p.into()};p.into_off();}").is_err());
}
#[test]
fn matches_refine_real_enum_variants() {
    assert_eq!(cost("fn f(p:TestStateEnum)->TestRegisters<Off>{match p{TestStateEnum::Off(p)=>p,TestStateEnum::On(p)=>p.into_off()}}"),6);
    assert!(check("fn f(p:TestStateEnum){match p{TestStateEnum::Off(p)=>{}}}").is_err());
}
#[test]
fn helper_calls_include_ambient_devices() {
    let reports=check("mod api{fn start(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()} fn sample(p:TestRegisters<Off>,q:&TestRegisters<On>)->TestRegisters<On>{start(p)}}").unwrap();
    assert!(reports.contains(&("api::sample".into(), 8)));
    assert!(reports.contains(&("api::start".into(), 3)));
}
#[test]
fn impl_methods_and_cells_use_the_same_state_machine() {
    let reports=check("mod api {struct Driver{regs:AbacusCell<TestStateEnum>} impl Driver{fn read(&self)->Option<u32>{self.regs.map(|regs|match regs{TestStateEnum::Off(p)=>{let p=p.into_start();let x=p.data.get();let p=p.into_off();(p.into(),x)},TestStateEnum::On(p)=>{let x=p.data.get();let p=p.into_off();(p.into(),x)}})}}}").unwrap();
    assert_eq!(reports, vec![("api::Driver::read".into(), 22)]);
}
#[test]
fn cell_failure_preserves_states_for_later_commands() {
    let once="cell.map(|p|match p{TestStateEnum::Off(p)=>(p.into(),()),TestStateEnum::On(p)=>{let p=p.into_off();(p.into(),())}})";
    let cost = cost(&format!(
        "fn f(cell:&AbacusCell<TestStateEnum>){{let _={once};let _={once};}}"
    ));
    assert_eq!(cost, 24);
}
#[test]
fn metadata_is_authoritative_for_costs_and_permissions() {
    let item =
        syn::parse_str("fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}").unwrap();
    let changed = model(STATES, &REGISTERS.replace("cost = 3", "cost = 9")).unwrap();
    assert_eq!(checking::check(&item, &[changed]).unwrap()[0].1, 9);
    let changed = model(
        STATES,
        &REGISTERS.replace("[Off], StateChange", "[On], StateChange"),
    )
    .unwrap();
    assert!(checking::check(&item, &[changed]).is_err());
}
#[test]
fn composite_any_patterns_preserve_substates() {
    let states = "(Active(RxIdle, TxIdle), Idle, cost: 1), (Active(RxBusy, TxIdle), Busy, cost: 5)";
    let regs="struct R{#[RegAttributes([Active(Any, TxIdle)], StateChange(Active(Any, TxIdle), [Task::SET], keep), cost = 2)]keep:WriteOnly<u32,Task::Register>}";
    let item=syn::parse_str("fn f(p:TestRegisters<Active<RxBusy,TxIdle>>)->TestRegisters<Active<RxBusy,TxIdle>>{p.into_keep()}").unwrap();
    assert_eq!(
        checking::check(&item, &[model(states, regs).unwrap()]).unwrap()[0].1,
        2
    );
}
#[test]
fn unmodeled_work_is_rejected() {
    let item =
        syn::parse_str("fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}").unwrap();
    for body in [
        "loop{}",
        "unknown();",
        "let mut x=1;x=2;",
        "assert!(true);",
        "if true{return;}p.data.get();",
    ] {
        assert!(check(&format!("fn f(p:&TestRegisters<On>){{{body}}}")).is_err());
    }
    let transient = model("(Off,cost:1),(On,*T*,cost:5)", REGISTERS).unwrap();
    assert!(checking::check(&item, &[transient]).is_err());
}
#[test]
fn inferred_bounds_cover_enumerated_concrete_executions() {
    for rounds in 1..=4 {
        let choice = "let p=if b{let p=p.into_start();p.into_off()}else{p};";
        let bound = cost(&format!(
            "fn f(p:TestRegisters<Off>,b:bool)->TestRegisters<Off>{{{}p}}",
            choice.repeat(rounds)
        ));
        for mask in 0..1 << rounds {
            let actual = 1
                + (0..rounds)
                    .map(|i| if mask & (1 << i) != 0 { 5 } else { 2 })
                    .sum::<usize>();
            assert!(bound >= actual);
        }
    }
}

#[test]
fn main_does_not_skip_unknown_constructors() {
    let error = check("fn main() { let driver = construct_driver(); }").unwrap_err();
    assert!(error.to_string().contains("unknown callee"));
}

#[test]
fn main_checks_helper_bodies_normally() {
    let reports = check(
        "mod api { fn construct_driver() { unknown_call(); } fn main() { construct_driver(); } }",
    );
    assert!(reports.unwrap_err().to_string().contains("unknown callee"));
}

#[test]
fn helper_calls_use_the_callee_scope_and_restore_the_caller_scope() {
    let reports = check(
        r#"
        mod api {
            fn leaf(p: &TestRegisters<On>) {
                let _a = p.data.get();
                let _b = p.data.get();
            }
            mod inner {
                fn leaf(p: &TestRegisters<On>) { p.data.get(); }
                fn read(p: &TestRegisters<On>) { leaf(p); }
            }
            fn run(p: &TestRegisters<On>) {
                inner::read(p);
                leaf(p);
            }
        }
    "#,
    )
    .unwrap();
    assert_eq!(
        reports
            .iter()
            .find(|(name, _)| name == "api::run")
            .unwrap()
            .1,
        21
    );
}

#[test]
fn associated_helpers_resolve_self_using_context_scope() {
    let reports = check(
        r#"
        mod api {
            struct Driver {}
            impl Driver {
                fn read(p: &TestRegisters<On>) { Self::leaf(p); }
                fn leaf(p: &TestRegisters<On>) { p.data.get(); }
            }
        }
    "#,
    )
    .unwrap();
    assert_eq!(
        reports
            .iter()
            .find(|(name, _)| name == "api::Driver::read")
            .unwrap()
            .1,
        7
    );
}

#[test]
fn block_exit_restores_bindings_but_keeps_peripheral_state_changes() {
    assert_eq!(cost("fn f(p:TestRegisters<On>) { { let (p, local) = (0, 1); let _ = p + local; } p.data.get(); }"), 17);
    assert!(check("fn f(p:TestRegisters<On>) { { let local = 1; } local; }").is_err());
    assert_eq!(cost("fn f(p:TestRegisters<Off>)->TestRegisters<Off> { let p = { let q = p.into_start(); q }; p.into_off() }"), 10);
}

#[test]
fn transitions_exclude_only_the_transitioning_peripheral_background() {
    assert_eq!(
        cost("fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}"),
        3
    );
    assert_eq!(
        cost("fn f(p:TestRegisters<Off>,q:TestRegisters<On>)->TestRegisters<On>{p.into_start()}"),
        8
    );
    assert_eq!(
        cost("fn f(p:TestRegisters<Off>,q:TestRegisters<Off>){let _=(p.into_start(),q.into_start());}"),
        6
    );
    assert_eq!(cost("fn f(p:TestRegisters<On>)->u32{p.data.get()}"), 7);
}

fn diagnostic_messages(source: &str) -> Vec<String> {
    checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[model(STATES, REGISTERS).unwrap()],
    )
    .unwrap()
    .diagnostics
    .into_iter()
    .map(|d| d.message)
    .collect()
}

fn control_flow_summary(source: &str, function: &str) -> checking::ControlFlowSummary {
    checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[model(STATES, REGISTERS).unwrap()],
    )
    .unwrap()
    .summaries
    .into_iter()
    .find(|(name, _)| name == function)
    .unwrap()
    .1
}

fn command_count(summary: &checking::ControlFlowSummary) -> usize {
    match summary {
        checking::ControlFlowSummary::Empty => 0,
        checking::ControlFlowSummary::Command(_) => 1,
        checking::ControlFlowSummary::Sequence(parts)
        | checking::ControlFlowSummary::Branch(parts) => parts.iter().map(command_count).sum(),
    }
}

#[test]
fn control_flow_summary_records_commands_backgrounds_and_peripheral_events() {
    let summary = control_flow_summary(
        "fn f(p:TestRegisters<Off>)->TestRegisters<On>{let p=p.into_start();p.data.get();p}",
        "f",
    );
    assert_eq!(summary.cost(), 15);
    let checking::ControlFlowSummary::Sequence(commands) = summary else {
        panic!("expected a linear command sequence");
    };
    assert_eq!(commands.len(), 3);

    let checking::ControlFlowSummary::Command(start) = &commands[0] else {
        panic!("expected the transition command");
    };
    assert!(start.background.is_empty());
    assert_eq!(start.events.len(), 1);
    assert!(start.events[0].transition);
    assert_eq!(start.events[0].source, BTreeSet::from(["Off".into()]));
    assert_eq!(start.events[0].target, BTreeSet::from(["On".into()]));

    let checking::ControlFlowSummary::Command(read) = &commands[1] else {
        panic!("expected the read command");
    };
    assert_eq!(read.foreground, 2);
    assert_eq!(read.background.len(), 1);
    assert_eq!(read.background[0].cost, 5);
    assert_eq!(read.events[0].operation, "data.get");
    assert!(!read.events[0].transition);
}

#[test]
fn control_flow_summary_preserves_branch_arms() {
    let summary = control_flow_summary(
        "fn f(p:TestRegisters<Off>,b:bool){if b{let p=p.into_start();p.data.get();}else{p.data.get();}}",
        "f",
    );
    assert_eq!(summary.cost(), 11);
    let checking::ControlFlowSummary::Sequence(parts) = summary else {
        panic!("expected the outer command followed by its branch");
    };
    assert_eq!(parts.len(), 2);
    let checking::ControlFlowSummary::Branch(arms) = &parts[1] else {
        panic!("expected a retained branch");
    };
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].cost(), 10);
    assert_eq!(arms[1].cost(), 3);
}

#[test]
fn checked_helper_summary_is_inlined_without_a_call_command() {
    let summary = control_flow_summary(
        "mod api{fn leaf(p:&TestRegisters<On>){p.data.get();}fn run(p:&TestRegisters<On>){leaf(p);}}",
        "api::run",
    );
    assert_eq!(summary.cost(), 7);
    assert_eq!(command_count(&summary), 1);
}

#[test]
fn one_function_summary_is_reused_at_multiple_call_sites() {
    let item = syn::parse_str(
        "mod api{fn read(p:&TestStateEnum){p.data.get();}fn run(p:&TestStateEnum){read(p);read(p);}}",
    )
    .unwrap();
    let report = checking::analyze(&item, &[model(STATES, REGISTERS).unwrap()]).unwrap();
    assert_eq!(report.diagnostics.len(), 1);
    let run = report
        .summaries
        .iter()
        .find(|(name, _)| name == "api::run")
        .unwrap();
    assert_eq!(command_count(&run.1), 2);
    assert_eq!(run.1.cost(), 14);
}

#[test]
fn burn_analysis_suggests_powering_down_for_a_long_unused_interval() {
    let source = "fn f(p:TestRegisters<Ready>)->TestRegisters<Ready>{let p=p.into_idle();let _=0;let _=0;let _=0;p.into_ready()}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:5),(Ready,cost:7)",
            2,
            2,
        )],
    )
    .unwrap();
    let burns = report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("burn bug"))
        .collect::<Vec<_>>();
    assert_eq!(burns.len(), 1);
    assert!(burns[0].message.contains("parked in Idle"));
    assert!(burns[0].message.contains("parking in Off"));
    assert!(burns[0].message.contains("costs 17"));
    assert!(burns[0].message.contains("costs 5"));
    assert!(burns[0].message.contains("saving 12"));
}

#[test]
fn burn_analysis_suggests_staying_on_when_off_transitions_cost_more() {
    let source = "fn f(p:TestRegisters<Ready>)->TestRegisters<Ready>{let p=p.into_off();let _=0;let p=p.into_idle();p.into_ready()}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:1),(Ready,cost:7)",
            5,
            5,
        )],
    )
    .unwrap();
    let burn = report
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("burn bug"))
        .unwrap();
    assert!(burn.message.contains("parked in Off"));
    assert!(burn.message.contains("parking in Idle"));
    assert!(burn.message.contains("costs 11"));
    assert!(burn.message.contains("costs 3"));
}

#[test]
fn burn_analysis_ignores_intervals_that_use_the_peripheral() {
    let source = "fn f(p:TestRegisters<Ready>)->TestRegisters<Ready>{let p=p.into_off();p.data.get();let p=p.into_idle();p.into_ready()}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:1),(Ready,cost:7)",
            5,
            5,
        )],
    )
    .unwrap();
    assert!(report
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("burn bug")));
}

#[test]
fn burn_analysis_is_silent_when_the_written_strategy_is_cheapest() {
    let source = "fn f(p:TestRegisters<Ready>)->TestRegisters<Ready>{let p=p.into_off();let _=0;let _=0;let _=0;let p=p.into_idle();p.into_ready()}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:5),(Ready,cost:7)",
            2,
            2,
        )],
    )
    .unwrap();
    assert!(report
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("burn bug")));
}

#[test]
fn burn_analysis_uses_the_maximum_branch_cost() {
    let source = "fn f(p:TestRegisters<Ready>,b:bool)->TestRegisters<Ready>{let p=p.into_idle();if b{let _=0;let _=0;let _=0;}else{let _=0;}p.into_ready()}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:5),(Ready,cost:7)",
            2,
            2,
        )],
    )
    .unwrap();
    let burn = report
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("burn bug"))
        .unwrap();
    assert!(burn.message.contains("costs 22"));
    assert!(burn.message.contains("costs 5"));
}

#[test]
fn burn_analysis_sees_unused_commands_inside_a_function_call() {
    let source = "mod api{fn compute(){let _=0;let _=0;let _=0;}fn f(p:TestRegisters<Ready>)->TestRegisters<Ready>{let p=p.into_idle();compute();p.into_ready()}}";
    let report = checking::analyze(
        &syn::parse_str(source).unwrap(),
        &[parking_model(
            "(Off,cost:0),(Idle,cost:5),(Ready,cost:7)",
            2,
            2,
        )],
    )
    .unwrap();
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("burn bug")));
}

#[test]
fn operation_reports_a_cheaper_possible_state() {
    let source = "fn f(p:TestRegisters<Off>, b:bool) { let p = if b { p.into_start().into() } else { p.into() }; p.data.get(); }";
    let messages = diagnostic_messages(source);
    assert_eq!(messages.len(), 1);
    let operation = &messages[0];
    assert!(operation.contains("Possible states reaching this operation: Off (1), On (5)"));
    assert!(operation.contains("valid in every possible state"));
    assert!(operation.contains("lower background cost by 4"));

    assert!(diagnostic_messages("fn f(p:TestRegisters<Off>) { p.data.get(); }").is_empty());
    assert!(diagnostic_messages("fn f(p:TestRegisters<On>) { p.data.get(); }").is_empty());
}

#[test]
fn a_branch_without_a_common_operation_is_not_reported() {
    let source =
        "fn f(p:TestRegisters<Off>, b:bool) { let p = if b { p.into_start() } else { p }; }";
    assert!(diagnostic_messages(source).is_empty());
}

#[test]
fn equal_cost_states_do_not_produce_optimization_diagnostics() {
    let report = checking::analyze(
        &syn::parse_str("fn f(p:TestRegisters<Off>, b:bool) { let p = if b { p.into_start() } else { p }; p.data.get(); }").unwrap(),
        &[model("(Off,cost:5),(On,cost:5)", REGISTERS).unwrap()],
    )
    .unwrap();
    assert!(report.diagnostics.is_empty());
}

#[test]
fn a_branch_that_rejoins_in_the_same_state_is_not_reported() {
    let source = "fn f(p:TestRegisters<Off>, b:bool) { let p = if b { let p = p.into_start(); p.into_off() } else { p }; }";
    assert!(diagnostic_messages(source).is_empty());
}
