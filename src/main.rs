#![no_std]
#![no_main]

use defmt::*;
use defmt_rtt as _;
use embassy_executor::Executor;
use embassy_rp::{
    bind_interrupts,
    gpio::{Input, Level, Output, Pull},
    multicore::{Stack, spawn_core1},
    peripherals::PIO0,
    pio::{InterruptHandler, Pio},
    pio_programs::ws2812::{PioWs2812, PioWs2812Program},
    spi,
    spi::Spi,
};
use embassy_time::{Duration, Ticker};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_sdmmc::sdcard::{DummyCsPin, SdCard};
use panic_probe as _;
use smart_leds::RGB8;
use static_cell::StaticCell;

struct DummyTimesource();
impl embedded_sdmmc::TimeSource for DummyTimesource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        embedded_sdmmc::Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

/// Input a value 0 to 255 to get a color value
/// The colours are a transition r - g - b - back to r.
fn wheel(mut wheel_pos: u8) -> RGB8 {
    wheel_pos = 255 - wheel_pos;
    if wheel_pos < 85 {
        return (255 - wheel_pos * 3, 0, wheel_pos * 3).into();
    }
    if wheel_pos < 170 {
        wheel_pos -= 85;
        return (0, wheel_pos * 3, 255 - wheel_pos * 3).into();
    }
    wheel_pos -= 170;
    (wheel_pos * 3, 255 - wheel_pos * 3, 0).into()
}

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"RP2350 Metro Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"Adafruit Metro RP2350 Full Featured Example"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

static mut CORE1_STACK: Stack<4096> = Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

#[cortex_m_rt::entry]
fn main() -> ! {
    info!("Init");
    let p = embassy_rp::init(Default::default());

    // Onboard LED Setup (PIN 23)
    let led = Output::new(p.PIN_23, Level::Low);

    // Onbaord Button Setup (PIN 24)
    let btn = Input::new(p.PIN_24, Pull::Up);

    // NeoPixel Setup (PIN 25)
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let program = PioWs2812Program::new(&mut common);
    // For the Adafruit RP2350 Metro the NeoPixel PIN is 25.
    let neo = PioWs2812::new(&mut common, sm0, p.DMA_CH0, p.PIN_25, &program);

    spawn_core1(
        p.CORE1,
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| unwrap!(spawner.spawn(core1_task(btn, led))));
        },
    );

    // SD Card
    // Onboard Button Setup (PIN 40)
    let sd_card_detect = Input::new(p.PIN_40, Pull::Up);
    if sd_card_detect.is_low() {
        info!("SD Card Present");

        // SPI clock needs to be running at <= 400kHz during initialization
        let mut config = spi::Config::default();
        config.frequency = 400_000;
        let spi = Spi::new_blocking(p.SPI0, p.PIN_34, p.PIN_35, p.PIN_36, config);
        // Use a dummy cs pin here, for embedded-hal SpiDevice compatibility reasons
        let spi_dev = ExclusiveDevice::new_no_delay(spi, DummyCsPin);
        // Real cs pin
        let cs = Output::new(p.PIN_39, Level::High);

        let sdcard = SdCard::new(spi_dev, cs, embassy_time::Delay);
        info!("Card size is {} bytes", sdcard.num_bytes().unwrap());

        // Now that the card is initialized, the SPI clock can go faster
        let mut config = spi::Config::default();
        config.frequency = 16_000_000;
        sdcard.spi(|dev| dev.bus_mut().set_config(&config));

        // Now let's look for volumes (also known as partitions) on our block device.
        // To do this we need a Volume Manager. It will take ownership of the block device.
        let mut volumes = embedded_sdmmc::VolumeManager::new(sdcard, DummyTimesource());

        // Try and access Volume 0 (i.e. the first partition).
        // The volume object holds information about the filesystem on that volume.
        let volume0 = &volumes.open_volume(embedded_sdmmc::VolumeIdx(0)).unwrap();
        info!("Volume 0: {:?}", defmt::Debug2Format(&volume0));
    } else {
        info!("No SD Card Present");
    }

    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| unwrap!(spawner.spawn(core0_task(neo))));
}

#[embassy_executor::task]
async fn core0_task(mut neo: PioWs2812<'static, PIO0, 0, 1>) {
    info!("Hello from core 0");

    // Loop forever making RGB values and pushing them out to the WS2812.
    let mut ticker = Ticker::every(Duration::from_millis(25));

    // NeoPixel Setup
    const NUM_LEDS: usize = 1;
    let mut data = [RGB8::default(); NUM_LEDS];

    loop {
        info!("Core 0: NeoPixel Loop");
        for j in 0..(256 * 3) {
            for i in 0..NUM_LEDS {
                data[i] = wheel((((i * 256) as u16 / NUM_LEDS as u16 + j as u16) & 255) as u8);
            }
            neo.write(&data).await;
            ticker.next().await;
        }
    }
}

#[embassy_executor::task]
async fn core1_task(btn: Input<'static>, mut led: Output<'static>) {
    info!("Hello from core 1");

    let mut btn_state = Level::High;
    loop {
        let btn_level = btn.get_level();

        if btn_state != btn_level {
            if btn_level.into() {
                info!("Core 1: BOOT BTN Released");
            } else {
                info!("Core 1: BOOT BTN Pressed");
            }
            btn_state = btn_level;
        }

        if btn_level.into() {
            led.set_low();
        } else {
            led.set_high();
        }
    }
}
