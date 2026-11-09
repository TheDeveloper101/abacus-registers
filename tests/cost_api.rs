#![allow(dead_code)]
use abacus_registers::heliod;
mod hardware {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };
    register_bitfields![u32, Task [ENABLE OFFSET(0) NUMBITS(1) []]];
    #[process_register_block(peripheral_name="Test",register_base_addr=0,
        states=[(Off,cost:1),(On,cost:5)])]
    struct Registers {
        #[RegAttributes([Off], StateChange(On, [Task::ENABLE::SET], start), cost = 3)]
        start: WriteOnly<u32, Task::Register>,
        #[RegAttributes([On, Off], StateChange(Off, [Task::ENABLE::SET]), cost = 1)]
        stop: WriteOnly<u32, Task::Register>,
        #[RegAttributes([Off, On], ReadOnly, cost = 2)]
        data: ReadOnly<u32, Task::Register>,
    }
}

mod sensor {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };

    register_bitfields![u32, SensorTask [ENABLE OFFSET(0) NUMBITS(1) []]];

    #[process_register_block(
        peripheral_name = "Sensor",
        register_base_addr = 0,
        states = [(Idle, cost: 1), (Sensing, cost: 4)]
    )]
    struct Registers {
        #[RegAttributes([Idle], StateChange(Sensing, [SensorTask::ENABLE::SET]), cost = 2)]
        start: WriteOnly<u32, SensorTask::Register>,
        #[RegAttributes([Sensing], ReadOnly, cost = 1)]
        sample: ReadOnly<u32, SensorTask::Register>,
    }
}

mod radio {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };

    register_bitfields![u32, RadioData [VALUE OFFSET(0) NUMBITS(32) []]];

    #[process_register_block(
        peripheral_name = "Radio",
        register_base_addr = 0,
        states = [(Sleep, cost: 1), (Ready, cost: 6)]
    )]
    struct Registers {
        #[RegAttributes([Ready], ReadOnly, cost = 3)]
        data: ReadOnly<u32, RadioData::Register>,
    }
}

mod timer {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };

    register_bitfields![u32, TimerTask [ENABLE OFFSET(0) NUMBITS(1) []]];

    #[process_register_block(
        peripheral_name = "Timer",
        register_base_addr = 0,
        states = [(Off, cost: 0), (Running, cost: 2)]
    )]
    struct Registers {
        #[RegAttributes([Off], StateChange(Running, [TimerTask::ENABLE::SET]), cost = 1)]
        start: WriteOnly<u32, TimerTask::Register>,
    }
}

mod symbolic_sensor {
    use abacus_registers::process_register_block;
    use abacus_registers::tock_registers::{
        interfaces::Readable,
        register_bitfields,
        registers::{ReadOnly, ReadWrite, WriteOnly},
    };

    register_bitfields![u32, SymbolicTask [ENABLE OFFSET(0) NUMBITS(1) []]];

    #[process_register_block(
        peripheral_name = "SymbolicSensor",
        register_base_addr = 0,
        states = [(Off, cost: off_cost), (On, cost: on_cost)]
    )]
    struct Registers {
        #[RegAttributes([Off], StateChange(On, [SymbolicTask::ENABLE::SET]), cost = start_cost)]
        start: WriteOnly<u32, SymbolicTask::Register>,
        #[RegAttributes([On], StateChange(Off, [SymbolicTask::ENABLE::SET]), cost = stop_cost)]
        stop: WriteOnly<u32, SymbolicTask::Register>,
        #[RegAttributes([On], ReadOnly, cost = read_cost)]
        data: ReadOnly<u32, SymbolicTask::Register>,
    }
}

use symbolic_sensor::{StepOff, StepOn};

#[heliod(symbolic_sensor::SymbolicSensorCost)]
fn symbolic_read(
    sensor: symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off>,
) -> symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off> {
    let sensor = sensor.into_on();
    let _ = sensor.data.get();
    sensor.into_off()
}

#[heliod(symbolic_sensor::SymbolicSensorCost)]
fn inferred_symbolic_read(sensor: &symbolic_sensor::SymbolicSensorRegisters) -> u32 {
    sensor.into_on();
    let value = sensor.data.get();
    sensor.into_off();
    value
}
#[heliod(symbolic_sensor::SymbolicSensorCost)]
fn test_branches_symbolic(
    sensor: symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off>,
    b: bool,
) -> symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off> {
    if b {
        let sensor = sensor.into_on();
        let a = sensor.data.get();
        sensor.into_off()
    } else {
        let sensor = sensor.into_on();
        sensor.into_off()
    }
}

