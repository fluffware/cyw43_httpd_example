#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
use core::fmt::{self, Write as _};
use cyw43::Control;
use defmt::{debug, info, warn};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::pio::{PioPeripherial, PioStateMachine};
use embassy_rp::Peripheral;
use embassy_time::{Duration, Timer};
use embedded_hal_async::spi::ExclusiveDevice;
use embedded_io::asynch::Write;
use httparse::{self, Request};
use pio_test::pio_spi::PioSpi;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        STATIC_CELL.init_with(move || $val)
    }};
}

struct WriteBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> WriteBuf<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    pub fn as_slice(&'a mut self) -> &'a mut [u8] {
        &mut self.buf[..self.len]
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> fmt::Result {
        let left = self.buf.len() - self.len;
        let copy = bytes.len();
        if left >= copy {
            self.buf[self.len..(self.len + copy)].copy_from_slice(bytes);
            self.len += copy;
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

impl<'a> fmt::Write for WriteBuf<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes())
    }
}

async fn handle_request<'a>(req: &'a Request<'a,'a>, status_code: &mut u32, content_type: &mut &str, body: &mut &[u8], led_on: &'a mut bool) {
    if let Some("GET") = req.method {
        if let Some(path) = req.path {
            if let Some(cmd) = path.strip_prefix("/cmd/") {
                *status_code = 204;
                *body = &[0; 0];
		if cmd.starts_with("on") {
		    *led_on = true;
		} else if cmd.starts_with("off") {
		    *led_on = false;
                } else {
                    *status_code = 400;
                    *body =
			"<html><head><title>Illegal request</title></head><body>400 Unknown command</body></html>".as_bytes();
                }
            } else if path.starts_with("/index.html") || path == "/" {
                *status_code = 200;
                *body = include_bytes!("index.html");
            } else if path.starts_with("/style.css") {
                *status_code = 200;
                *body = include_bytes!("style.css");
                *content_type = "text/css";
            }
        }
    } else {
        *status_code = 400;
        *body =
			"<html><head><title>Illegal request</title></head><body>400 Only GET allowed</body></html>".as_bytes();
    }
}
#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDevice<'static>>) -> ! {
    stack.run().await
}
const MAX_TX_BLOCK: usize = 1024;
const MAX_RX_BLOCK: usize = 1024;

#[embassy_executor::task]
async fn setup_task(spawner: Spawner, mut control: Control<'static>) {
    let clm = include_bytes!("../../../cyw43/43439A0_clm.bin");
    let net_device = control.init(clm).await;
    info!("Joining");
    control.join_wpa2(env!("SSID"), env!("PASS")).await;
    let config = embassy_net::ConfigStrategy::Dhcp;
    let seed = 63395997077266;

    let stack = &*singleton!(Stack::new(
        net_device,
        config,
        singleton!(StackResources::<1, 2, 8>::new()),
        seed
    ));
    spawner.spawn(net_task(stack)).unwrap();
    info!("Done");

    control.gpio_set(0, true).await;

    let rx_buffer: &mut [u8; MAX_RX_BLOCK] = singleton!([0; MAX_RX_BLOCK]);
    let tx_buffer: &mut [u8; MAX_TX_BLOCK] = singleton!([0; MAX_TX_BLOCK]);
    let buf: &mut [u8; 4096] = singleton!([0; 4096]);
    let resp: &mut [u8; 8192] = singleton!([0u8; 8192]);
    let mut led_on = true;
    loop {
        let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
        socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));

        info!("Listening on TCP:80...");
        if let Err(e) = socket.accept(80).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());
        let mut buf_end: usize = 0;

        loop {
            let n = match socket.read(&mut buf[buf_end..]).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("read error: {:?}", e);
                    break;
                }
            };

            buf_end += n;
            info!("Buffer size: {}", buf_end);

            let mut headers = [httparse::EMPTY_HEADER; 20];
            let mut req = httparse::Request::new(&mut headers);
            let res = match req.parse(buf) {
                Ok(res) => res,
                Err(_) => {
                    warn!("Parsing request failed");
                    socket.close();
                    continue;
                }
            };
            if res.is_complete() {
                let mut status_code: u32 = 404;
                let mut content_type = "text/html;charset=UTF-8";
                let mut body =
                    "<html><head><title>Not found</title></head><body>404 Not found</body></html>"
                        .as_bytes();
                info!("Method: {:?}", req.method);
                handle_request(&req, &mut status_code, &mut content_type, &mut body, &mut led_on).await;
                buf_end = 0;
		control.gpio_set(0, led_on).await;
                let mut resp_writer = WriteBuf::new(resp);

                write!(
                    resp_writer,
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\n\r\n",
                    status_code,
                    body.len(),
                    content_type,
                )
                .unwrap();

                resp_writer.write_bytes(body).unwrap();
                let mut tx_block: &[u8] = resp_writer.as_slice();
                while tx_block.len() > 0 {
                    let send_block: &[u8];
                    if tx_block.len() > MAX_TX_BLOCK {
                        send_block = &tx_block[..MAX_TX_BLOCK];
                    } else {
                        send_block = tx_block;
                    }
                    match socket.write_all(&send_block).await {
                        Ok(()) => {}
                        Err(e) => {
                            warn!("write error: {:?}", e);
                            break;
                        }
                    }
                    debug!("Sent: {}", send_block.len());
                    tx_block = &tx_block[send_block.len()..]
                }
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let pio = p.PIO0;
    let (_, sm0, ..) = pio.split();

    info!("Pio: {}, SM: {}", sm0.pio_no(), sm0.sm_no());

    let wl_on = Output::new(p.PIN_23, Level::Low);
    //let wl_on = Output::new(p.PIN_4, Level::Low);
    Timer::after(Duration::from_millis(150)).await;

    let fw = include_bytes!("../../../cyw43/43439A0.bin");

    let mut bus = PioSpi::new(
        sm0,
        p.PIN_29,
        p.PIN_24,
        p.DMA_CH0.into_ref(),
        p.DMA_CH1.into_ref(),
    );
    //let mut bus = PioSpi::new(&pio, &sm0, p.PIN_1, p.PIN_0);
    bus.set_data_level(Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    //let cs = Output::new(p.PIN_2, Level::High);
    let spi = ExclusiveDevice::new(bus, cs);
    let state = singleton!(cyw43::State::new());
    info!("Initializing");
    let (control, runner) = cyw43::new(state, wl_on, spi, fw).await;
    spawner.spawn(setup_task(spawner, control)).unwrap();
    runner.run().await;
}
