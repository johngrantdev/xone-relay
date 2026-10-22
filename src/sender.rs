use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::config::{Config, DEFAULT_DEVICE_NAME};
use crate::linux::{
    close_fd, connect_tcp, erase_ff_effect, open_event_rw, read_device_info, sysfs_device_name,
    tcp_tune, upload_ff_effect, write_ff_event, FfEffect, EVENT_SIZE, FF_AUTOCENTER, FF_GAIN,
    FF_RUMBLE,
};
use crate::protocol::{events_are_aligned, read_frame, write_frame, Message, PROTOCOL_VERSION};

const MAX_CONTROLLERS: usize = 8;
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const SLOT_RESERVATION_TTL: Duration = Duration::from_secs(300);

struct SlotAllocator {
    used: [bool; MAX_CONTROLLERS],
    reservations: HashMap<String, (u8, Instant)>,
}

impl SlotAllocator {
    fn new() -> Self {
        Self {
            used: [false; MAX_CONTROLLERS],
            reservations: HashMap::new(),
        }
    }

    fn alloc(&mut self, key: &str) -> Option<u8> {
        self.expire();
        if let Some((slot, _)) = self.reservations.get(key).copied() {
            if !self.used[slot as usize] {
                self.used[slot as usize] = true;
                self.reservations
                    .insert(key.to_string(), (slot, Instant::now()));
                return Some(slot);
            }
        }

        // Prefer a free slot that no key has reserved, so a sleeping controller
        // keeps its player slot.
        let free = (0..MAX_CONTROLLERS).find(|&slot| {
            !self.used[slot]
                && !self
                    .reservations
                    .values()
                    .any(|(reserved, _)| *reserved as usize == slot)
        });
        let slot = free.or_else(|| self.used.iter().position(|used| !used))?;
        self.used[slot] = true;
        self.reservations
            .retain(|_, (reserved, _)| *reserved != slot as u8);
        self.reservations
            .insert(key.to_string(), (slot as u8, Instant::now()));
        Some(slot as u8)
    }

    fn free(&mut self, slot: u8) {
        if let Some(used) = self.used.get_mut(slot as usize) {
            *used = false;
        }
        // The reservation survives a disconnect. expire() drops it after the
        // reservation timeout.
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let used = self.used;
        self.reservations.retain(|_, (slot, last)| {
            used[*slot as usize] || now.duration_since(*last) < SLOT_RESERVATION_TTL
        });
    }
}

struct Device {
    index: u32,
    fd: RawFd,
    slot: u8,
    key: String,
}

struct Session {
    client: RawFd,
    devices: Vec<Device>,
    slots: SlotAllocator,
    effects: HashMap<(u8, u16), u16>,
}

impl Session {
    fn new(client: RawFd) -> Self {
        Self {
            client,
            devices: Vec::new(),
            slots: SlotAllocator::new(),
            effects: HashMap::new(),
        }
    }

    fn send(&self, message: &Message) -> io::Result<()> {
        write_frame(self.client, message)
    }

    fn device(&self, slot: u8) -> Option<&Device> {
        self.devices.iter().find(|device| device.slot == slot)
    }

    fn scan(&mut self, filter: &str) -> io::Result<()> {
        for index in 0..=255u32 {
            if self.devices.iter().any(|device| device.index == index) {
                continue;
            }
            // Read the name from sysfs. An FF upload can hold the evdev
            // mutex. Opening every evdev node then wedges discovery.
            let Some(name) = sysfs_device_name(index) else {
                continue;
            };
            if name != filter {
                continue;
            }
            let Some((fd, writable)) = open_event_rw(index) else {
                continue;
            };
            let info = match read_device_info(fd, name.clone(), writable) {
                Ok(info) => info,
                Err(error) => {
                    eprintln!("xone-relay-sender: input{index}: {error}");
                    close_fd(fd);
                    continue;
                }
            };
            // Deduplicate only on a real serial. Several controllers behind one
            // adapter often share phys, so a phys key hides all but the first.
            // The event node is the identity when uniq is empty.
            let key = if info.uniq.is_empty() {
                format!("event:{index}")
            } else {
                format!("uniq:{}", info.uniq)
            };
            if self.devices.iter().any(|device| device.key == key) {
                close_fd(fd);
                continue;
            }
            let Some(slot) = self.slots.alloc(&key) else {
                close_fd(fd);
                eprintln!("xone-relay-sender: no free controller slot");
                continue;
            };
            if let Err(error) = self.send(&Message::DeviceAdd {
                slot,
                info: info.clone(),
            }) {
                close_fd(fd);
                self.slots.free(slot);
                return Err(error);
            }
            eprintln!(
                "xone-relay-sender: controller {slot} {name} ({key}){}{}",
                if writable {
                    ""
                } else {
                    " (read-only: rumble disabled)"
                },
                if info.ff.is_empty() { " (no FF)" } else { "" }
            );
            self.devices.push(Device {
                index,
                fd,
                slot,
                key,
            });
        }
        Ok(())
    }

