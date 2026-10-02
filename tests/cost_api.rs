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
        #[RegAttributes([On], StateChange(Off, [Task::ENABLE::SET]), cost = 1)]
        stop: WriteOnly<u32, Task::Register>,
        #[RegAttributes([Off, On], ReadOnly, cost = 2)]
        data: ReadOnly<u32, Task::Register>,
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

use hardware::*;
#[test]
fn original_rust_signatures_are_preserved_without_executing_mmio() {
    let _: fn(TestRegisters<Off>, bool) -> TestRegisters<Off> = api::sample;
    let _: fn(&api::Driver) -> Option<u32> = api::Driver::read;
}
