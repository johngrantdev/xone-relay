use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

use crate::linux::{
    close_fd, enable_bits, ioctl_int, ioctl_mut, ioctl_none, open_uinput, write_all, DeviceInfo,
    UinputAbsSetup, UinputSetup, EV_ABS, EV_FF, EV_KEY, EV_MSC, UINPUT_MAX_NAME_SIZE, UI_ABS_SETUP,
    UI_DEV_CREATE, UI_DEV_DESTROY, UI_DEV_SETUP, UI_SET_ABSBIT, UI_SET_EVBIT, UI_SET_FFBIT,
    UI_SET_KEYBIT, UI_SET_MSCBIT, UI_SET_PHYS,
};

pub fn create_uinput(
    info: &DeviceInfo,
    name_override: Option<&str>,
    phys_override: Option<&str>,
) -> io::Result<RawFd> {
    let fd = open_uinput()?;

    let result = (|| -> io::Result<()> {
        if !info.key.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
        }
        if !info.abs.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_ABS as libc::c_int)?;
        }
        if !info.ff.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_FF as libc::c_int)?;
        }
        if !info.msc.is_empty() {
            ioctl_int(fd, UI_SET_EVBIT, EV_MSC as libc::c_int)?;
        }

        enable_bits(fd, UI_SET_KEYBIT, &info.key)?;
        enable_bits(fd, UI_SET_MSCBIT, &info.msc)?;
        enable_bits(fd, UI_SET_FFBIT, &info.ff)?;

        for (code, absinfo) in &info.abs {
            ioctl_int(fd, UI_SET_ABSBIT, *code as libc::c_int)?;
            let mut setup = UinputAbsSetup {
                code: *code,
                absinfo: *absinfo,
            };
            ioctl_mut(fd, UI_ABS_SETUP, &mut setup)?;
        }

        let mut setup = UinputSetup {
            id: info.id,
            name: [0; UINPUT_MAX_NAME_SIZE],
            ff_effects_max: if info.ff.is_empty() { 0 } else { 96 },
        };
        let name = name_override.unwrap_or(&info.name).as_bytes();
        let name_len = name.len().min(UINPUT_MAX_NAME_SIZE - 1);
        setup.name[..name_len].copy_from_slice(&name[..name_len]);
        ioctl_mut(fd, UI_DEV_SETUP, &mut setup)?;

        let phys = phys_override
            .filter(|value| !value.is_empty())
            .unwrap_or(&info.phys);
        if let Ok(phys) = CString::new(phys) {
            // SAFETY: `phys` is a live NUL-terminated C string.
            let _ = crate::linux::cvt(unsafe {
                libc::ioctl(fd, UI_SET_PHYS, phys.as_ptr() as *const libc::c_char)
            });
        }

        ioctl_none(fd, UI_DEV_CREATE)?;
        Ok(())
    })();

    if let Err(error) = result {
        close_fd(fd);
        return Err(error);
    }
    Ok(fd)
}

pub fn destroy_uinput(fd: RawFd) {
    let _ = ioctl_none(fd, UI_DEV_DESTROY);
    close_fd(fd);
}

pub fn write_uinput(fd: RawFd, data: &[u8]) -> io::Result<()> {
    write_all(fd, data)
}

pub fn rename_for_slot(slot: u8, info: &DeviceInfo) -> String {
    if info.phys.is_empty() {
        format!("xone-relay/{slot}")
    } else {
        format!("{}/relay-{slot}", info.phys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phys_falls_back_to_slot() {
        let mut info = DeviceInfo::default();
        info.phys.clear();
        assert_eq!(rename_for_slot(3, &info), "xone-relay/3");
        info.phys = "usb-1/input0".into();
        assert_eq!(rename_for_slot(3, &info), "usb-1/input0/relay-3");
    }
}
