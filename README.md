# abacus-registers

Abacus provides a register interface that statically prevents 
device protocol violations, i.e. software initiated hardware operations
that violate the hardware's specification.

Abacus provides a DSL for developers to encode the hardware state machine
and device protocol. Abacus then autogenerates type-states and type-state 
transitions to represent the device protocol in the type-system and, using
the Rust compiler, statically prevent device protocol violations.

## Using abacus-registers

`abacus-registers` is a wrapper around the [`tock-registers`](https://crates.io/crates/tock-registers) interface. To use
Abacus, we first annotate the register `struct` as follows:

```Rust
#[repr(C)]
#[process_register_block(
    peripheral_name = "Nrf5xTemp",
    register_base_addr = 0x4000C000,
    states = [
        (Off, cost: 1),
        (Reading, cost: 5),
    ]
)]
struct TemperatureRegisters {
    /// Start temperature measurement
    /// Address: 0x000 - 0x004
    #[RegAttributes([Off], StateChange(Reading, [Task::ENABLE::SET]))]
    pub task_start: WriteOnly<u32, Task::Register>,
    /// Stop temperature measurement
    /// Address: 0x004 - 0x008
    #[RegAttributes([Reading], StateChange(Off, [Task::ENABLE::SET]))]
    pub task_stop: WriteOnly<u32, Task::Register>,
    /// Enable interrupt
    /// Address: 0x304 - 0x308
    pub intenset: ReadWrite<u32, Intenset::Register>,
    /// Disable interrupt
    /// Address: 0x308 - 0x30c
    pub intenclr: ReadWrite<u32, Intenclr::Register>,
    /// Temperature in °C (0.25° steps)
    /// Address: 0x508 - 0x50c
    #[RegAttributes([Reading], ReadOnly)]
    pub temp: ReadOnly<u32, Temperature::Register>,
```

Abacus currently supports annotating:
- State transitioning registers
- Constraining register operations to a set of states.

The `task_start` and `task_stop` registers above are examples of a state transitioning
register. 
```Rust
#[RegAttributes([Off], StateChange(Reading, [Task::ENABLE::SET]))]
                 ^^^                ^^^^^^         ^^^^^^^^^^
[States trans. valid from]    [State trans. to]   [Register bitfields to write to perform transition] 
```

The `temp` register represents a standard R/O register that may only be interacted
while the peripheral is in a certain state.
```Rust
#[RegAttributes([Reading], Readonly)]
pub temp: ReadOnly<u32, Temperature::Register>
```


A key difference with Abacus is that the driver no longer holds a reference to the 
register struct. 

```Rust
struct TemperatureDriver {
(-)  regs: &TemperatureRegisters
(+)  regs: AbacusCell(TemperatureStore)     
     ...
}
```

Instead, Abacus has a custom Cell type: `AbacusCell`. Because Abacus must track the hardware type-state
and must model state transitions, we use move semantics and a singleton type-state. The AbacusCell
uses interior mutability and enables `&self` drivers to provide access to an owned register type-state
within a closure.

```Rust
    fn read_temperature(&self) -> Result<(), ErrorCode> {
        self.registers
            .map(|regs| match regs {
                Nrf5xTempStore::Off(reg) => {
                    self.enable_interrupts(&reg);
                    reg.event_datardy.write(Event::READY::CLEAR);
                    let reg = reg.into_reading();
                    (reg.into(), Ok(()))
                }
                Nrf5xTempStore::Reading(reg) => (reg.into(), Err(ErrorCode::BUSY)),
            })
            .map_or_else(|| Err(ErrorCode::FAIL), |res| res)
    }
```

(see [this branch](https://github.com/abacus-rs/tock/blob/master/chips/nrf5x/src/temperature.rs) for the full 
example shown above)

## Heliod cost checking

States accept integer or symbolic background costs, e.g. `(Off, cost: 1)` or
`(Off, cost: off_cost)`. Register attributes accept an optional integer or symbolic
foreground cost, e.g. `#[RegAttributes([Off], ReadOnly, cost = read_cost)]`.
Annotate real API functions with `#[heliod(PeripheralCost)]` to print their
cost bounds during compilation. Compiler diagnostics also highlight expensive
branches and uncertain peripheral states at register operations. See the [usage guide](abacus-registers-dsl/COST_CHECKING.md)
for helpers, `AbacusCell` drivers, and supported constructs.

Heliod can infer peripheral states when the generated register type omits its state
argument. In that form transitions happen in place, so drivers can store the
register handle directly and do not need `AbacusCell` solely to accommodate a
changing Rust typestate.

## Wrapping an existing PAC

Drivers that already use a peripheral access crate can select an external backend
instead of duplicating its MMIO register layout:

```Rust
#[process_register_block(
    peripheral_name = "Nrf5xTemp",
    backend = external(pac::temp::Temp),
    states = [
        (Off, cost: temp_off),
        (Reading, *T*, cost: temp_reading)
    ],
    transitions = [Off => Reading, Reading => Off]
)]
struct Nrf5xTempModel;
```

This generates `Nrf5xTempRegisters<S>`, the state types, the state enum, and only
the declared retyping transitions. `Nrf5xTempRegisters::new` is unsafe because the
driver must establish exclusive ownership and the asserted initial hardware state.
The wrapped PAC handle and transition methods are crate-private: the trusted HAL
implementation can implement the hardware operations, while downstream application
code cannot bypass its typestate API. HAL operations should use `heliod_operation`
contracts so their costs and state effects compose into application bounds.
