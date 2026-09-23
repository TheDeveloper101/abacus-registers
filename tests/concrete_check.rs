#![allow(dead_code)]
use abacus_registers::abacus_cost;

mod hardware {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };
    register_bitfields![u32, Task [ENABLE OFFSET(0) NUMBITS(1) []]];
    #[process_register_block(
        peripheral_name = "Test",
        register_base_addr = 0,
        states = [(Off, cost = 1), (On, cost = 5)]
    )]
    #[repr(C)]
    struct Registers {
        #[RegAttributes([Off], StateChange(On, [Task::ENABLE::SET], start), cost = 3)]
        start: WriteOnly<u32, Task::Register>,
        #[RegAttributes([On], StateChange(Off, [Task::ENABLE::SET]), cost = 1)]
        stop: WriteOnly<u32, Task::Register>,
        #[RegAttributes([Off, On], ReadOnly, cost = 2)]
        data: ReadOnly<u32, Task::Register>,
    }
}
use hardware::{Off, On, StepOff, Stepstart, TestRegisters};

// This is the real driver API, not a duplicate analysis body. Do not execute MMIO.
#[abacus_cost(hardware::TestCost)]
fn sample(sensor: TestRegisters<Off>) -> TestRegisters<Off> {
    let sensor = sensor.into_start();
    let _reading = sensor.data.get();
    sensor.into_off()
}

#[abacus_cost(hardware::TestCost)]
fn read_only(sensor: &TestRegisters<On>) -> u32 {
    sensor.data.get()
}

#[abacus_cost(hardware::TestCost)]
mod api {
    use crate::hardware::*;
    use abacus_registers::AbacusCell;
    fn enable(sensor: TestRegisters<Off>) -> TestRegisters<On> {
        sensor.into_start()
    }
    pub fn sample(sensor: TestRegisters<Off>, enabled: bool) -> TestRegisters<Off> {
        if enabled {
            let sensor = enable(sensor);
            sensor.into_off()
        } else {
            sensor
        }
    }
    pub struct Driver {
        registers: AbacusCell<TestStateEnum>,
    }
    impl Driver {
        pub fn read(&self) -> Option<u32> {
            self.registers.map(|registers| match registers {
                TestStateEnum::Off(sensor) => {
                    let sensor = sensor.into_start();
                    let reading = sensor.data.get();
                    let sensor = sensor.into_off();
                    (sensor.into(), reading)
                }
                TestStateEnum::On(sensor) => {
                    let reading = sensor.data.get();
                    let sensor = sensor.into_off();
                    (sensor.into(), reading)
                }
            })
        }
    }
}

struct PeripheralApi;
#[abacus_cost(hardware::TestCost)]
impl PeripheralApi {
    fn read(&self, sensor: &TestRegisters<On>) -> u32 {
        self.read_inner(sensor)
    }
    fn read_inner(&self, sensor: &TestRegisters<On>) -> u32 {
        sensor.data.get()
    }
}

mod other_hardware {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };
    register_bitfields![u32, Data [VALUE OFFSET(0) NUMBITS(1) []]];
    #[process_register_block(peripheral_name = "Other", register_base_addr = 0, states = [(Ready, cost = 1 / 2)])]
    struct Registers {
        #[RegAttributes([Ready], ReadOnly, cost = 1 / 3)]
        data: ReadOnly<u32, Data::Register>,
    }
}
#[abacus_cost(hardware::TestCost, other_hardware::OtherCost)]
fn two_devices(
    sensor: &TestRegisters<On>,
    other: &other_hardware::OtherRegisters<other_hardware::Ready>,
) -> u32 {
    let first = sensor.data.get();
    first + other.data.get()
}

#[test]
fn annotated_functions_keep_their_original_rust_signatures() {
    let _: fn(TestRegisters<Off>) -> TestRegisters<Off> = sample;
    let _: fn(&TestRegisters<On>) -> u32 = read_only;
    let _: fn(TestRegisters<Off>, bool) -> TestRegisters<Off> = api::sample;
    // Never instantiate MMIO register references or execute the functions here.
}
