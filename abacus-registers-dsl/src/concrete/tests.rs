use super::{
    api,
    model::{Cost, Peripheral},
    peripheral,
};
use crate::{macro_input::MacroInput, process_struct_fields};
use std::{collections::BTreeMap, unreachable};
use syn::DeriveInput;

const STATES: &str = "(Off, cost = 1), (On, cost = 5)";
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
        "peripheral_name = \"Test\", register_base_addr = 0, states = [{states}]"
    ))?;
    let ast: DeriveInput = syn::parse_str(registers)?;
    let syn::Data::Struct(data) = ast.data else {
        unreachable!()
    };
    let (_, registers) = process_struct_fields(&data.fields)?;
    peripheral(&input, &registers)
}
fn analyze_with(states: &str, registers: &str, source: &str) -> syn::Result<Vec<(String, Cost)>> {
    let model = model(states, registers)?;
    api::analyze(
        &syn::parse_str(source)?,
        &BTreeMap::from([("TestRegisters".into(), model)]),
    )
}
fn analyze(source: &str) -> syn::Result<Vec<(String, Cost)>> {
    analyze_with(STATES, REGISTERS, source)
}
fn cost(source: &str) -> Cost {
    analyze(source).unwrap()[0].1
}
fn error(source: &str) -> String {
    analyze(source)
        .err()
        .expect("expected rejection")
        .to_string()
}

