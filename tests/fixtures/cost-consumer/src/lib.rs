use abacus_registers::abacus_cost;
#[abacus_cost(provider::ExternalCost)]
pub fn external_read(p: &provider::ExternalRegisters<provider::Ready>) -> u32 {
    p.data.get()
}
