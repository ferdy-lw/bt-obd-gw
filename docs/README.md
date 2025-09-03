# ESP32 BT Classic ELM327 OBD to HTTP
 
This ESP Connects to a BT classic OBD dongle, OBDLink MX+, using SPP (serial port profile) and starts an HTTP post endpoint that allows the caller to send commands to the OBD port.

This is part of a [car dashboard](https://github.com/ferdy-lw/pm-guage), specifically for a Promaster van, that uses an LCD to show engine temperatures and other OBD pid data. The LCD is a Waveshare 5" ESP32 s3, which does not support BT classic, unlike the ESP32 which supports both classic and BLE. 
Instead of buying a new dongle with BLE and incorporating the OBD logic into the LCD/s3 this gateway was written so the s3 could talk to the MX+.
  
There will be some delay with hops over http to BT and the occasional connection issues but high speed isn't really an issue, only the temperature pids are sub second and other data is only requested every few seconds to few minutes.

# Design

The project is built using the rust wrappers `esp-idf-svc` on a ESP32 wroom dev kit.

## esp-idf-svc and SPP

BT classic support in esp-idf-svc now includes SPP in the master [branch](https://github.com/esp-rs/esp-idf-svc/pull/606).

 ## WIFI

 The LCD acts as an AP to which the gateway connects to get an IP address that is then used by the LCD to send http requests. 
 
 ## ESPNOW

 I initially intended to use ESPNOW instead of HTTP for the gateway but after much trial and error it seems that ESPNOW rx is very unstable when using BT (coexistence), at least when using classic. 
 My guess is the modem is constantly serving the classic connection and the wifi sleeps too long missing the ESPNOW packets. A sender will get an error that the packet was not delivered, can resend the packet and eventually it will be delivered, typically after 2 or 3 retries. 
 Transmits typically work every time. Perhaps under BLE, which shouldn't require the modem as much, receive may work better.

 Some OBD responses (multiframe) are greater than 250 bytes which means splitting the response over several espnow packets, or a frame per espnow packet, which will require reassembly. 
 With the issue around non guaranteed delivery it would have made the protocol overly complicated whereas HTTP has better error handling and the entire OBD request/response can be done over a single call.

 `experimental` contains some attempt at a guaranteed delivery espnow protocol using retries and ack packets which basically worked but I gave up on it.

 espnow is used once the gateway is all setup and ready to receive obd commands. 
 An espnow packet with the gateway's IP address is broadcast which is picked up by the LCD so it knows the gateway is ready and then starts sending obd requests.
 If the gateway disconnects from the AP the LCD will stop sending requests and will wait for the espnow IP packet again.

 ## ELM327

 The gateway is used for getting pid requests from the LCD and returning the entire raw response in one request/response cycle, just like issuing a direct elm327 command except the returns `\r` and end `>` chars are not included. 
 The caller is responsible for converting the 'hex' response into data bytes and reconstituting multiframe elm responses. 
The gateway sets up the elm327, including the OBD protocol for a Promaster (29 bit, 500k) `Elm327.setup()`. The Promaster uses a single ECU, combined ECM/TCM, for all pids so the header can just be set once for all pid requests.

## BT Pairing

The OBDLink MX+ is BT classic and uses a pin of `1234`. When using no IO capabilities it will accept a pairing when the button on the dongle is pressed. The pairing process only needs to be done once when the ESP32 tries to connect the first time - press the button, boot the ESP32, and it should connect. After that the ESP32 will simply connect to the MX+ on boot as it stores device pairing info in nvs.
Not all events in `bt.handle_gap` are triggered, some of them I wrote for trial and error.

Sometimes connection with the MX+ will fail, either the MX+ is in a bad state or the BT connection fails, but a reboot of the ESP usually reconnects the next time. There is an auto reboot, via panic, if the initial SPP discovery connect fails, and this is only done a couple of times so it won't enter a boot loop.