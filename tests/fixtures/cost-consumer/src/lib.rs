use abacus_registers::heliod;
use provider::StepActive;
#[heliod(provider::ExternalCost)]
pub fn external_read(p: &provider::ExternalRegisters<provider::Ready>) -> u32 {
    p.data.get()
}

#[heliod(provider::ExternalCost)]
pub fn expensive_branch(
    p: provider::ExternalRegisters<provider::Ready>,
    enabled: bool,
) -> provider::ExternalStateEnum {
    if enabled {
        p.into_active().into()
    } else {
        p.into()
    }
}
