# Heliod peripheral API cost checking

Costs use the existing DSL. A provider can use integer `usize` values:

- State background cost: `(Off, cost: 1)`.
- Composite state: `(Active(RxIdle, TxIdle), Idle, cost: 1)`.
- Transient state: `(Reading, *T*, cost: 5)`.
- Operation cost: `#[RegAttributes([Off], StateChange(On, [Task::ENABLE::SET]), cost = 3)]`.

Every state requires a cost. An omitted register-operation cost defaults to zero,
as in the parser's original design. All operations and costs are taken from these
same declarations; there is no separate state machine or resource selector.

`process_register_block` generates a metadata provider named after the peripheral:
`peripheral_name = "Sensor"` produces `SensorCost`. Put `#[heliod(SensorCost)]`
on the actual API function, outside the register declaration:

```rust
use abacus_registers::{heliod, process_register_block};
use abacus_registers::tock_registers::{
    interfaces::Readable, register_bitfields,
    registers::{ReadOnly, ReadWrite, WriteOnly},
};
register_bitfields![u32, Task [ENABLE OFFSET(0) NUMBITS(1) []]];

#[process_register_block(
    peripheral_name = "Sensor",
    register_base_addr = 0,
    states = [(Off, cost: 1), (On, cost: 5)]
)]
struct Registers {
    #[RegAttributes([Off], StateChange(On, [Task::ENABLE::SET]), cost = 3)]
    start: WriteOnly<u32, Task::Register>,
    #[RegAttributes([On], StateChange(Off, [Task::ENABLE::SET]), cost = 1)]
    stop: WriteOnly<u32, Task::Register>,
    #[RegAttributes([On], ReadOnly, cost = 2)]
    data: ReadOnly<u32, Task::Register>,
}

#[heliod(SensorCost)]
pub fn sample(sensor: SensorRegisters<Off>) -> SensorRegisters<Off> {
    let sensor = sensor.into_on();
    let _reading = sensor.data.get();
    sensor.into_off()
}
```

Compilation prints one number per function:

```text
Heliod cost for sample: 11
```

Here the cost is `3 + (5 + 2) + 1`. Transition commands do not charge the
transitioning peripheral's background cost. No cost constants, ordered pairs, runtime logging, or MMIO execution are
added. Diagnostic-only constants may be emitted to produce compiler warnings. The
original function is emitted unchanged; Rust still checks its types and ownership. Cost arithmetic assumes totals fit in `usize`, and checked
programs are assumed to contain no recursive calls. Reports are emitted when the compiler expands the attribute; Cargo
may replay cached compiler output without rerunning analysis.

Costs may instead be symbolic identifiers:

```rust
states = [(Off, cost: off_cost), (On, cost: on_cost)]

#[RegAttributes([On], ReadOnly, cost = read_cost)]
data: ReadOnly<u32, Task::Register>,
```

In symbolic mode, compilation prints a sum such as
`on_cost + read_cost + (off_cost -> on_cost)`. Repeated terms are written with a
coefficient, and branches use `max(...)` because their symbolic terms cannot be
ordered during compilation. A transition is represented by its source and target
state variables. Burn analysis and numeric optimization diagnostics are skipped
when any selected provider has symbolic costs.

## API scopes

The attribute accepts functions, nongeneric inherent impls, and inline modules.
Annotate an entire module or impl when a caller needs to include helper bodies.
Unknown callees are errors, rather than free operations.

For drivers with `AbacusCell<PeripheralStateEnum>` fields, annotate an inline
module containing both the struct and impl. The checker supports `self.regs.map`
with an inline closure, matches on generated state-enum variants, and the normal
`(registers.into(), result)` return. It also accounts for the possibility that the
cell is unavailable and the callback does not run.

Providers can be selected across modules or crates, and multiple peripheral types
can be checked together:

```text
#[heliod(chip::SensorCost, chip::UartCost)]
fn transfer(sensor: &chip::SensorRegisters<chip::On>,
            uart: &chip::UartRegisters<chip::Idle>) -> u32 { ... }
```

Import the generated `Step...` traits to call transitions across modules. Generated
transition traits and register-access wrapper types are public. Peripheral names
must be unique within a crate because they name exported metadata macros. Selected
providers must also have distinct register type names.

