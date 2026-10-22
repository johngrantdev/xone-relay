use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::config::Config;
use crate::linux::{
    accept_client, close_fd, create_server, ioctl_mut, tcp_tune, DeviceInfo, InputEvent,
    UinputFfErase, UinputFfUpload, EVENT_SIZE, EV_FF, EV_UINPUT, FF_AUTOCENTER, FF_GAIN,
    UI_BEGIN_FF_ERASE, UI_BEGIN_FF_UPLOAD, UI_END_FF_ERASE, UI_END_FF_UPLOAD, UI_FF_ERASE,
    UI_FF_UPLOAD,
};
use crate::protocol::{
    events_are_aligned, read_frame, write_frame, FfUploadData, Message, PROTOCOL_VERSION,
};
use crate::uinput::{create_uinput, destroy_uinput, rename_for_slot, write_uinput};

struct UinputDevice {
    fd: RawFd,
    name: String,
}

struct PendingUpload {
    request: UinputFfUpload,
}

struct PendingErase {
    request: UinputFfErase,
}

struct Session {
    client: RawFd,
    devices: HashMap<u8, UinputDevice>,
    uploads: HashMap<(u8, u16), PendingUpload>,
    erases: HashMap<(u8, u16), PendingErase>,
    effects: HashMap<(u8, u16), u16>,
    deferred: HashMap<u8, Vec<u8>>,
    uinput_name: Option<String>,
}

impl Session {
    fn new(client: RawFd, uinput_name: Option<String>) -> Self {
        Self {
            client,
            devices: HashMap::new(),
            uploads: HashMap::new(),
            erases: HashMap::new(),
            effects: HashMap::new(),
            deferred: HashMap::new(),
            uinput_name,
        }
    }

    fn send(&self, message: &Message) -> io::Result<()> {
        write_frame(self.client, message)
    }

    fn remove_device(&mut self, slot: u8) {
        if let Some(device) = self.devices.remove(&slot) {
            destroy_uinput(device.fd);
            eprintln!(
                "xone-relay-receiver: removed controller {slot} ({})",
                device.name
            );
        }
        self.uploads.retain(|(device, _), _| *device != slot);
        self.erases.retain(|(device, _), _| *device != slot);
        self.effects.retain(|(device, _), _| *device != slot);
        self.deferred.remove(&slot);
    }

    fn add_device(&mut self, slot: u8, info: &DeviceInfo) -> io::Result<()> {
        self.remove_device(slot);
        let phys = rename_for_slot(slot, info);
        let fd = create_uinput(info, self.uinput_name.as_deref(), Some(&phys))?;
        let name = self
            .uinput_name
            .clone()
            .unwrap_or_else(|| info.name.clone());
        eprintln!("xone-relay-receiver: controller {slot} {name}");
        self.devices.insert(slot, UinputDevice { fd, name });
        Ok(())
    }

    fn handle_message(&mut self, message: Message) -> io::Result<()> {
        match message {
            Message::DeviceAdd { slot, info } => {
                if let Err(error) = self.add_device(slot, &info) {
                    eprintln!("xone-relay-receiver: uinput for {slot}: {error}");
                }
                Ok(())
            }
            Message::DeviceRemove { slot } => {
                self.remove_device(slot);
                Ok(())
            }
            Message::Events { slot, data } => {
                if !events_are_aligned(&data) {
                    return Ok(());
                }
                if self.slot_busy(slot) {
                    // A uinput write can block while the kernel waits for an
                    // FF upload. Queue input until the host answers, or the
                    // FF response behind it cannot be read.
                    let deferred = self.deferred.entry(slot).or_default();
                    if deferred.len() < 1 << 20 {
                        deferred.extend_from_slice(&data);
                    }
                    return Ok(());
                }
                let Some(fd) = self.devices.get(&slot).map(|device| device.fd) else {
                    return Ok(());
                };
                if let Err(error) = write_uinput(fd, &data) {
                    eprintln!("xone-relay-receiver: uinput write for {slot}: {error}");
                    self.remove_device(slot);
                }
                Ok(())
            }
            Message::FfUploadResp {
                slot,
                local_id,
                result,
                host_id,
            } => {
                self.finish_upload(slot, local_id, result, host_id);
                Ok(())
            }
            Message::FfEraseResp {
                slot,
                local_id,
                result,
            } => {
                self.finish_erase(slot, local_id, result);
                Ok(())
            }
            Message::Hello { .. }
            | Message::FfUpload { .. }
            | Message::FfErase { .. }
            | Message::FfPlay { .. } => Ok(()),
        }
    }