    fn remove_device(&mut self, position: usize) {
        let device = self.devices.remove(position);
        self.effects.retain(|(slot, _), _| *slot != device.slot);
        self.slots.free(device.slot);
        let _ = self.send(&Message::DeviceRemove { slot: device.slot });
        close_fd(device.fd);
        eprintln!("xone-relay-sender: controller {} removed", device.slot);
    }

    fn handle_ff_upload(
        &mut self,
        slot: u8,
        local_id: u16,
        effect: &crate::protocol::FfUploadData,
    ) -> io::Result<()> {
        let Some(position) = self.devices.iter().position(|device| device.slot == slot) else {
            return self.send(&Message::FfUploadResp {
                slot,
                local_id,
                result: -libc::ENODEV,
                host_id: 0,
            });
        };
        if effect.type_ != FF_RUMBLE {
            return self.send(&Message::FfUploadResp {
                slot,
                local_id,
                result: -libc::EINVAL,
                host_id: 0,
            });
        }
        let mut ff: FfEffect = effect.to_effect();
        let result = upload_ff_effect(self.devices[position].fd, &mut ff);
        match result {
            Ok(()) => {
                let host_id = ff.id as u16;
                self.effects.insert((slot, local_id), host_id);
                self.send(&Message::FfUploadResp {
                    slot,
                    local_id,
                    result: 0,
                    host_id,
                })
            }
            Err(error) => self.send(&Message::FfUploadResp {
                slot,
                local_id,
                result: -error.raw_os_error().unwrap_or(libc::EIO),
                host_id: 0,
            }),
        }
    }

    fn handle_ff_erase(&mut self, slot: u8, local_id: u16) -> io::Result<()> {
        let Some(position) = self.devices.iter().position(|device| device.slot == slot) else {
            return self.send(&Message::FfEraseResp {
                slot,
                local_id,
                result: -libc::ENODEV,
            });
        };
        let Some(host_id) = self.effects.remove(&(slot, local_id)) else {
            return self.send(&Message::FfEraseResp {
                slot,
                local_id,
                result: -libc::ENOENT,
            });
        };
        let result = erase_ff_effect(self.devices[position].fd, host_id);
        self.send(&Message::FfEraseResp {
            slot,
            local_id,
            result: match result {
                Ok(()) => 0,
                Err(error) => -error.raw_os_error().unwrap_or(libc::EIO),
            },
        })
    }

    fn handle_ff_play(&mut self, slot: u8, code: u16, value: i32) -> io::Result<()> {
        let Some(device) = self.device(slot) else {
            return Ok(());
        };
        let host_code = if code == FF_GAIN || code == FF_AUTOCENTER {
            code
        } else {
            let Some(host_id) = self.effects.get(&(slot, code)) else {
                return Ok(());
            };
            *host_id
        };
        if let Err(error) = write_ff_event(device.fd, host_code, value) {
            eprintln!("xone-relay-sender: FF play on {slot}: {error}");
        }
        Ok(())
    }

    fn handle_client_message(&mut self, message: Message) -> io::Result<()> {
        match message {
            Message::FfUpload {
                slot,
                local_id,
                effect,
            } => self.handle_ff_upload(slot, local_id, &effect),
            Message::FfErase { slot, local_id } => self.handle_ff_erase(slot, local_id),
            Message::FfPlay { slot, code, value } => self.handle_ff_play(slot, code, value),
            Message::Hello { .. } => Ok(()),
            _ => Ok(()),
        }
    }

