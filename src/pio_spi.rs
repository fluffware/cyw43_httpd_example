//use core::arch::asm;
//use defmt::debug;
use embassy_rp::dma::Channel;
use embassy_rp::gpio::{Drive, Level, Pin, Pull, SlewRate};
use embassy_rp::pio::{PioStateMachine, ShiftDirection};
use embassy_rp::pio_instr_util;
use embassy_rp::PeripheralRef;
use embedded_hal_async::spi::{self, ErrorType, SpiBusFlush, SpiBusRead, SpiBusWrite};
use pio::ProgramWithDefines;

//const IRQ_SAMPLE_DELAY_NS: u32 = 100;

fn setup_program<SM: PioStateMachine, const PROGRAM_SIZE: usize, PD>(
    sm: &mut SM,

    prg: ProgramWithDefines<PD, PROGRAM_SIZE>,
) {
    let origin = prg.program.origin.unwrap_or(0);
    sm.write_instr(origin as usize, prg.program.code.into_iter());
    sm.set_wrap(prg.program.wrap.source, prg.program.wrap.target);
    pio_instr_util::exec_jmp(sm, origin);
    sm.set_side_enable(prg.program.side_set.optional());
    sm.set_side_pindir(prg.program.side_set.pindirs());
    sm.set_sideset_count(prg.program.side_set.bits());
}

//const SYS_CLK: u32 = 125_000_000;
/*
pub fn ns_delay(ns: u32) {
    unsafe {
        asm!("1: subs {ns},{ns}, #1", // 1 cycle
             "bne 1b", // 2 cycles
             ns = inout(reg) ((ns as u64 * SYS_CLK as u64)/ (3 *1_000_000_000)) as u32 => _,
        );
    }
}*/

#[derive(Debug)]
pub struct PioSpiError {
    kind: spi::ErrorKind,
}

impl spi::Error for PioSpiError {
    fn kind(&self) -> spi::ErrorKind {
        self.kind
    }
}

pub struct PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    sm: SM,
    dma_write: PeripheralRef<'a, ChW>,
    dma_read: PeripheralRef<'a, ChR>,
    write_start: u8,
    read_start: u8,
}

impl<'a, SM, ChW, ChR> PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    pub fn new(
        mut sm: SM,
        clock_pin: impl Pin,
        data_pin: impl Pin,
        dma_write: PeripheralRef<'a, ChW>,
        dma_read: PeripheralRef<'a, ChR>,
    ) -> Self {
        sm.set_side_enable(false);
        sm.restart();

        // Load program
        let prg = pio_proc::pio_file!("src/spi.pio", select_program("spi_write_read"),);
        let read_start = prg.public_defines.read as u8;
        let write_start = prg.public_defines.write as u8;
        setup_program(&mut sm, prg);

        // Set up pins
        let mut data_pin = sm.make_pio_pin(data_pin);
        data_pin.set_drive_strength(Drive::_12mA);
        data_pin.set_slew_rate(SlewRate::Fast);
        data_pin.set_pull(Pull::Up);
        data_pin.set_schmitt(true);
        let mut clock_pin = sm.make_pio_pin(clock_pin);

        // Set clock pin as output
        sm.set_set_pins(&[&clock_pin]);
        pio_instr_util::set_pindir(&mut sm, 0b1);
        clock_pin.set_drive_strength(Drive::_8mA);
        let out_pins = [&data_pin];
        sm.set_out_pins(&out_pins);
        sm.set_set_pins(&out_pins);
        sm.set_in_base_pin(&data_pin);
        sm.set_sideset_count(1);
        sm.set_sideset_base_pin(&clock_pin);
        data_pin.set_input_sync_bypass(true);
        sm.set_clkdiv((125e6 / 10e6 * 256.0) as u32);

        // Configure FIFOs
        sm.set_autopull(true);
        sm.set_autopush(true);
        sm.set_pull_threshold(32);
        sm.set_push_threshold(32);

        sm.set_out_shift_dir(ShiftDirection::Left);
        sm.set_in_shift_dir(ShiftDirection::Left);

        PioSpi {
            sm,
            dma_write,
            dma_read,
            write_start,
            read_start,
        }
    }

    pub fn set_data_level(&mut self, level: Level) {
        self.sm.set_enable(false);
        self.sm.restart();
        pio_instr_util::set_out_pindir(&mut self.sm, 0xffffffff);
        pio_instr_util::set_out_pin(
            &mut self.sm,
            if level == Level::High { 0xffffffff } else { 0 },
        );
    }
}

impl<'a, SM, ChW, ChR> SpiBusWrite<u32> for PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    async fn write(&mut self, words: &[u32]) -> Result<(), Self::Error> {
        //debug!("SPI write: {:08x} {}", words[0], words.len());
        self.sm.set_enable(false);

        self.sm.restart();
        self.sm.clear_fifos();
        pio_instr_util::set_pindir(&mut self.sm, 0b1);
        self.sm.clkdiv_restart();

        pio_instr_util::set_x(&mut self.sm, words.len() as u32 * 32 - 1);
        pio_instr_util::exec_jmp(&mut self.sm, self.write_start);
        self.sm.clear_irq(0);
        self.sm.set_enable(true);

        self.sm.dma_push(self.dma_write.reborrow(), words).await;
        self.sm.wait_irq(0).await;

        assert!(self.sm.tx_level() == 0);
        assert!(!self.sm.has_tx_overflowed());

        Ok(())
    }
}

impl<'a, SM, ChW, ChR> SpiBusRead<u32> for PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    async fn read(&mut self, words: &mut [u32]) -> Result<(), Self::Error> {
        //debug!("SPI read: {}", words.len());
        self.sm.set_enable(false);

        self.sm.restart();
        self.sm.clear_fifos();
        pio_instr_util::set_pindir(&mut self.sm, 0b0);
        self.sm.clkdiv_restart();

        pio_instr_util::set_x(&mut self.sm, words.len() as u32 * 32 - 1);
        pio_instr_util::exec_jmp(&mut self.sm, self.read_start);
        self.sm.clear_irq(0);
        self.sm.set_enable(true);
        self.sm.dma_pull(self.dma_read.reborrow(), words).await;
        self.sm.wait_irq(0).await;
        assert!(self.sm.rx_level() == 0);
        //debug!("SPI read done: {:08x}", words[0]);
        assert!(!self.sm.has_rx_underflowed());
        Ok(())
    }
}

impl<'a, SM, ChW, ChR> SpiBusFlush for PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    async fn flush(&mut self) -> Result<(), Self::Error> {
        //debug!("SPI flush");
        self.sm.wait_irq(0).await;
        Ok(())
    }
}

impl<'a, SM, ChW, ChR> ErrorType for PioSpi<'a, SM, ChW, ChR>
where
    SM: PioStateMachine,
    ChW: Channel,
    ChR: Channel,
{
    type Error = PioSpiError;
}
