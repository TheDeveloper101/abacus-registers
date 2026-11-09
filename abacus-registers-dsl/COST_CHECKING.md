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

## Opaque operation contracts

`#[heliod_operation]` describes a peripheral operation whose implementation is
outside the checked Rust scope, such as a Tock userspace syscall wrapper. The
function body is preserved and compiled normally, but Heliod uses the contract
instead of attempting to inspect that body:

```rust
#[heliod(TestCost)]
mod userspace {
    use abacus_registers::heliod_operation;

    #[heliod_operation(
        peripheral = TestCost,
        from = Off,
        to = Off,
        cost = temperature_read
    )]
    fn read_temperature() -> Result<i32, Error> {
        platform_temperature_read()
    }

    fn sample_twice() {
        let _ = read_temperature();
        let _ = read_temperature();
    }
}
```

This reports `temperature_read` for `read_temperature` and
`2 * temperature_read` for `sample_twice`. The provider and both states are
validated against the selected Abacus model. The cost is an all-inclusive
operation summary, so Heliod does not add another background charge for the
summarized peripheral. One implicit peripheral identity is shared by contracts
using the same provider, including across helper calls and branches.

The `cost` field accepts the same integer or symbolic identifier as the register
DSL. A symbolic identifier is the name of an externally established summary;
the current macro does not extract a function summary from a separately compiled
kernel crate. Crossing that crate boundary therefore requires publishing the
derived kernel expression under the identifier used by the userspace contract.

Opaque operations may also be async. Heliod supports immediately awaited calls
to opaque async functions and to checked async helpers:

```rust
#[heliod_operation(
    peripheral = Nrf5xTempCost,
    from = Off,
    to = Off,
    cost = 42,
    background_steps = 10
)]
async fn read_temperature(temp: &mut embassy_nrf::temp::Temp<'_>) -> i32 {
    embassy_time::with_timeout(
        embassy_time::Duration::from_millis(10),
        temp.read(),
    )
    .await
    .map(|value| value.to_num())
    .unwrap_or(i32::MIN)
}

async fn sample_twice(temp: &mut embassy_nrf::temp::Temp<'_>) {
    let _first = read_temperature(temp).await;
    let _second = read_temperature(temp).await;
}
```

The manually supplied `cost` is the complete worst-case cost of the summarized
peripheral itself, including its timeout and recovery. `background_steps` is the
maximum number of background-charge points inside that operation. At a call site,
Heliod adds each other live peripheral's background cost that many times; it does
not add the summarized peripheral again. A stored or unawaited async operation is
rejected. This deliberately small async fragment does not yet support stored
futures, async method calls, `join`, `select`, spawning, or arbitrary future
combinators in checked callers. The opaque operation body remains ordinary Rust
and may use those implementation details internally; only its contract is checked.

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
container. Recursive functions remain unsupported.

## Bounded asynchronous operations

Heliod models the paper's capturing `async` and timed `await` rules with two
analyzer intrinsics. A Tock integration can provide functions with these names
whose runtime behavior starts an interrupt-driven operation and installs its
completion and alarm callbacks:

```rust
let registers = registers.into_start(); // enters a transient state
let task = heliod_async(registers, |registers| {
    // normal completion path
    registers.sync_state()
});
heliod_await(task, 10, |registers| {
    // timeout recovery path: cancel/reset and synchronize
    registers.sync_state()
});
```

`heliod_async` captures exactly one peripheral. Its closure is checked from the
full hardware-reachable transient state set and must finish in stable states.
The captured peripheral cannot be used by the caller before `heliod_await`.
`heliod_await` requires a nonnegative integer-literal timeout, checks its fallback
from every reachable transient state, and requires the fallback to finish in
stable states as well. Every task must be awaited before its function exits.

The bound for the await is the maximum of the normal completion path and:

```text
timeout * (maximum reachable transient background cost
           + background cost of every other live peripheral)
+ fallback cost
```

The join restores the peripheral with the union of the normal and timeout stable
states. `sync_state()` reuses Abacus's transient reconciliation mechanism. For
cost checking, every possible transient source must have a declared state-machine
transition to a stable state; Heliod conservatively unions those stable targets
and takes their maximum transition cost. Transient-to-transient declared state
changes form `HWReach` because the current Abacus state machine does not have a
separate hardware-edge declaration.

Other pending async peripherals remain live during the timeout. Heliod charges
each one using its maximum reachable transient background cost on every timeout
step; ordinary ambient peripherals use their current possible state sets.

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

Outside the bounded async forms, this remains a concrete sequential checker. Unknown calls, loops,
assignment, macros in checked bodies, mixed early-return/fallthrough branches,
Rust `async`/`.await`, clock changes, and unsupported Rust constructs
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

Burn analysis also recognizes repeated active sessions. When a peripheral is
torn down, remains unused, and is restored to the same state before its next use,
Heliod compares the written teardown/idle/setup sequence with keeping the
peripheral active across the interval. If staying active is cheaper, it reports a
batching opportunity with both costs and the potential saving. The interval may
contain checked helper calls and branches; branches use their maximum cost. Any
intervening operation on the peripheral is replayed in the retained active state.
The recommendation is emitted only if every such operation remains available,
preserves the expected state, and the resulting schedule is cheaper.

Transition-boundary analysis reports expensive states entered before their first
required use and left after their last required use. It replays the intervening
commands in the cheaper state, including checked helpers and maximum-cost branch
arms, and compares the complete affected region after relocating the transition.
The diagnostic reports how many commands the boundary can cross and the estimated
saving. This is advisory: Heliod does not attempt to represent warm-up time or
other intent that is absent from the state machine.

Helper diagnostics identify the function whose bound is being computed and point
to the operation or branch in the helper's source. Reports explain potential local
savings; they do not change code or reduce the reported function bound.

On stable Rust these source-located diagnostics use a generated deprecated constant
with no runtime work. They therefore follow the `deprecated` lint configuration;
`-D warnings` promotes them to errors. Run `cargo test --test cost_api` to see both
kinds of diagnostics in the example.