## Inferred peripheral states

Omit the state type argument to use state inference:

```rust
#[heliod(SensorCost)]
fn sample(sensor: &SensorRegisters) -> u32 {
    sensor.into_on();
    let value = sensor.data.get();
    sensor.into_off();
    value
}
```

Transitions on an inferred register operate in place and return `()`. Heliod starts
each peripheral in the full set of declared stable states and constructs the
smallest satisfying input set from operation constraints. A transition constrains
the preceding state and replaces the current state with its target. At an `if`,
the input requirements of both arms are intersected and the output states are
unioned. Function summaries contain the inferred input and output sets, so callers
apply the callee contract without adding a separate call command.

Because the inferred register has one stable Rust type, drivers may store it
directly instead of wrapping a generated state enum in `AbacusCell`:

```rust
struct Driver {
    registers: SensorRegisters,
}

#[heliod(SensorCost)]
impl Driver {
    fn read(&self) -> u32 {
        self.registers.into_on();
        let value = self.registers.data.get();
        self.registers.into_off();
        value
    }
}
```

The explicit `SensorRegisters<Off>` typestate API and `AbacusCell` remain available
for code that is not checked by Heliod or that needs the existing runtime state
container. Recursive functions and hardware-concurrent transient-state inference
are outside the sequential fragment.

## Rules and scope

- Background cost is charged once per command using the states at command entry:
  sum the maximum possible background cost of each peripheral. For a state
  transition command, omit the transitioning peripheral from this sum; other
  live peripherals still contribute their background costs.
- Register operations add their declared cost and apply their existing state
  changes. Forward, backward, cyclic, and self-transitions are all `StateChange`.
- Branches take the maximum cost and union the possible states per peripheral.
  Matches refine those states. Operations must be valid from every possible state.
- Moving/rebinding a register keeps its peripheral identity; it does not create
  another device or duplicate its background charge.
- Helper bodies include background costs of other live peripherals. Tail
  expressions and bare closure/match-arm bodies count as commands. Empty/grouping
  blocks add no extra charge.
- Ordinary scalar expressions have zero foreground cost. This is a bound under
  the supplied peripheral cost model, not a complete CPU timing model.

This remains a concrete sequential checker. Unknown calls, loops,
assignment, macros in checked bodies, mixed early-return/fallthrough branches,
transient states, async execution, clock changes, and unsupported Rust constructs
produce errors. Generic typestates and type aliases are not resolved. General
`Option`/`Result` combinators and dynamic dispatch need additional rules.

The implementation separates these responsibilities: `cost/mod.rs` passes metadata
between macros, `cost/model.rs` reads existing state/register declarations, and
`cost/checking.rs` walks real Rust API bodies. `cost/diagnostics.rs` formats
explanations of the bounds and emits source-located compiler warnings. There is no global macro registry,
filesystem lookup, symbolic solver, or second value-type checker.

Heliod checks every statement in an annotated body. Constructor calls follow the
same rules as other helpers: their bodies must be available in the checked scope
and use supported constructs. No initialization boundary bypasses analysis.


## Branch and state diagnostics

Heliod automatically emits diagnostics during compilation; no opt-in is required.

At a peripheral operation, Heliod reports when the receiver may be in multiple
states with different background costs. The diagnostic lists those states, names
the cheapest state in which the operation is permitted, and shows the difference
between the cheapest and most expensive background costs for that command.

For `if` and generated-state `match` expressions, Heliod compares the states in
which each arm leaves every peripheral. If one arm can leave a peripheral in a
more expensive state, the diagnostic lists the resulting states and shows the
extra background cost that subsequent commands may pay. Branches whose arms end
at the same cost do not produce this diagnostic.

Helper diagnostics identify the function whose bound is being computed and point
to the operation or branch in the helper's source. Reports explain potential local
savings; they do not change code or reduce the reported function bound.

On stable Rust these source-located diagnostics use a generated deprecated constant
with no runtime work. They therefore follow the `deprecated` lint configuration;
`-D warnings` promotes them to errors. Run `cargo test --test cost_api` to see both
kinds of diagnostics in the example.