    fn poll_once(&mut self) -> io::Result<bool> {
        let mut pollfds = Vec::with_capacity(1 + self.devices.len());
        pollfds.push(libc::pollfd {
            fd: self.client,
            events: libc::POLLIN,
            revents: 0,
        });
        for device in &self.devices {
            pollfds.push(libc::pollfd {
                fd: device.fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // SAFETY: pollfds is a live slice and the timeout is a valid integer.
        let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 1000) };
        if ret < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(true);
            }
            return Err(error);
        }
        if ret == 0 {
            return Ok(true);
        }

        if pollfds[0].revents != 0 {
            match read_frame(self.client) {
                Ok(message) => self.handle_client_message(message)?,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            }
        }

        let mut remove = Vec::new();
        let mut buffer = [0u8; 65536];
        for position in 0..self.devices.len() {
            if pollfds[position + 1].revents == 0 {
                continue;
            }
            let fd = self.devices[position].fd;
            // SAFETY: buffer is writable for its length and `fd` is an open
            // evdev descriptor.
            let ret =
                unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
            if ret < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::ENODEV) => {
                        remove.push(position);
                        continue;
                    }
                    _ => return Err(error),
                }
            }
            if ret == 0 {
                remove.push(position);
                continue;
            }
            let len = ret as usize;
            let whole = len - len % EVENT_SIZE;
            if whole == 0 {
                continue;
            }
            debug_assert!(events_are_aligned(&buffer[..whole]));
            self.send(&Message::Events {
                slot: self.devices[position].slot,
                data: buffer[..whole].to_vec(),
            })?;
        }

        for position in remove.into_iter().rev() {
            self.remove_device(position);
        }
        Ok(true)
    }

    fn run(&mut self, filter: &str) -> io::Result<()> {
        self.send(&Message::Hello {
            version: PROTOCOL_VERSION,
        })?;
        self.scan(filter)?;
        let mut last_scan = Instant::now();
        loop {
            if !self.poll_once()? {
                return Ok(());
            }
            if last_scan.elapsed() >= SCAN_INTERVAL {
                self.scan(filter)?;
                last_scan = Instant::now();
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for device in &self.devices {
            close_fd(device.fd);
        }
    }
}

pub fn run_sender(
    receiver_address: Option<String>,
    port_override: Option<u16>,
    device_name: Option<String>,
) {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("xone-relay-sender: {error}");
            std::process::exit(1);
        }
    };
    let receiver = match receiver_address {
        Some(address) => address,
        None => match config.require_receiver_address() {
            Ok(address) => address.to_string(),
            Err(error) => {
                eprintln!("xone-relay-sender: {error}");
                std::process::exit(1);
            }
        },
    };
    let port = port_override.unwrap_or(config.port);
    let filter = device_name.unwrap_or_else(|| DEFAULT_DEVICE_NAME.to_string());

    loop {
        let client = match connect_tcp(&receiver, port) {
            Ok(fd) => fd,
            Err(error) => {
                eprintln!("xone-relay-sender: connect {receiver}:{port}: {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        tcp_tune(client);
        eprintln!("xone-relay-sender: connected to {receiver}:{port}");
        let mut session = Session::new(client);
        if let Err(error) = session.run(&filter) {
            eprintln!("xone-relay-sender: {error}");
        }
        drop(session);
        close_fd(client);
        eprintln!("xone-relay-sender: disconnected; reconnecting");
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_reused_by_key() {
        let mut slots = SlotAllocator::new();
        assert_eq!(slots.alloc("pad-a"), Some(0));
        assert_eq!(slots.alloc("pad-b"), Some(1));
        slots.free(0);
        assert_eq!(slots.alloc("pad-a"), Some(0));
        assert_eq!(slots.alloc("pad-c"), Some(2));
        for n in 3..MAX_CONTROLLERS {
            assert_eq!(slots.alloc(&format!("pad-{n}")), Some(n as u8));
        }
        assert_eq!(slots.alloc("pad-overflow"), None);
    }

    #[test]
    fn a_disconnected_key_keeps_its_slot() {
        let mut slots = SlotAllocator::new();
        assert_eq!(slots.alloc("left"), Some(0));
        slots.free(0);
        assert_eq!(slots.alloc("right"), Some(1));
        slots.free(1);
        assert_eq!(slots.alloc("left"), Some(0));
        assert_eq!(slots.alloc("right"), Some(1));
    }
}
