use std::borrow::Borrow;

use esp_idf_svc::{
    bt::{
        gap::{DeviceProp, EspGap, GapEvent},
        BtClassicEnabled, BtDriver,
    },
    sys::{esp, esp_bt_gap_ssp_confirm_reply},
};

use log::*;

/// BT GAP callback handler
pub fn handle_gap<'d, M, T>(gap: &EspGap<'d, M, T>, event: GapEvent<'_>)
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    match event {
        GapEvent::DeviceDiscovered { bd_addr, props } => {
            info!("GAP: Found device: {bd_addr:?}");

            for prop in props {
                info!("Prop: {:?}", prop.prop());

                if let DeviceProp::Eir(eir) = prop.prop() {
                    // let eir: Eir = eir as _;
                    info!(
                        "  Short Local Name: {}, Local Name: {}",
                        eir.short_local_name::<M, T>().unwrap_or("-"),
                        eir.local_name::<M, T>().unwrap_or("-")
                    );
                }
            }

            //let _ = gap.stop_discovery();
        }
        GapEvent::SspPasskeyRequest { bd_addr } => {
            info!("GAP: pass key request");
            gap.reply_passkey(&bd_addr, Some(123456)).unwrap();
        }
        GapEvent::PairingUserConfirmationRequest { bd_addr, number } => {
            info!("GAP: ssp pin confirm: {number}");
            // gap.reply_ssp_confirm(&bd_addr, true).unwrap();
            esp!(unsafe { esp_bt_gap_ssp_confirm_reply(&bd_addr as *const _ as *mut _, true) })
                .unwrap();
        }
        GapEvent::AuthenticationCompleted {
            bd_addr,
            status,
            device_name,
        } => {
            info!("GAP: Authcomplete, {bd_addr}, status {status:?}, device {device_name}");
        }
        GapEvent::PairingPinRequest {
            bd_addr,
            min_16_digit,
        } => {
            // ESP_BT_GAP_PIN_REQ_EVT - for variable pin
            info!("GAP: PinRequest, {bd_addr}, 16pin {min_16_digit}");

            if min_16_digit {
                error!("Min 16 pin not supported");
            } else {
                gap.reply_variable_pin(&bd_addr, Some(&[1, 2, 3, 4]))
                    .unwrap();
            }
        }
        _ => (),
    }
}
