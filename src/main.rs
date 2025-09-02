use std::{
    cell::RefCell,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use elm327::Elm327;

use embedded_svc::http::Headers;

use esp_idf_svc::wifi::AuthMethod;
use esp_idf_svc::{
    bt::spp::{self, EspSpp, SppConfig},
    bt::{
        gap::{DiscoveryMode, EspGap},
        reduce_bt_memory, BdAddr, BtClassic, BtDriver,
    },
    espnow::{EspNow, PeerInfo},
    eventloop::EspSystemEventLoop,
    hal::gpio::PinDriver,
    http::{server::EspHttpServer, Method},
    io::Write,
    nvs::{EspDefaultNvsPartition, EspNvs},
    sys::{
        esp, esp_bt_gap_set_security_param, esp_bt_sp_param_t_ESP_BT_SP_IOCAP_MODE,
        ESP_BT_IO_CAP_NONE,
    },
    wifi::{self, BlockingWifi, EspWifi},
};
use esp_idf_svc::{hal::prelude::Peripherals, http::server::Configuration};

use log::*;
use spp_handler::SppHandler;

use error::{start_led_blink, ErrorInd, LedBlink};

//use crate::error::MSG_LOGGER;

mod bt;
mod elm327;
mod error;
// mod espidf;
mod spp_handler;

// OBDLink MX+ mac
static BD_ADDR: BdAddr = BdAddr::from_bytes([0x00, 0x04, 0x3E, 0x83, 0xFC, 0x98]);

const ESPNOW_CHANNEL: u8 = 1;
const NVS_DISC_FAIL_COUNT: &str = "dsc_fail_cnt";
const SSID: &str = "OBD-ESPWIFI";
// const PASSWORD: &str = "123456789";

/// OBDLink MX+ BT Classic to HTTP interface. Takes simple HTTP requests for ELM327 commands and
/// returns the result.
///
/// Establishes the BT connection, sets up the ELM327, and then joins the WIFI AP (LCD). Once joined
/// and the HTTP handler is ready an ESPNOW message is broadcast with our IP address and we can
/// start servicing ELM327 requests.
fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    // esp_idf_svc::log::EspLogger::initialize_default();
    esp_idf_svc::log::init(LevelFilter::Debug);

    // esp_idf_svc::log::set_target_level("esp_dev", LevelFilter::Debug)?;
    // esp_idf_svc::log::set_target_level("esp_dev::espidf::spp", LevelFilter::Debug)?;
    // esp_idf_svc::log::set_target_level("esp_dev::spp_handler", LevelFilter::Debug)?;
    // esp_idf_svc::log::set_target_level("esp_dev::elm327", LevelFilter::Debug)?;
    // esp_idf_svc::log::set_target_level("esp_dev::espnow", LevelFilter::Debug)?;

    info!("Starting...");

    // unsafe {
    //     heap_caps_print_heap_info(MALLOC_CAP_DEFAULT);
    // }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let led = PinDriver::output(peripherals.pins.gpio2)?;

    let led_blink = start_led_blink(led);

    let modem = RefCell::new(peripherals.modem);

    reduce_bt_memory(modem.borrow_mut())?;

    // unsafe {
    //     heap_caps_print_heap_info(MALLOC_CAP_DEFAULT);
    // }

    //========
    // The ordering of peripheral startup and using HTTP instead of ESPNOW is based around
    // BT/WIFI co-existence. ESPNOW rx proved to be very unreliable due to the modem switching
    // causing many timeouts. ESPNOW is a non guaranteed delivery which required a lot of
    // handling and retries to make sure an elm request was replied to. Also, some elm responses
    // can exceed the ESPNOW message length which makes an even more complicated protocol on top of
    // guaranteed delivery.
    //========

    //-----
    // NVS
    //-----
    // Store the BT discovery failure count, sometimes discovery will fail so we should
    // try again but don't continually reboot and discover
    let elm_nvs = Arc::new(EspNvs::new(nvs.clone(), "elm_ns", true)?);

    //-----------
    // BLUETOOTH
    //-----------
    let driver = BtDriver::<BtClassic>::new(modem.borrow_mut(), Some(nvs.clone()))?;

    driver.set_device_name("OBD-ESP32")?;

    info!("Bluetooth initialized");

    let gap = EspGap::new(&driver)?;

    info!("GAP created");

    let spp_config = SppConfig {
        mode: spp::Mode::Cb,
        enable_l2cap_ertm: true,
        tx_buffer_size: 0,
    };

    let spp = Arc::new(EspSpp::new(&driver, &spp_config)?);

    info!("SPP created");

    unsafe {
        gap.subscribe_nonstatic(|event| bt::handle_gap(&gap, event))?;
    }

    // No IO capability
    // gap.set_ssp_io_cap(IOCapabilities::None)?;
    esp!(unsafe {
        esp_bt_gap_set_security_param(
            esp_bt_sp_param_t_ESP_BT_SP_IOCAP_MODE,
            &ESP_BT_IO_CAP_NONE as *const _ as *mut std::ffi::c_void,
            1,
        )
    })?;

    gap.set_pin("1234")?;
    gap.set_scan_mode(true, DiscoveryMode::Discoverable)?;

    info!("GAP initialized");

    let spp_handler = SppHandler::new(&spp);

    let spp_rem_handle = Arc::clone(&spp_handler.handle);
    let write_buf = Arc::clone(&spp_handler.write_buf);
    let read_buf = Arc::clone(&spp_handler.read_buf);
    let spp_sub = Arc::clone(&spp);
    let elm_nvs_2 = Arc::clone(&elm_nvs);
    let led_blink_2 = led_blink.clone();
    unsafe {
        spp.subscribe_nonstatic(move |event| {
            spp_handler::handle_spp(
                &elm_nvs_2,
                &led_blink_2,
                &spp_sub,
                &spp_rem_handle,
                &write_buf,
                &read_buf,
                event,
            )
        })?;
    }

    spp.start_discovery(&BD_ADDR).error_ind(1)?;

    led_blink.send(LedBlink::Times(1))?;

    //--------
    // ELM327
    //--------
    let elm327: Arc<Mutex<Elm327<'_, BtClassic, &BtDriver<'_, BtClassic>>>> =
        Arc::new(Mutex::new(Elm327::new(spp_handler)));

    elm327.lock().unwrap().setup().error_ind(2)?;

    led_blink.send(LedBlink::Times(2))?;
    info!("ELM327 initialized");

    // Reset the discovery fail count if needed
    if elm_nvs.get_u8(NVS_DISC_FAIL_COUNT)?.is_some_and(|n| n > 0) {
        info!("Resetting discovery fail count");
        let _ = elm_nvs.set_u8(NVS_DISC_FAIL_COUNT, 0);
    }

    //--------------------
    // Start/Connect WIFI
    //--------------------
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem.borrow_mut(), sys_loop.clone(), Some(nvs.clone()))?,
        sys_loop,
    )?;

    let ip_addr = connect_wifi_client(&mut wifi).error_ind(3)?;

    led_blink.send(LedBlink::Times(3))?;

    //--------
    // ESPNOW
    //--------
    pub const BROADCAST: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let espnow = EspNow::take()?;

    espnow.add_peer(PeerInfo {
        peer_addr: BROADCAST,
        channel: ESPNOW_CHANNEL,
        ifidx: esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
        encrypt: false,
        ..Default::default()
    })?;

    //-------------
    // HTTP Server
    //-------------
    info!("Starting service request handler");

    let server_configuration = Configuration {
        stack_size: 4096,
        max_sessions: 4,
        max_open_sockets: 2,
        ..Default::default()
    };

    let mut server = EspHttpServer::new(&server_configuration).context("Failed to create httpd")?;

    /* Handler to get log 'messages'. Not really using it... */
    /*
    server
        .fn_handler::<anyhow::Error, _>("/log", Method::Get, |req| {
            req.into_response(200, Some("OK"), &[("Content-Type", "text/plain")])?
                .write_all(MSG_LOGGER.get_messages().as_bytes())
                .map_err(anyhow::Error::from)
        })
        .context("Register log handler")
        .and(Ok(()))?;
    */

    unsafe {
        server
            .fn_handler_nonstatic::<anyhow::Error, _>("/post", Method::Post, move |mut req| {
                let len = req.content_len().unwrap_or(0) as usize;

                if len > 250 {
                    req.into_status_response(413)?
                        .write_all("Request too big".as_bytes())?;
                    return Ok(());
                }

                led_blink.send(LedBlink::High)?;

                let mut buf = vec![0; len];
                req.read(&mut buf)?;

                let mut elm327 = elm327.lock().unwrap();
                elm327.write_request(&buf)?;

                let req_string = elm327.read_response()?;

                led_blink.send(LedBlink::Low)?;

                let mut resp = req.into_ok_response()?;

                resp.write_all(req_string.as_bytes())?;

                Ok(())
            })
            .context("Register service handler")
            .and(Ok(()))?
    }

    //------------------
    // Off to the races
    //------------------
    // Tell the LCD our IP
    let mut data = heapless::Vec::<u8, 5>::new();
    let _ = data.push(0x01); // simple ID to identify the espnow packet as ready/IP addr
    let _ = data.extend_from_slice(&ip_addr.octets());

    espnow.send(BROADCAST, &data).error_ind(2)?;

    loop {
        thread::sleep(Duration::from_millis(10));
    }
}

fn connect_wifi_client(wifi: &mut BlockingWifi<EspWifi<'_>>) -> Result<Ipv4Addr> {
    let wifi_configuration: wifi::Configuration =
        wifi::Configuration::Client(wifi::ClientConfiguration {
            ssid: SSID.try_into().unwrap(),
            auth_method: AuthMethod::None,
            channel: Some(ESPNOW_CHANNEL),
            ..Default::default()
        });

    wifi.set_configuration(&wifi_configuration)?;

    wifi.start()?;
    info!("Wifi started");

    let mut connect_tries = 3;
    loop {
        connect_tries -= 1;
        match wifi.connect() {
            Ok(_) => {
                break;
            }
            Err(e) => {
                error!("Wifi connect failed: {e}");
                if connect_tries == 0 {
                    Err(e)?;
                }
                thread::sleep(Duration::from_millis(1000));
            }
        };
    }
    info!("Wifi connected");

    wifi.wait_netif_up()?;
    info!("Wifi netif up");

    info!("Connected Wi-Fi with WIFI_SSID `{SSID}`");

    Ok(wifi.wifi().sta_netif().get_ip_info()?.ip)
}
