use circular_buffer::CircularBuffer;
use esp_idf_svc::{
    bt::{BtClassicEnabled, BtDriver},
    nvs::{EspNvs, NvsDefault},
    sys::EspError,
};
use std::{
    borrow::Borrow,
    io::{self, Read, Write},
    sync::{
        atomic::{self, AtomicU32},
        mpsc::SyncSender,
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::Result;

use crate::{
    error::LedBlink,
    spp::{self, EspSpp, SppEvent},
};
use crate::{BD_ADDR, NVS_DISC_FAIL_COUNT};
use log::*;

const WRITE_BUF_SIZE: usize = 250;
const READ_BUF_SIZE: usize = 500;

type WriteBuffer = Arc<Mutex<Box<CircularBuffer<WRITE_BUF_SIZE, u8>>>>;
type ReadBuffer = Arc<(Mutex<DataBuffer>, Condvar)>;

pub struct DataBuffer {
    data: Box<CircularBuffer<READ_BUF_SIZE, u8>>,
    available: bool,
}

pub struct SppHandler<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    spp: &'d EspSpp<'d, M, T>,
    pub handle: Arc<AtomicU32>,
    pub write_buf: WriteBuffer,
    pub read_buf: ReadBuffer,
}

impl<'d, M, T> Write for SppHandler<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    /// Write some data to the OBDLink. Will not block.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.extend_write_buf(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.flush()?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let handle = self.handle.load(atomic::Ordering::Relaxed);
        if handle > 0 {
            let mut write_buf = self.write_buf.lock().unwrap();

            if let Err(err) = self.spp.write(handle, write_buf.make_contiguous()) {
                error!("Failed to write: {err}");
                write_buf.clear();

                return Err::<(), io::Error>(io::Error::new::<EspError>(
                    io::ErrorKind::ConnectionReset,
                    err,
                ));
            }
        }

        Ok(())
    }
}

impl<'d, M, T> Read for SppHandler<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    /// Read a response from the OBDLink. Will BLOCK until there is some data available
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (read_buf, cvar) = &*self.read_buf;

        // lock read buf
        let mut read_buf = read_buf.lock().unwrap();

        while read_buf.data.is_empty() {
            read_buf = cvar
                .wait_while(read_buf, |data| !data.available)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Poisoned"))?;

            read_buf.available = false; // might be false wake up

            debug!("read buf ({})", read_buf.data.len());
        }

        let nread = read_buf.data.read(buf)?;

        read_buf.available = !read_buf.data.is_empty();

        Ok(nread)
    }
}

impl<'d, M, T> SppHandler<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    pub fn new(spp: &'d EspSpp<'d, M, T>) -> Self {
        Self {
            spp,
            handle: Arc::new(AtomicU32::new(0)),
            write_buf: Arc::new(Mutex::new(CircularBuffer::boxed())),
            read_buf: Arc::new((
                Mutex::new(DataBuffer {
                    data: CircularBuffer::boxed(),
                    available: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn write_elm_request(&mut self, request: &[u8]) -> Result<()> {
        self.extend_write_buf(request)?;

        self.write_all(b"\r")?;

        Ok(())
    }

    fn extend_write_buf(&self, buf: &[u8]) -> Result<()> {
        if buf.len() > WRITE_BUF_SIZE {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buf too large. max ({WRITE_BUF_SIZE})",
            ))?;
        };

        let mut write_buf = self.write_buf.lock().unwrap();

        write_buf.extend_from_slice(buf);

        Ok(())
    }
}

impl<'d, M, T> Drop for SppHandler<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    fn drop(&mut self) {
        let handle = self.handle.load(atomic::Ordering::Relaxed);
        if handle > 0 {
            self.spp.disconnect(handle).unwrap();
        }
    }
}

