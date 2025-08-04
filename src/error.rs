use std::{
    sync::{
        mpsc::{self, SyncSender},
        OnceLock,
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use esp_idf_svc::hal::gpio::{self, PinDriver};
use log::error;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReadObdError {
    #[error("Device IO Error")]
    IOError(#[from] std::io::Error),
}

pub enum LedBlink {
    Error(u8),
    Times(u8),
    High,
    Low,
}

static ERROR_IND_SENDER: OnceLock<SyncSender<LedBlink>> = OnceLock::new();

pub trait ErrorInd<T, E> {
    fn error_ind(self, blink: u8) -> std::result::Result<T, E>;
}

impl<T, E> ErrorInd<T, E> for std::result::Result<T, E> {
    fn error_ind(self, blink: u8) -> Result<T, E> {
        if self.is_err() {
            if let Some(sender) = ERROR_IND_SENDER.get() {
                let _ = sender.send(LedBlink::Error(blink));
            }
        }
        self
    }
}

pub fn start_led_blink(
    mut led: PinDriver<'static, gpio::Gpio2, gpio::Output>,
) -> SyncSender<LedBlink> {
    let (led_blink_tx, led_blink_rx) = mpsc::sync_channel(1);

    let _ = ERROR_IND_SENDER.set(led_blink_tx.clone());

    thread::spawn(move || loop {
        let mut forever = false;
        let mut count = 0;

        if let Ok(blink) = led_blink_rx.recv() {
            match blink {
                LedBlink::Error(n) => {
                    count = n;
                    forever = true;
                }
                LedBlink::Times(n) => count = n,
                LedBlink::High => led.set_high().unwrap_or_default(),
                LedBlink::Low => led.set_low().unwrap_or_default(),
            }
        }

        if count > 0 {
            loop {
                for _ in 0..count {
                    let _ = led.set_high();
                    thread::sleep(Duration::from_millis(500));
                    let _ = led.set_low();
                    thread::sleep(Duration::from_millis(250));
                }
                if !forever {
                    break;
                }
                thread::sleep(Duration::from_millis(2000));
            }
        }
    });

    led_blink_tx
}

// pub fn stop_error_ind() {
//     if let Some(sender) = ERROR_IND_SENDER.get() {
//         let _ = sender.send(10);
//     }
// }

// A facility to log messages that can be retrieved using a HTTP call.
// Not really using it...
/*
pub static MSG_LOGGER: LazyLock<MsgLogger> = LazyLock::new(MsgLogger::start);

pub struct MsgLogger {
    tx: SyncSender<String>,
    messages: Arc<Mutex<CircularBuffer<10, String>>>,
}

impl MsgLogger {
    pub fn log(&self, msg: String) {
        let _ = self.tx.send(msg);
    }

    pub fn get_messages(&self) -> String {
        let mut messages = String::new();

        let stack_free =
            unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) } as i32;
        let used: i32 = 4096 - stack_free;

        let heap = unsafe {
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_DEFAULT)
        };

        messages.push_str(&format!(
            "stack use high water mark {used}/4096\r\nHeap free {heap}\r\n"
        ));

        for msg in self.messages.lock().unwrap().iter() {
            messages += msg;
            messages.push_str("\r\n");
        }

        messages
    }

    fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel(10);

        let mut buffer = CircularBuffer::new();
        buffer.push_back("Start of log".to_owned());

        let messages = Arc::new(Mutex::new(buffer));

        let messages1 = Arc::clone(&messages);
        thread::spawn(move || loop {
            match rx.recv() {
                Ok(msg) => {
                    messages1.lock().unwrap().push_back(msg);
                }
                Err(e) => {
                    error!("Message logger sender disconnected? {e}");
                    break;
                }
            }
        });

        MsgLogger { tx, messages }
    }
}
*/
