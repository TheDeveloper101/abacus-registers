use abacus_registers::process_register_block;
use abacus_registers::tock_registers::{
    interfaces::Readable,
    register_bitfields,
    registers::{ReadOnly, ReadWrite, WriteOnly},
};
register_bitfields![u32, Data [VALUE OFFSET(0) NUMBITS(1) []]];
#[process_register_block(peripheral_name="External", register_base_addr=0, states=[(Ready,cost:1)])]
struct Registers {
    #[RegAttributes([Ready],ReadOnly,cost=2)]
    data: ReadOnly<u32, Data::Register>,
}