#[heliod(
    sensor::SensorCost,
    radio::RadioCost,
    timer::TimerCost,
    hardware::TestCost
)]
mod three_peripherals {
    use crate::{
        hardware::*,
        radio::{RadioRegisters, *},
        sensor::*,
        timer::*,
    };

    pub fn first(sensor: &SensorRegisters, radio: &RadioRegisters) -> u32 {
        let sample = sensor.sample.get(); // 4 + 6 + 1 == 11
        let packet = radio.data.get(); // 4 + 6 + 3 == 13
        sample + packet // 4 + 6 == 10
    }
    // total cost is 34

    pub fn second(
        timer: TimerRegisters,
        sensor: &SensorRegisters,
        radio: &RadioRegisters,
    ) -> (TimerRegisters, u32) {
        timer.into_running(); // 4 + 6 + 1 == 11
        let value = first(sensor, radio); // costs 40 with the addition of the timer
        (timer, value) // 4 + 6 + 2 == 12
    }
    // total cost is 63

    pub fn third(hardware: &TestRegisters) {
        // hardware.into_off();
        if true {
            hardware.into_start();
        } else {
            hardware.into_off();
        }
    }
}

#[heliod(hardware::TestCost)]
mod api {
    use crate::hardware::*;
    use abacus_registers::AbacusCell;
    fn start(p: TestRegisters<Off>) -> TestRegisters<On> {
        p.into_start()
    }
    pub fn sample(p: TestRegisters<Off>, enabled: bool) -> TestRegisters<Off> {
        if enabled {
            let p = start(p);
            p.into_off()
        } else {
            p
        }
    }

    fn expensive_computation() {
        let _first_step = 1 + 1;
        let _second_step = 2 + 2;
        let _third_step = 3 + 3;
    }

    pub fn burn_bug(p: TestRegisters<Off>) -> TestRegisters<Off> {
        let p = p.into_start();
        expensive_computation();
        p.into_off()
    }

    pub struct Driver {
        regs: AbacusCell<TestStateEnum>,
    }
    impl Driver {
        pub fn read(&self) -> Option<u32> {
            self.regs.map(|regs| match regs {
                TestStateEnum::Off(p) => {
                    let p = p.into_start();
                    let reading = p.data.get();
                    let p = p.into_off();
                    (p.into(), reading)
                }
                TestStateEnum::On(p) => {
                    let reading = p.data.get();
                    let p = p.into_off();
                    (p.into(), reading)
                }
            })
        }
    }
}

#[heliod(hardware::TestCost)]
mod opaque_userspace_api {
    use abacus_registers::heliod_operation;

    pub struct EmbassyTemp;

    async fn platform_temperature_read(_temp: &mut EmbassyTemp) -> Result<i32, ()> {
        Ok(2350)
    }

    #[heliod_operation(
        peripheral = crate::hardware::TestCost,
        from = Off,
        to = Off,
        cost = nrf52_temperature_read,
        background_steps = 10
    )]
    async fn read_temperature(temp: &mut EmbassyTemp) -> Result<i32, ()> {
        platform_temperature_read(temp).await
    }

    pub async fn sample_twice(temp: &mut EmbassyTemp) {
        let _ = read_temperature(temp).await;
        let _ = read_temperature(temp).await;
    }
}

use hardware::*;
#[test]
fn original_rust_signatures_are_preserved_without_executing_mmio() {
    let _: fn(&symbolic_sensor::SymbolicSensorRegisters) -> u32 = inferred_symbolic_read;
    let _: fn(
        symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off>,
    ) -> symbolic_sensor::SymbolicSensorRegisters<symbolic_sensor::Off> = symbolic_read;
    let _: fn(TestRegisters<Off>, bool) -> TestRegisters<Off> = api::sample;
    let _: fn(TestRegisters<Off>) -> TestRegisters<Off> = api::burn_bug;
    let _: fn(&api::Driver) -> Option<u32> = api::Driver::read;
    // let _: fn(
    //     &sensor::SensorRegisters<sensor::Sensing>,
    //     &radio::RadioRegisters<radio::Ready>,
    // ) -> u32 = three_peripherals::first;
    // let _: fn(
    //     timer::TimerRegisters<timer::Off>,
    //     &sensor::SensorRegisters<sensor::Sensing>,
    //     &radio::RadioRegisters<radio::Ready>,
    // ) -> (timer::TimerRegisters<timer::Running>, u32) = three_peripherals::second;
}
