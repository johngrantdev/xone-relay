use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::config::{Config, DEFAULT_DEVICE_NAME};
use crate::linux::{
    append_event, bit_is_set, close_fd, connect_tcp, erase_ff_effect, eviocgabs,
    input_event_indices, ioctl_mut, open_event_rw, read_device_info, read_key_state,
    sysfs_device_name, tcp_tune, upload_ff_effect, write_ff_event, DeviceInfo, FfEffect,
    InputAbsInfo, EVENT_SIZE, EV_ABS, EV_KEY, EV_SYN, FF_AUTOCENTER, FF_GAIN, FF_RUMBLE,
    SYN_DROPPED, SYN_REPORT,
};
use crate::protocol::{events_are_aligned, FrameParser, FrameWriter, Message, PROTOCOL_VERSION};

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
    info: DeviceInfo,
}

struct Session {
    client: RawFd,
    devices: Vec<Device>,
    slots: SlotAllocator,
    effects: HashMap<(u8, u16), u16>,
    reader: FrameParser,
    writer: FrameWriter,
}

impl Session {
    fn new(client: RawFd) -> Self {
        Self {
            client,
            devices: Vec::new(),
            slots: SlotAllocator::new(),
            effects: HashMap::new(),
            reader: FrameParser::new(),
            writer: FrameWriter::new(),
        }
    }

    fn send(&mut self, message: &Message) -> io::Result<()> {
        self.writer.queue(message)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush(self.client)
    }

    fn device(&self, slot: u8) -> Option<&Device> {
        self.devices.iter().find(|device| device.slot == slot)
    }

    fn scan(&mut self, filter: &str) -> io::Result<()> {
        for index in input_event_indices() {
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
            let device = Device {
                index,
                fd,
                slot,
                key,
                info,
            };
            if let Err(error) = self.send(&Message::DeviceAdd {
                slot,
                info: device.info.clone(),
            }) {
                close_fd(fd);
                self.slots.free(slot);
                return Err(error);
            }
            // A button may already be held when the receiver creates the
            // device; start from the controller's current state instead of
            // waiting for the next change.
            match state_snapshot(&device) {
                Ok(data) => {
                    if let Err(error) = self.send(&Message::Events { slot, data }) {
                        close_fd(device.fd);
                        self.slots.free(slot);
                        return Err(error);
                    }
                }
                Err(error) => {
                    eprintln!("xone-relay-sender: input{index}: read state: {error}");
                }
            }
            eprintln!(
                "xone-relay-sender: controller {slot} {name} ({}){}{}",
                device.key,
                if writable {
                    ""
                } else {
                    " (read-only: rumble disabled)"
                },
                if device.info.ff.is_empty() {
                    " (no FF)"
                } else {
                    ""
                }
            );
            self.devices.push(device);
        }
        Ok(())
    }

    // An effect the guest has gone away from is a rumble that never stops:
    // erasing it is the only thing that ends it on the controller.
    fn stop_effects(&mut self, slot: u8, fd: RawFd) {
        let host_ids: Vec<u16> = self
            .effects
            .iter()
            .filter(|((effect_slot, _), _)| *effect_slot == slot)
            .map(|(_, host_id)| *host_id)
            .collect();
        for host_id in &host_ids {
            let _ = erase_ff_effect(fd, *host_id);
        }
        self.effects
            .retain(|(effect_slot, _), _| *effect_slot != slot);
    }

    fn remove_device(&mut self, position: usize) {
        let device = self.devices.remove(position);
        self.stop_effects(device.slot, device.fd);
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
        // The guest replaces an effect by re-uploading it under the same id.
        // The host gets a new id for every upload, so the old effect would
        // stay in the controller's table, and an old infinite effect keeps
        // rumbling with nothing left to stop it. Drop it either way.
        let previous = self.effects.remove(&(slot, local_id));
        let mut ff: FfEffect = effect.to_effect();
        let result = upload_ff_effect(self.devices[position].fd, &mut ff);
        match result {
            Ok(()) => {
                let host_id = ff.id as u16;
                if let Some(previous) = previous.filter(|previous| *previous != host_id) {
                    let _ = erase_ff_effect(self.devices[position].fd, previous);
                }
                self.effects.insert((slot, local_id), host_id);
                self.send(&Message::FfUploadResp {
                    slot,
                    local_id,
                    result: 0,
                    host_id,
                })
            }
            Err(error) => {
                if let Some(previous) = previous {
                    let _ = erase_ff_effect(self.devices[position].fd, previous);
                }
                self.send(&Message::FfUploadResp {
                    slot,
                    local_id,
                    result: -error.raw_os_error().unwrap_or(libc::EIO),
                    host_id: 0,
                })
            }
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
        let mut client_events = libc::POLLIN;
        if !self.writer.is_empty() {
            client_events |= libc::POLLOUT;
        }
        pollfds.push(libc::pollfd {
            fd: self.client,
            events: client_events,
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
                self.flush()?;
                return Ok(true);
            }
            return Err(error);
        }
        if ret == 0 {
            self.flush()?;
            return Ok(true);
        }

        let client = pollfds[0].revents;
        if client & (libc::POLLIN | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            let open = self.reader.read_from(self.client)?;
            while let Some(message) = self.reader.next()? {
                self.handle_client_message(message)?;
            }
            if !open {
                let _ = self.flush();
                return Ok(false);
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
            // A full evdev queue drops events and leaves SYN_DROPPED. The
            // client would otherwise keep the last state it saw, which can be
            // a held button whose release was dropped. Replace the batch with
            // a snapshot of the controller's real state.
            if has_syn_dropped(&buffer[..whole]) {
                match state_snapshot(&self.devices[position]) {
                    Ok(data) => self.send(&Message::Events {
                        slot: self.devices[position].slot,
                        data,
                    })?,
                    Err(error) => {
                        eprintln!(
                            "xone-relay-sender: input{}: resync: {error}",
                            self.devices[position].index
                        );
                        self.send(&Message::Events {
                            slot: self.devices[position].slot,
                            data: buffer[..whole].to_vec(),
                        })?;
                    }
                }
                continue;
            }
            self.send(&Message::Events {
                slot: self.devices[position].slot,
                data: buffer[..whole].to_vec(),
            })?;
        }

        for position in remove.into_iter().rev() {
            self.remove_device(position);
        }
        self.flush()?;
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
        let devices = std::mem::take(&mut self.devices);
        for device in devices {
            self.stop_effects(device.slot, device.fd);
            close_fd(device.fd);
        }
    }
}

/// True when an evdev read lost events and the kernel left a marker.
fn has_syn_dropped(data: &[u8]) -> bool {
    data.as_chunks::<EVENT_SIZE>().0.iter().any(|chunk| {
        // SAFETY: chunk is exactly EVENT_SIZE bytes and read_unaligned does
        // not require alignment.
        let event: crate::linux::InputEvent =
            unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const _) };
        event.type_ == EV_SYN && event.code == SYN_DROPPED
    })
}

