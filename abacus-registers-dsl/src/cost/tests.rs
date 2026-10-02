use super::{checking, model::Peripheral};
use crate::{
    macro_input::MacroInput,
    parsing::{RegisterAttributes, StateDefinition},
};

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
    assert!(syn::parse_str::<RegisterAttributes>("[Off], ReadOnly, cost = nope").is_err());
}
#[test]
fn actual_rust_api_costs_follow_state_changes_and_rebinding() {
    assert_eq!(cost("fn sample(p:TestRegisters<Off>)->TestRegisters<Off>{let p=p.into_start();let x=p.data.get();p.into_off()}"),17);
    assert_eq!(cost("fn read(p:&TestRegisters<On>)->u32{p.data.get()}"), 7);
    assert_eq!(cost("fn cycle(p:TestRegisters<Off>)->TestRegisters<On>{let p=p.into_start();let p=p.into_off();p.into_start()}"),14);
}
#[test]
fn branch_costs_use_maximum_and_state_sets_use_union() {
    assert_eq!(cost("fn sample(p:TestRegisters<Off>,b:bool)->TestRegisters<Off>{if b{let p=p.into_start();p.into_off()}else{p}}"),11);
    assert!(check("fn bad(p:TestRegisters<Off>,b:bool){let p=if b{p.into_start().into()}else{p.into()};p.into_off();}").is_err());
}
#[test]
fn matches_refine_real_enum_variants() {
    assert_eq!(cost("fn f(p:TestStateEnum)->TestRegisters<Off>{match p{TestStateEnum::Off(p)=>p,TestStateEnum::On(p)=>p.into_off()}}"),11);
    assert!(check("fn f(p:TestStateEnum){match p{TestStateEnum::Off(p)=>{}}}").is_err());
}
#[test]
fn helper_calls_include_ambient_devices() {
    let reports=check("mod api{fn start(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()} fn sample(p:TestRegisters<Off>,q:&TestRegisters<On>)->TestRegisters<On>{start(p)}}").unwrap();
    assert!(reports.contains(&("api::sample".into(), 15)));
    assert!(reports.contains(&("api::start".into(), 4)));
}
#[test]
fn impl_methods_and_cells_use_the_same_state_machine() {
    let reports=check("mod api {struct Driver{regs:AbacusCell<TestStateEnum>} impl Driver{fn read(&self)->Option<u32>{self.regs.map(|regs|match regs{TestStateEnum::Off(p)=>{let p=p.into_start();let x=p.data.get();let p=p.into_off();(p.into(),x)},TestStateEnum::On(p)=>{let x=p.data.get();let p=p.into_off();(p.into(),x)}})}}}").unwrap();
    assert_eq!(reports, vec![("api::Driver::read".into(), 28)]);
}
#[test]
fn cell_failure_preserves_states_for_later_commands() {
    let once="cell.map(|p|match p{TestStateEnum::Off(p)=>(p.into(),()),TestStateEnum::On(p)=>{let p=p.into_off();(p.into(),())}})";
    let cost = cost(&format!(
        "fn f(cell:&AbacusCell<TestStateEnum>){{let _={once};let _={once};}}"
    ));
    assert_eq!(cost, 34); // each call: entry 5 + match 5 + max(1,6+1)
}
#[test]
fn metadata_is_authoritative_for_costs_and_permissions() {
    let item =
        syn::parse_str("fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}").unwrap();
    let changed = model(STATES, &REGISTERS.replace("cost = 3", "cost = 9")).unwrap();
    assert_eq!(checking::check(&item, &[changed]).unwrap()[0].1, 10);
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
        7
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
                    .map(|i| if mask & (1 << i) != 0 { 11 } else { 2 })
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
        36
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
        12
    );
}

#[test]
fn block_exit_restores_bindings_but_keeps_peripheral_state_changes() {
    assert_eq!(cost("fn f(p:TestRegisters<On>) { { let (p, local) = (0, 1); let _ = p + local; } p.data.get(); }"), 17);
    assert!(check("fn f(p:TestRegisters<On>) { { let local = 1; } local; }").is_err());
    assert_eq!(cost("fn f(p:TestRegisters<Off>)->TestRegisters<Off> { let p = { let q = p.into_start(); q }; p.into_off() }"), 16);
}
