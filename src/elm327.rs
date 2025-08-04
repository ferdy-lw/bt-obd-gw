use anyhow::{Context, Result};
use esp_idf_svc::bt::{BtClassicEnabled, BtDriver};
use log::{debug, error, trace};
use std::borrow::Borrow;
use std::io::Read;

// use crate::command::OBDResponse;
use crate::error::ReadObdError;
use crate::spp_handler::SppHandler;

pub struct Elm327<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    port: SppHandler<'d, M, T>,
}

impl<'d, M, T> Elm327<'d, M, T>
where
    M: BtClassicEnabled,
    T: Borrow<BtDriver<'d, M>>,
{
    pub fn new(handler: SppHandler<'d, M, T>) -> Self {
        Elm327 { port: handler }
    }

    pub fn setup(&mut self) -> Result<()> {
        // Turn off any monitoring, and wait for response line
        self.write_request(b"??")?;
        self.read_response()?;

        // Reset elm327
        self.write_request(b"ATZ")?;
        self.read_response()?;

        // Turn off echo
        self.write_request(b"ATE 0")?;
        self.read_response()?;

        // RAM Promaster protocol - ISO 15765-4 CAN (29 bit ID, 500 Kbaud)
        self.write_request(b"STP 34")?;
        self.read_response()?;

        // Get Version
        self.write_request(b"ATI")?;
        self.read_response()?;

        // Display headers
        self.write_request(b"ATH 1")?;
        self.read_response()?;

        // Auto formatting
        self.write_request(b"ATCAF 1")?;
        self.read_response()?;

        // Use spaces
        self.write_request(b"ATS 1")?;
        self.read_response()?;

        // So far, all service requests are for module 10
        self.write_request(b"ATSH DA10F1")?;
        self.read_response()?;

        Ok(())
    }

    /// Write the request to the OBDLink
    pub fn write_request(&mut self, request: &[u8]) -> Result<()> {
        debug!("Write string ({})", String::from_utf8_lossy(request));

        self.port.write_elm_request(request)
    }

    /// Read a complete OBDLink response. Will block until we get the total response, which
    /// will not include the trailing '>' and '\r'.
    pub fn read_response(&mut self) -> Result<String> {
        let mut response: Vec<u8> = Vec::new();

        let mut loop_count = 0;
        loop {
            loop_count += 1;
            if loop_count == 50 {
                error!("Read response loop count exceeded! ({loop_count})");
                break;
            }

            let mut buf = [0u8; 20];

            let bytes_read = match self.port.read(&mut buf) {
                Ok(n) => n,
                Err(err) => {
                    trace!("Read error {:?}", err);
                    Err(ReadObdError::IOError(err)).context("read data")?
                }
            };

            trace!("Response buffer ({:?})", &buf[..bytes_read]);

            for b in &buf[..bytes_read] {
                if *b != b'\r' && *b != b'\n' && *b != b'>' {
                    response.push(*b);
                }
            }

            if bytes_read > 0 && buf[bytes_read - 1] == b'>' {
                break;
            }
        }

        let response = String::from_utf8(response)?;

        debug!("Response string ({response})");

        // Send data to the ESPNOW handler via channel

        Ok(response)
    }
}