/// Build a complete EV_KEY/EV_ABS snapshot for a device.
///
/// Every supported key is reported, so a release lost by evdev is corrected
/// instead of leaving a button held. The trailing SYN_REPORT lets the receiver
/// apply the snapshot as one atomic update.
fn state_snapshot(device: &Device) -> io::Result<Vec<u8>> {
    let keys = read_key_state(device.fd, device.info.key.len())?;
    let mut data = Vec::with_capacity((device.info.key.len() + device.info.abs.len()) * 8 + 24);
    for code in 0..device.info.key.len() * 8 {
        if !bit_is_set(&device.info.key, code) {
            continue;
        }
        append_event(
            &mut data,
            EV_KEY,
            code as u16,
            i32::from(bit_is_set(&keys, code)),
        );
    }
    for (code, _) in &device.info.abs {
        let mut abs = InputAbsInfo::default();
        ioctl_mut(device.fd, eviocgabs(*code), &mut abs)?;
        append_event(&mut data, EV_ABS, *code, abs.value);
    }
    append_event(&mut data, EV_SYN, SYN_REPORT, 0);
    Ok(data)
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

    #[test]
    fn a_device_batch_is_forwarded_as_one_message() {
        let mut socket = [0; 2];
        // SAFETY: socket is writable for two descriptors.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, socket.as_mut_ptr()) },
            0
        );
        let mut pipe = [0; 2];
        // SAFETY: pipe is writable for two descriptors.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);

        let mut session = Session::new(socket[0]);
        session.devices.push(Device {
            index: 0,
            fd: pipe[0],
            slot: 0,
            key: "pad".into(),
            info: DeviceInfo::default(),
        });
        // Two EV_SYN/SYN_REPORT records are enough to prove the bytes are
        // forwarded unchanged.
        let events = [0u8; EVENT_SIZE * 2];
        // SAFETY: pipe[1] is open for writing and events is readable.
        assert_eq!(
            unsafe {
                libc::write(
                    pipe[1],
                    events.as_ptr() as *const libc::c_void,
                    events.len(),
                )
            },
            events.len() as isize
        );

        assert!(session.poll_once().expect("poll"));

        let mut parser = FrameParser::new();
        assert!(parser.read_from(socket[1]).expect("read"));
        match parser.next().expect("parse") {
            Some(Message::Events {
                slot,
                data: decoded,
            }) => {
                assert_eq!(slot, 0);
                assert_eq!(decoded, events);
            }
            other => panic!("unexpected {other:?}"),
        }

        drop(session);
        // SAFETY: all remaining descriptors are open and owned by this test.
        unsafe {
            libc::close(socket[1]);
            libc::close(pipe[1]);
        }
    }

    #[test]
    fn syn_dropped_marks_a_lost_batch() {
        let mut data = Vec::new();
        append_event(&mut data, EV_SYN, SYN_REPORT, 0);
        assert!(!has_syn_dropped(&data));
        append_event(&mut data, EV_SYN, SYN_DROPPED, 0);
        assert!(has_syn_dropped(&data));
        assert!(!has_syn_dropped(&data[..EVENT_SIZE]));
    }

    #[test]
    fn stopping_a_device_clears_only_its_effects() {
        let mut session = Session::new(-1);
        session.effects.insert((0, 1), 7);
        session.effects.insert((1, 1), 8);
        // The ioctls fail on a closed descriptor, which is fine: this checks
        // the bookkeeping that decides what would be stopped.
        session.stop_effects(0, -1);
        assert!(!session.effects.contains_key(&(0, 1)));
        assert_eq!(session.effects.get(&(1, 1)), Some(&8));
    }

    #[test]
    fn a_reupload_drops_the_previous_host_effect() {
        let mut session = Session::new(-1);
        session.devices.push(Device {
            index: 0,
            fd: -1,
            slot: 0,
            key: "pad".into(),
            info: DeviceInfo::default(),
        });
        session.effects.insert((0, 1), 7);
        session.effects.insert((1, 1), 8);
        let effect = crate::protocol::FfUploadData {
            type_: FF_RUMBLE,
            direction: 0,
            trigger: Default::default(),
            replay: Default::default(),
            strong: 0,
            weak: 0,
        };
        let _ = session.handle_ff_upload(0, 1, &effect);
        assert!(!session.effects.contains_key(&(0, 1)));
        assert_eq!(session.effects.get(&(1, 1)), Some(&8));
    }
}