/// BT Serial Port Profile callback handler
pub fn handle_spp<'d, M, T>(
    elm_nvs: &EspNvs<NvsDefault>,
    led_blink: &SyncSender<LedBlink>,
    spp: &EspSpp<'d, M, T>,
    rem_handle: &AtomicU32,
    write_buf: &Mutex<Box<CircularBuffer<WRITE_BUF_SIZE, u8>>>,
    read_buf: &(Mutex<DataBuffer>, Condvar),
    event: SppEvent<'_>,
) where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    match event {
        SppEvent::DiscoveryComp {
            status,
            scn_num,
            scn,
            service_name,
        } => {
            if status == spp::Status::Success {
                debug!(
                    "Event: DisComp, scn_num ({scn_num}), scn ({:?}), service_name ({:?})",
                    scn, service_name
                );

                if let Err(err) = spp.connect(
                    spp::Security::Authenticate,
                    spp::Role::Master,
                    scn[0],
                    &BD_ADDR,
                ) {
                    error!("Event: DisComp failed to dispatch spp.connect, {err}")
                }
            } else {
                error!("Event: DisComp FAILED, status {:?}", status);

                // Panic so we can try discover again, but only do this a few times so we don't go into a
                // boot loop
                let _ = led_blink.send(LedBlink::Times(4));
                thread::sleep(Duration::from_millis(3500)); // wait for the leds...

                if let Some(n) = elm_nvs
                    .get_u8(NVS_DISC_FAIL_COUNT)
                    .unwrap_or(Some(0))
                    .or(Some(0))
                    .filter(|n| n <= &2)
                {
                    info!("Fail count {n}");
                    let _ = elm_nvs.set_u8(NVS_DISC_FAIL_COUNT, n + 1);
                    panic!("Failed to discover OBDLink, rebooting...");
                }

                info!("Rebooted too many times, not rebooting again");
                let _ = led_blink.send(LedBlink::Error(4));
            }
        }
        SppEvent::Open {
            status,
            handle,
            fd,
            rem_bda,
        } => {
            if status == spp::Status::Success {
                debug!("Event: Open, handle ({handle}), fd ({fd}), rem_bda ({rem_bda})");

                rem_handle.store(handle, atomic::Ordering::Relaxed);

                // If we have data, write now...
                let mut write_buf = match write_buf.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        error!("Event: Open, write buf poisoned, acquiring anyway...");
                        poisoned.into_inner()
                    }
                };

                if !write_buf.is_empty() {
                    debug!("writing... {} bytes", write_buf.len());
                    if let Err(err) = spp.write(handle, write_buf.make_contiguous()) {
                        error!("Event: Open write failed {err}");
                    }
                }
            } else {
                error!("Event: Open FAILED, status {:?}", status);
            }
        }
        SppEvent::DataInd {
            status,
            handle,
            length,
            data,
        } => {
            if status == spp::Status::Success {
                debug!("Event: DataInd, handle ({handle}), data ({:?})", data);

                let (read_buf, cvar) = read_buf;

                // get read lock
                let mut read_buf = match read_buf.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        error!("Event: DataInd, read buf poisoned, acquiring anyway...");
                        poisoned.into_inner()
                    }
                };

                let max_length = READ_BUF_SIZE - read_buf.data.len();
                let read_length: usize = length as _;

                if read_length > max_length {
                    error!(
                        "Read buffer overflow, total bytes would be ({})",
                        read_buf.data.len() + read_length
                    );
                };

                read_buf
                    .data
                    .extend_from_slice(unsafe { core::slice::from_raw_parts(data, read_length) });

                read_buf.available = true;
                cvar.notify_all();
            } else {
                error!("Event: DataInd FAILED, status {:?}", status);
            }
        }
        SppEvent::Write {
            status,
            handle,
            length,
            cong,
        } => {
            let mut write_buf = match write_buf.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    error!("Event: Write, write buf poisoned, acquiring anyway...");
                    poisoned.into_inner()
                }
            };

            if status == spp::Status::Success {
                debug!(
                    "Event: Write, handle {handle}, length {length}, cong {cong}; write buf {}",
                    write_buf.len()
                );

                let length: usize = length as _;
                let mut write_buf_length = write_buf.len();
                if length > write_buf_length {
                    warn!(
                        "Write length more than buffer. buffer_len ({write_buf_length}), written ({length})"
                    );
                    write_buf_length = length;
                }
                if length < write_buf_length {
                    warn!(
                        "Write not fully written. buffer_len ({write_buf_length}), written ({length})"  
                    );
                }
                write_buf.truncate_front(write_buf_length - length);
            } else {
                error!(
                    "Event: Write FAILED, status {:?} write buf {}",
                    status,
                    write_buf.len()
                );
            }

            // If not congested and there is more data to write...
            if !cong && !write_buf.is_empty() {
                if let Err(err) = spp.write(handle, write_buf.make_contiguous()) {
                    error!("Event: Write, not cong but write again failed {err}");
                }
            }
        }
        SppEvent::Cong {
            status,
            handle,
            cong,
        } => {
            if status == spp::Status::Success {
                debug!("Event: Cong, handle {handle}, cong {cong}");

                let mut write_buf = match write_buf.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        error!("Event: Cong, write buf poisoned, acquiring anyway...");
                        poisoned.into_inner()
                    }
                };

                if !cong && !write_buf.is_empty() {
                    if let Err(err) = spp.write(handle, write_buf.make_contiguous()) {
                        error!("Event: Cong write failed {err}");
                    }
                }
            } else {
                error!("Event: Cong FAILED, status {:?}", status);
            }
        }
        SppEvent::Close {
            status,
            port_status,
            handle,
            async_,
        } => {
            if status == spp::Status::Success {
                debug!("Event: Close, handle {handle}, port_status {port_status}, async {async_}");
            } else {
                error!("Event: Close FAILED, status {:?}", status);
            }

            rem_handle.store(0, atomic::Ordering::Relaxed);
        }
        _ => (),
    }
}
