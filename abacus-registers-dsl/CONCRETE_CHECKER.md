# Cost checking on the peripheral API

Declare costs on the existing Abacus states and `RegAttributes`. Annotate the
**actual Rust API** that uses the generated register methods with
`#[abacus_cost(SensorCost)]`. There are no analysis bodies or cost constants inside
`process_register_block`, no duplicate state machine, and no resource selector.

```rust
use abacus_registers::{abacus_cost, process_register_block};
use abacus_registers::tock_registers::{
    register_bitfields,
    registers::{ReadOnly, ReadWrite, WriteOnly},
    interfaces::Readable,
};
register_bitfields![u32, Task [ENABLE OFFSET(0) NUMBITS(1) []]];

#[process_register_block(
    peripheral_name = "Sensor",
    register_base_addr = 0,
    states = [(Off, cost = 1), (On, cost = 5)]
)]
#[repr(C)]
struct Registers {
    #[RegAttributes([Off], StateChange(On, [Task::ENABLE::SET], start), cost = 3)]
    start: WriteOnly<u32, Task::Register>,
    #[RegAttributes([On], StateChange(Off, [Task::ENABLE::SET]), cost = 1)]
    stop: WriteOnly<u32, Task::Register>,
    #[RegAttributes([On], ReadOnly, cost = 2)]
    data: ReadOnly<u32, Task::Register>,
}

#[abacus_cost(SensorCost)]
pub fn sample(sensor: SensorRegisters<Off>) -> SensorRegisters<Off> {
    let sensor = sensor.into_start();
    let _reading = sensor.data.get();
    sensor.into_off()
}
```

Compilation prints:

```text
Abacus cost for sample: 17
```

The bound is `(1 + 3) + (5 + 2) + (5 + 1) = 17`. The original Rust function is
emitted unchanged and still undergoes normal type and borrow checking. Analysis
never executes its MMIO operations. Reports are compile-time output, not runtime
logging, so this works with `no_std` drivers. Cargo may not rerun the analyzer when
an unchanged artifact is reused; rebuild the affected crate to obtain fresh reports.

The result is **one number per function**, expressed in the units of the supplied
costs. Costs may be nonnegative integers or fractions (`cost = 1 / 3`). The checker
accumulates fractions exactly internally and rounds the final bound **up** to a
whole tick for printing. There is no generated ordered pair or public cost constant.
Arithmetic overflow and missing costs produce errors instead of an understated bound.

## Selecting existing peripheral metadata

A register block with `peripheral_name = "Sensor"` generates `SensorRegisters`,
its state types and transition methods, and a metadata macro named `SensorCost`.
`#[abacus_cost(SensorCost)]` obtains its state machine and costs directly from
those declarations. Metadata is passed through macro expansion; there is no global
registry, filesystem side channel, or dependency on procedural macro execution order.

Providers can be imported from other modules or crates. Multiple providers are
supported, for example:

```text
#[abacus_cost(chip::SensorCost, chip::UartCost)]
fn transfer(sensor: &chip::SensorRegisters<chip::On>,
            uart: &chip::UartRegisters<chip::Idle>) -> u32 { ... }
```

Import the generated `Step...` traits when calling transitions from a different
module, as with any trait method. Register access wrappers and transition traits
are public so API code outside the register declaration module can use them.
Peripheral names must be unique within a crate (they name exported metadata
macros), and selected providers must have distinct generated register type names.
The frontend recognizes generated type names and concrete state arguments; Rust
type aliases and generic typestate parameters are not currently resolved.

## Functions, impls, helpers, and cells

The attribute can be placed on a free function, a nongeneric inherent `impl`, or
an inline module containing API functions/impls. It analyzes real `let` bindings,
ownership transfers, returns, arithmetic, register accesses, branches, and matches
on generated state enums. Function parameter/return types supply the concrete
state contracts; they do not need to be repeated in an analysis language.

To analyze helper calls, put the caller and helper in the same annotated inline
module or impl. An external function body that is unavailable is rejected rather
than assigned a zero cost. Recursion is rejected. Each API function receives its
own printed bound; callers include helper execution and the background costs of
other live peripherals throughout that execution.

For drivers that hold `AbacusCell<PeripheralStateEnum>`, annotate an inline module
containing both the struct and its impl so the checker can inspect the field type:

```text
#[abacus_cost(chip::SensorCost)]
mod api {
    use chip::*;
    use abacus_registers::AbacusCell;

    pub struct Driver { registers: AbacusCell<SensorStateEnum> }
    impl Driver {
        pub fn read(&self) -> Option<u32> {
            self.registers.map(|registers| match registers {
                SensorStateEnum::Off(sensor) => {
                    let sensor = sensor.into_start();
                    let value = sensor.data.get();
                    let sensor = sensor.into_off();
                    (sensor.into(), value)
                }
                SensorStateEnum::On(sensor) => {
                    let value = sensor.data.get();
                    let sensor = sensor.into_off();
                    (sensor.into(), value)
                }
            })
        }
    }
}
```

`map` requires the closure to return the captured peripheral and a result. The
analysis includes the possibility that the cell is unavailable and the callback
does not run, preserving the original possible hardware states for later commands.

## Cost and state rules

- Each state declaration's `cost` is its background cost **per command**.
- Each `RegAttributes` cost is the foreground cost of the annotated operation.
  Its existing permitted source states and `StateChange` target define the edge.
  Upward/downward/cyclic edges are all ordinary state changes; no lattice is needed.
- At command entry, sum the maximum background cost of each live peripheral's
  possible states. Then add foreground work and costs of nested commands.
  A let-call is one command. Empty/grouping blocks add no charge; tail expressions,
  branch conditions, and bare match-arm/closure bodies are commands.
- Branches join each peripheral's possible states with union and take the maximum
  branch cost. Enum patterns refine the states in their arms. An operation must
  be permitted from every possible current state.
- Peripheral identity is retained across `let` rebinding, so moving or borrowing
  a register does not create another device or double-charge its background cost.
- Literal arithmetic and value operations have zero foreground cost. Register
  bitfield constants remain Rust expressions and are checked by rustc. The bound
  is for the supplied peripheral cost model, not a whole-machine WCET estimate.

Existing cost-free register declarations still compile. Invoking cost checking on
an operation or background state without a required cost produces an error.
Malformed `RegAttributes` also produce errors rather than silently generating an
unrestricted register field.

## Current limits

This is a concrete sequential checker, not a general Rust compiler analysis.
It rejects transient states, async/await, loops, recursion, unknown calls, macros
inside checked functions, assignment, mixed early-return/fallthrough branches,
and other unsupported expressions. Inherent impls without a visible struct can
check explicitly typed peripheral arguments; access to `self` fields requires the
struct and impl in the same annotated module. Module helper names must be
unambiguous. Additional adapters are needed for general `Result`/`Option`
combinators, callbacks, dynamic dispatch, clock changes, and arbitrary driver
field types. Such code fails with a diagnostic instead of reporting a false bound.

Run `cargo test` at the repository root for API and cross-crate/report integration
tests. Run `cargo test --manifest-path abacus-registers-dsl/Cargo.toml` for the
checker, parser, generation, and documentation tests.
