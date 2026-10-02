use abacus_registers::heliod;
#[heliod(provider::ExternalCost)]
pub fn external_read(p: &provider::ExternalRegisters<provider::Ready>) -> u32 {
    p.data.get()
}