    fn slot_busy(&self, slot: u8) -> bool {
        self.uploads.keys().any(|(device, _)| *device == slot)
            || self.erases.keys().any(|(device, _)| *device == slot)
    }

    fn flush_deferred(&mut self, slot: u8) {
        let Some(data) = self.deferred.remove(&slot) else {
            return;
        };
        let Some(fd) = self.devices.get(&slot).map(|device| device.fd) else {
            return;
        };
        if let Err(error) = write_uinput(fd, &data) {
            eprintln!("xone-relay-receiver: uinput write for {slot}: {error}");
            self.remove_device(slot);
        }
    }

    fn finish_upload(&mut self, slot: u8, local_id: u16, result: i32, host_id: u16) {
        let Some(pending) = self.uploads.remove(&(slot, local_id)) else {
            return;
        };
        let Some(fd) = self.devices.get(&slot).map(|device| device.fd) else {
            return;
        };
        let mut request = pending.request;
        request.retval = result;
        if result == 0 {
            self.effects.insert((slot, local_id), host_id);
        } else {
            self.effects.remove(&(slot, local_id));
        }
        if let Err(error) = ioctl_mut(fd, UI_END_FF_UPLOAD, &mut request) {
            eprintln!("xone-relay-receiver: UI_END_FF_UPLOAD {slot}/{local_id}: {error}");
        }
        self.flush_deferred(slot);
    }

    fn finish_erase(&mut self, slot: u8, local_id: u16, result: i32) {
        let Some(pending) = self.erases.remove(&(slot, local_id)) else {
            return;
        };
        let Some(fd) = self.devices.get(&slot).map(|device| device.fd) else {
            return;
        };
        self.effects.remove(&(slot, local_id));
        let mut request = pending.request;
        request.retval = result;
        if let Err(error) = ioctl_mut(fd, UI_END_FF_ERASE, &mut request) {
            eprintln!("xone-relay-receiver: UI_END_FF_ERASE {slot}/{local_id}: {error}");
        }
        self.flush_deferred(slot);
    }

