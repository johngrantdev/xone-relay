//! xone-relay: forward Xbox evdev devices to a receiver over TCP.
//!
//! The binary has two modes.
//!
//! `--sender` runs on the host that owns the Xbox Wireless Adapter. It
//! discovers every matching evdev controller. It sends each controller with
//! a stable slot. It forwards force-feedback requests back to evdev.
//!
//! `--receiver` runs on the receiver machine. It creates one uinput device
//! per slot. It forwards the receiver's force-feedback requests to the sender.
//!
//! It uses only `libc`. It has no async runtime, no serde, and no libevdev or
//! libudev.

mod config;
mod linux;
mod protocol;
mod receiver;
mod sender;
mod uinput;

use crate::config::{DEFAULT_DEVICE_NAME, DEFAULT_PORT};

fn usage() -> ! {
    eprintln!(
        "usage: xone-relay --sender|--receiver [options]\n\
         \n\
         --sender              forward matching local controllers to a receiver\n\
         --receiver            recreate the controllers locally with uinput\n\
         --receiver-address HOST\n\
                              sender: receiver address (overrides config)\n\
         --port N              TCP port (default {DEFAULT_PORT})\n\
         --device-name NAME    sender: override the evdev name filter for\n\
                              diagnostics (default \"{DEFAULT_DEVICE_NAME}\")\n\
         --uinput-name NAME    receiver: override the created device name"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut sender = false;
    let mut receiver = false;
    let mut port: Option<u16> = None;
    let mut device_name: Option<String> = None;
    let mut receiver_address: Option<String> = None;
    let mut uinput_name: Option<String> = None;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--sender" => sender = true,
            "--receiver" => receiver = true,
            "--help" | "-h" => usage(),
            "--port" => {
                index += 1;
                port = Some(
                    args.get(index)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_else(|| usage()),
                );
            }
            "--device-name" => {
                index += 1;
                device_name = Some(args.get(index).cloned().unwrap_or_else(|| usage()));
            }
            "--receiver-address" => {
                index += 1;
                receiver_address = Some(args.get(index).cloned().unwrap_or_else(|| usage()));
            }
            "--uinput-name" => {
                index += 1;
                uinput_name = Some(args.get(index).cloned().unwrap_or_else(|| usage()));
            }
            other => {
                eprintln!("xone-relay: unknown argument {other}");
                usage();
            }
        }
        index += 1;
    }

    if sender == receiver {
        usage();
    }
    linux::ignore_sigpipe();

    if sender {
        sender::run_sender(receiver_address, port, device_name);
    } else {
        receiver::run_receiver(port, uinput_name);
    }
}