#[test]
fn checks_actual_rust_moves_and_return_types() {
    assert_eq!(cost("fn sample(p: TestRegisters<Off>) -> TestRegisters<Off> { let p = p.into_start(); let x = p.data.get(); p.into_off() }"),Cost(17,1));
    assert!(
        error("fn bad(p: TestRegisters<Off>) -> TestRegisters<Off> { p.into_start() }")
            .contains("typestate")
    );
    assert!(error(
        "fn bad(p: TestRegisters<Off>) { let q = p.into_start(); let r = p.into_start(); }"
    )
    .contains("moved"));
}
#[test]
fn transitions_and_downgrades_are_only_state_changes() {
    assert_eq!(cost("fn sample(p: TestRegisters<Off>) -> TestRegisters<On> { let p = p.into_start(); let p = p.into_off(); p.into_start() }"),Cost(14,1));
    assert!(error("fn bad(p: TestRegisters<Off>) { p.into_on(); }").contains("unavailable"));
}
#[test]
fn borrowed_reads_and_multiple_peripherals_are_accounted_once_each() {
    assert_eq!(
        cost("fn read(p: &TestRegisters<On>) -> u32 { p.data.get() }"),
        Cost(7, 1)
    );
    assert_eq!(cost("fn read(p: &TestRegisters<On>, q: &TestRegisters<Off>) -> u32 { let x=p.data.get(); x+q.data.get() }"),Cost(16,1));
    assert!(error("fn bad(p: &TestRegisters<Off>) { p.into_start(); }").contains("owned"));
}
#[test]
fn branches_return_owned_registers_and_take_maximum_cost() {
    assert_eq!(cost("fn f(p: TestRegisters<Off>, b: bool) -> TestRegisters<Off> { if b { let p=p.into_start(); p.into_off() } else { p } }"),Cost(11,1));
    assert_eq!(cost("fn f(p: TestRegisters<Off>, b: bool) -> TestStateEnum { if b { p.into_start().into() } else { p.into() } }").reported(),5);
}
#[test]
fn enum_match_refines_real_variants() {
    assert_eq!(cost("fn f(p: TestStateEnum) -> TestRegisters<Off> { match p { TestStateEnum::Off(p) => p, TestStateEnum::On(p) => p.into_off() } }"),Cost(11,1));
    assert!(
        error("fn f(p: TestStateEnum) { match p { TestStateEnum::Off(p) => {} } }")
            .contains("non-exhaustive")
    );
}
#[test]
fn state_sets_after_branches_require_all_sources() {
    assert!(error("fn f(p: TestRegisters<Off>, b: bool) { let p = if b { p.into_start().into() } else { p.into() }; p.into_off(); }").contains("unavailable"));
}
#[test]
fn helpers_analyze_the_actual_bodies_and_keep_ambient_costs() {
    let reports=analyze("mod api { fn start(p: TestRegisters<Off>) -> TestRegisters<On> { p.into_start() } fn f(p: TestRegisters<Off>, q: &TestRegisters<On>) -> TestRegisters<On> { start(p) } }").unwrap();
    assert!(reports.contains(&("api::f".into(), Cost(15, 1)))); // call bg6 + body bg6+3
    assert!(reports.contains(&("api::start".into(), Cost(4, 1))));
    assert!(error("fn f(p: TestRegisters<Off>) { unknown(p); }").contains("callee body"));
    assert!(error("mod api { fn f(p: TestRegisters<Off>) { f(p); } }").contains("recursive"));
}
#[test]
fn inherent_methods_and_self_helpers_are_checked() {
    let reports=analyze("impl Driver { fn read(&self,p:&TestRegisters<On>) -> u32 { self.inner(p) } fn inner(&self,p:&TestRegisters<On>) -> u32 { p.data.get() } }").unwrap();
    assert!(reports.contains(&("Driver::read".into(), Cost(12, 1))));
}
#[test]
fn cell_closures_use_enum_state_refinement_and_real_move_syntax() {
    let reports=analyze("mod api { struct Driver { regs: AbacusCell<TestStateEnum> } impl Driver { fn read(&self)->Option<u32> { self.regs.map(|regs| match regs { TestStateEnum::Off(p)=>{let p=p.into_start();let x=p.data.get();let p=p.into_off();(p.into(),x)},TestStateEnum::On(p)=>{let x=p.data.get();let p=p.into_off();(p.into(),x)} }) } } }").unwrap();
    assert_eq!(reports, vec![("api::Driver::read".into(), Cost(28, 1))]);
}
#[test]
fn cell_map_failure_keeps_the_original_possible_state() {
    let reports=analyze("fn f(cell:&AbacusCell<TestStateEnum>)->Option<()> { let _=cell.map(|p|match p { TestStateEnum::Off(p)=>(p.into(),()),TestStateEnum::On(p)=>{let p=p.into_off();(p.into(),())} }); cell.map(|p|match p { TestStateEnum::Off(p)=>(p.into(),()),TestStateEnum::On(p)=>{let p=p.into_off();(p.into(),())} }) }").unwrap();
    assert!(reports[0].1 .0 > 20); // second map must still account for On
}
#[test]
fn changing_existing_metadata_changes_cost_and_permissions() {
    let source = "fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}";
    assert_eq!(cost(source), Cost(4, 1));
    assert_eq!(
        analyze_with(STATES, &REGISTERS.replace("cost = 3", "cost = 7"), source).unwrap()[0].1,
        Cost(8, 1)
    );
    assert!(analyze_with(
        STATES,
        &REGISTERS.replace("[Off], StateChange", "[On], StateChange"),
        source
    )
    .is_err());
}
#[test]
fn missing_costs_and_transient_states_fail_closed() {
    let source = "fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}";
    assert!(analyze_with(&STATES.replace("Off, cost = 1", "Off"), REGISTERS, source).is_err());
    assert!(analyze_with(STATES, &REGISTERS.replace(", cost = 3", ""), source).is_err());
    assert!(analyze_with(
        &STATES.replace("On, cost", "On, *T*, cost"),
        REGISTERS,
        source
    )
    .is_err());
}
#[test]
fn reporting_is_one_conservative_integer_not_a_pair() {
    assert_eq!(
        api::report("sample", Cost(17, 1)),
        "Abacus cost for sample: 17"
    );
    assert_eq!(
        api::report("sample", Cost(1, 3)),
        "Abacus cost for sample: 1"
    );
    assert_eq!(
        api::report("sample", Cost(14, 3)),
        "Abacus cost for sample: 5"
    );
    assert_eq!(Cost(u128::MAX, 1).reported(), u128::MAX);
    assert_eq!(Cost(0, 7).reported(), 0);
}
#[test]
fn fractions_are_accumulated_exactly_before_reporting() {
    let reports = analyze_with(
        "(Off, cost = 1 / 3), (On, cost = 5)",
        &REGISTERS.replace("cost = 3", "cost = 1 / 3"),
        "fn f(p:TestRegisters<Off>)->TestRegisters<On>{p.into_start()}",
    )
    .unwrap();
    assert_eq!(reports[0].1, Cost(2, 3));
    assert_eq!(reports[0].1.reported(), 1);
}
#[test]
fn old_embedded_checks_are_rejected_with_migration_guidance() {
    let result = syn::parse_str::<MacroInput>(
        "peripheral_name=\"Test\",register_base_addr=0,states=[(Off)],checks={}",
    );
    assert!(result.err().unwrap().to_string().contains("API functions"));
}
#[test]
fn metadata_is_generated_even_for_legacy_declarations_and_no_constant_is_emitted() {
    let input: MacroInput =
        syn::parse_str("peripheral_name=\"Test\",register_base_addr=0,states=[(Off),(On)],")
            .unwrap();
    let ast = syn::parse_str(REGISTERS).unwrap();
    let result = crate::expand_register_block(input, ast)
        .unwrap()
        .to_string();
    assert!(result.contains("TestCost"));
    assert!(result.contains("macro_rules"));
    assert!(!result.contains("const BOUND"));
}
#[test]
fn unknown_work_is_never_silently_free() {
    for body in [
        "loop {}",
        "while true {}",
        "unknown();",
        "let mut x=1;x=2;",
        "let _=std::time::Instant::now();",
        "assert!(true);",
        "if true {return;} p.data.get();",
    ] {
        assert!(
            analyze(&format!("fn f(p:&TestRegisters<On>){{{body}}}")).is_err(),
            "accepted {body}"
        );
    }
}
#[test]
fn explicit_return_and_tuples_preserve_rust_signatures() {
    assert_eq!(
        cost("fn f(p:TestRegisters<Off>)->TestRegisters<On>{return p.into_start();}"),
        Cost(4, 1)
    );
    assert_eq!(
        cost("fn f(p:TestRegisters<Off>)->(TestRegisters<Off>,u32){let x=p.data.get();(p,x)}"),
        Cost(4, 1)
    );
}
#[test]
fn bounds_cover_each_concrete_branch_execution() {
    for rounds in 1..=4 {
        let stmt = "let p=if b {let p=p.into_start();p.into_off()} else {p};";
        let bound = cost(&format!(
            "fn f(p:TestRegisters<Off>,b:bool)->TestRegisters<Off>{{{}p}}",
            stmt.repeat(rounds)
        ));
        for mask in 0..1 << rounds {
            let mut actual = 1;
            for i in 0..rounds {
                actual += if mask & (1 << i) != 0 {
                    1 + 4 + 6
                } else {
                    1 + 1
                };
            }
            assert!(bound.0 >= actual * bound.1);
        }
    }
}