    fn handle_uinput_events(&mut self, slot: u8, data: &[u8]) -> io::Result<()> {
        let Some(fd) = self.devices.get(&slot).map(|device| device.fd) else {
            return Ok(());
        };
        for chunk in data.chunks(EVENT_SIZE) {
            if chunk.len() != EVENT_SIZE {
                continue;
            }
            // SAFETY: chunk has EVENT_SIZE bytes and read_unaligned does not
            // require alignment.
            let event: InputEvent =
                unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
            match event.type_ {
                EV_UINPUT if event.code == UI_FF_UPLOAD => {
                    let mut request = UinputFfUpload {
                        request_id: event.value as u32,
                        ..Default::default()
                    };
                    if let Err(error) = ioctl_mut(fd, UI_BEGIN_FF_UPLOAD, &mut request) {
                        eprintln!("xone-relay-receiver: UI_BEGIN_FF_UPLOAD: {error}");
                        continue;
                    }
                    let local_id = request.effect.id as u16;
                    if let Some(old) = self.uploads.remove(&(slot, local_id)) {
                        let mut old = old.request;
                        old.retval = -libc::EBUSY;
                        let _ = ioctl_mut(fd, UI_END_FF_UPLOAD, &mut old);
                    }
                    let Some(effect) = FfUploadData::from_effect(&request.effect) else {
                        request.retval = -libc::EINVAL;
                        let _ = ioctl_mut(fd, UI_END_FF_UPLOAD, &mut request);
                        continue;
                    };
                    self.send(&Message::FfUpload {
                        slot,
                        local_id,
                        effect,
                    })?;
                    self.uploads
                        .insert((slot, local_id), PendingUpload { request });
                }
                EV_UINPUT if event.code == UI_FF_ERASE => {
                    let mut request = UinputFfErase {
                        request_id: event.value as u32,
                        ..Default::default()
                    };
                    if let Err(error) = ioctl_mut(fd, UI_BEGIN_FF_ERASE, &mut request) {
                        eprintln!("xone-relay-receiver: UI_BEGIN_FF_ERASE: {error}");
                        continue;
                    }
                    let local_id = request.effect_id as u16;
                    self.send(&Message::FfErase { slot, local_id })?;
                    self.erases
                        .insert((slot, local_id), PendingErase { request });
                }
                EV_FF => {
                    let code = event.code;
                    if code == FF_GAIN
                        || code == FF_AUTOCENTER
                        || self.effects.contains_key(&(slot, code))
                    {
                        self.send(&Message::FfPlay {
                            slot,
                            code,
                            value: event.value,
                        })?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn poll_once(&mut self) -> io::Result<bool> {
        let mut pollfds = Vec::with_capacity(1 + self.devices.len());
        let mut slots = Vec::with_capacity(self.devices.len());
        pollfds.push(libc::pollfd {
            fd: self.client,
            events: libc::POLLIN,
            revents: 0,
        });
        for (slot, device) in &self.devices {
            pollfds.push(libc::pollfd {
                fd: device.fd,
                events: libc::POLLIN,
                revents: 0,
            });
            slots.push(*slot);
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
                Ok(message) => self.handle_message(message)?,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            }
        }

        let mut remove = Vec::new();
        let mut buffer = [0u8; 8192];
        for (position, slot) in slots.iter().enumerate() {
            if pollfds[position + 1].revents == 0 {
                continue;
            }
            let Some(fd) = self.devices.get(slot).map(|device| device.fd) else {
                continue;
            };
            // SAFETY: buffer is writable for its length and `fd` is an open
            // uinput descriptor.
            let ret =
                unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
            if ret < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::ENODEV) => {
                        remove.push(*slot);
                        continue;
                    }
                    _ => return Err(error),
                }
            }
            if ret == 0 {
                remove.push(*slot);
                continue;
            }
            let len = ret as usize;
            let whole = len - len % EVENT_SIZE;
            if whole > 0 {
                self.handle_uinput_events(*slot, &buffer[..whole])?;
            }
        }
        for slot in remove {
            self.remove_device(slot);
        }
        Ok(true)
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            if !self.poll_once()? {
                return Ok(());
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for (_, device) in self.devices.drain() {
            destroy_uinput(device.fd);
        }
    }
}

fn serve_client(client: RawFd, uinput_name: Option<&str>) -> io::Result<()> {
    let first = read_frame(client)?;
    match first {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        Message::Hello { version } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("protocol version {version}, expected {PROTOCOL_VERSION}"),
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sender did not start with hello",
            ));
        }
    }

    let mut session = Session::new(client, uinput_name.map(str::to_string));
    session.run()
}

pub fn run_receiver(port_override: Option<u16>, uinput_name: Option<String>) {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("xone-relay-receiver: {error}");
            std::process::exit(1);
        }
    };
    let port = port_override.unwrap_or(config.port);
    let server = match create_server(port) {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("xone-relay-receiver: listen on 0.0.0.0:{port}: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("xone-relay-receiver: listening on 0.0.0.0:{port}");

    loop {
        let client = match accept_client(server) {
            Ok(fd) => fd,
            Err(error) => {
                eprintln!("xone-relay-receiver: accept: {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        tcp_tune(client);
        eprintln!("xone-relay-receiver: sender connected");
        if let Err(error) = serve_client(client, uinput_name.as_deref()) {
            eprintln!("xone-relay-receiver: {error}");
        }
        close_fd(client);
        eprintln!("xone-relay-receiver: sender disconnected; waiting for the next one");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn event_parse_size_is_stable() {
        assert_eq!(mem::size_of::<InputEvent>(), EVENT_SIZE);
        assert_eq!(mem::size_of::<InputEvent>() % EVENT_SIZE, 0);
    }
}
