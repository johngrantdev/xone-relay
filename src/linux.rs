use std::ffi::CString;
use std::io;
use std::mem;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{IntoRawFd, RawFd};
use std::ptr;
use std::time::Duration;

pub const EVENT_SIZE: usize = 24;

pub const KEY_BITMAP_LEN: usize = 96;
pub const ABS_BITMAP_LEN: usize = 8;
pub const FF_BITMAP_LEN: usize = 16;
pub const MSC_BITMAP_LEN: usize = 1;

pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_FF: u16 = 0x15;
pub const EV_UINPUT: u16 = 0x0101;

pub const UI_FF_UPLOAD: u16 = 1;
pub const UI_FF_ERASE: u16 = 2;

pub const FF_RUMBLE: u16 = 0x50;
pub const FF_GAIN: u16 = 0x60;
pub const FF_AUTOCENTER: u16 = 0x61;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRSHIFT: u32 = 30;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::Ioctl {
    ((dir << IOC_DIRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)
        | ((ty as u32) << IOC_TYPESHIFT)
        | ((nr as u32) << IOC_NRSHIFT)) as libc::Ioctl
}

const fn ior(ty: u8, nr: u8, size: usize) -> libc::Ioctl {
    ioc(IOC_READ, ty, nr, size)
}

const fn iow(ty: u8, nr: u8, size: usize) -> libc::Ioctl {
    ioc(IOC_WRITE, ty, nr, size)
}

const fn io(ty: u8, nr: u8) -> libc::Ioctl {
    ioc(IOC_NONE, ty, nr, 0)
}

pub const fn eviocgid() -> libc::Ioctl {
    ior(b'E', 0x02, mem::size_of::<InputId>())
}

pub const fn eviocgbit(event_type: u16, len: usize) -> libc::Ioctl {
    ioc(IOC_READ, b'E', 0x20 + event_type as u8, len)
}

const fn eviocgabs(abs: u16) -> libc::Ioctl {
    ior(b'E', 0x40 + abs as u8, mem::size_of::<InputAbsInfo>())
}

pub const fn eviocsff() -> libc::Ioctl {
    iow(b'E', 0x80, mem::size_of::<FfEffect>())
}

pub const fn eviocrmff() -> libc::Ioctl {
    iow(b'E', 0x81, mem::size_of::<libc::c_int>())
}

pub const UI_DEV_CREATE: libc::Ioctl = io(b'U', 1);
pub const UI_DEV_DESTROY: libc::Ioctl = io(b'U', 2);
pub const UI_DEV_SETUP: libc::Ioctl = iow(b'U', 3, mem::size_of::<UinputSetup>());
pub const UI_ABS_SETUP: libc::Ioctl = iow(b'U', 4, mem::size_of::<UinputAbsSetup>());
pub const UI_SET_EVBIT: libc::Ioctl = iow(b'U', 100, mem::size_of::<libc::c_int>());
pub const UI_SET_KEYBIT: libc::Ioctl = iow(b'U', 101, mem::size_of::<libc::c_int>());
pub const UI_SET_ABSBIT: libc::Ioctl = iow(b'U', 103, mem::size_of::<libc::c_int>());
pub const UI_SET_MSCBIT: libc::Ioctl = iow(b'U', 104, mem::size_of::<libc::c_int>());
pub const UI_SET_FFBIT: libc::Ioctl = iow(b'U', 107, mem::size_of::<libc::c_int>());
pub const UI_SET_PHYS: libc::Ioctl = iow(b'U', 108, mem::size_of::<*const libc::c_char>());
pub const UI_BEGIN_FF_UPLOAD: libc::Ioctl = ioc(
    IOC_READ | IOC_WRITE,
    b'U',
    200,
    mem::size_of::<UinputFfUpload>(),
);
pub const UI_END_FF_UPLOAD: libc::Ioctl = iow(b'U', 201, mem::size_of::<UinputFfUpload>());
pub const UI_BEGIN_FF_ERASE: libc::Ioctl = ioc(
    IOC_READ | IOC_WRITE,
    b'U',
    202,
    mem::size_of::<UinputFfErase>(),
);
pub const UI_END_FF_ERASE: libc::Ioctl = iow(b'U', 203, mem::size_of::<UinputFfErase>());

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub time: libc::timeval,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub fn new(type_: u16, code: u16, value: i32) -> Self {
        Self {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfTrigger {
    pub button: u16,
    pub interval: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfReplay {
    pub length: u16,
    pub delay: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfRumbleEffect {
    pub strong_magnitude: u16,
    pub weak_magnitude: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FfEffectU {
    pub rumble: FfRumbleEffect,
    pub raw: [u8; 32],
    _align: [u64; 4],
}

impl Default for FfEffectU {
    fn default() -> Self {
        Self { raw: [0; 32] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FfEffect {
    pub type_: u16,
    pub id: i16,
    pub direction: u16,
    pub trigger: FfTrigger,
    pub replay: FfReplay,
    pub u: FfEffectU,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UinputFfUpload {
    pub request_id: u32,
    pub retval: i32,
    pub effect: FfEffect,
    pub old: FfEffect,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UinputFfErase {
    pub request_id: u32,
    pub retval: i32,
    pub effect_id: u32,
}

pub const UINPUT_MAX_NAME_SIZE: usize = 80;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UinputSetup {
    pub id: InputId,
    pub name: [u8; UINPUT_MAX_NAME_SIZE],
    pub ff_effects_max: u32,
}

impl Default for UinputSetup {
    fn default() -> Self {
        Self {
            id: InputId::default(),
            name: [0; UINPUT_MAX_NAME_SIZE],
            ff_effects_max: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UinputAbsSetup {
    pub code: u16,
    pub absinfo: InputAbsInfo,
}

const _: () = assert!(mem::size_of::<InputEvent>() == EVENT_SIZE);
const _: () = assert!(mem::size_of::<InputAbsInfo>() == 24);
const _: () = assert!(mem::size_of::<UinputAbsSetup>() == 28);
const _: () = assert!(mem::size_of::<UinputSetup>() == 92);
const _: () = assert!(mem::size_of::<FfEffect>() == 48);
const _: () = assert!(mem::align_of::<FfEffect>() == 8);
const _: () = assert!(mem::size_of::<FfRumbleEffect>() == 4);
const _: () = assert!(mem::size_of::<UinputFfUpload>() == 104);
const _: () = assert!(mem::size_of::<UinputFfErase>() == 12);

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub name: String,
    pub id: InputId,
    pub key: Vec<u8>,
    pub abs: Vec<(u16, InputAbsInfo)>,
    pub ff: Vec<u8>,
    pub msc: Vec<u8>,
    pub uniq: String,
    pub phys: String,
}

pub fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub fn ioctl_mut<T>(fd: RawFd, request: libc::Ioctl, value: &mut T) -> io::Result<()> {
    // SAFETY: the caller passes a request for `fd` and a live `T` of the
    // size that request expects.
    cvt(unsafe { libc::ioctl(fd, request, value as *mut T) }).map(|_| ())
}

pub fn ioctl_int(fd: RawFd, request: libc::Ioctl, value: libc::c_int) -> io::Result<()> {
    // SAFETY: the caller passes an integer request for `fd`.
    cvt(unsafe { libc::ioctl(fd, request, value) }).map(|_| ())
}

pub fn ioctl_none(fd: RawFd, request: libc::Ioctl) -> io::Result<()> {
    // SAFETY: the caller passes a no-argument request for `fd`.
    cvt(unsafe { libc::ioctl(fd, request, 0) }).map(|_| ())
}

pub fn set_sockopt(
    fd: RawFd,
    level: libc::c_int,
    option: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    // SAFETY: `value` is a live integer and the length matches the option.
    cvt(unsafe {
        libc::setsockopt(
            fd,
            level,
            option,
            &value as *const libc::c_int as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    })
    .map(|_| ())
}

pub fn tcp_tune(fd: RawFd) {
    // libc does not export every Linux socket option number below.
    const TCP_QUICKACK: libc::c_int = 12;
    const TCP_KEEPIDLE: libc::c_int = 4;
    const TCP_KEEPINTVL: libc::c_int = 5;
    const TCP_KEEPCNT: libc::c_int = 6;
    const TCP_USER_TIMEOUT: libc::c_int = 18;

    let _ = set_sockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    let _ = set_sockopt(fd, libc::IPPROTO_TCP, TCP_QUICKACK, 1);
    let _ = set_sockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    let _ = set_sockopt(fd, libc::IPPROTO_TCP, TCP_KEEPIDLE, 30);
    let _ = set_sockopt(fd, libc::IPPROTO_TCP, TCP_KEEPINTVL, 10);
    let _ = set_sockopt(fd, libc::IPPROTO_TCP, TCP_KEEPCNT, 3);
    // A peer that stops acknowledging ends the session, so a write cannot
    // block forever.
    let _ = set_sockopt(fd, libc::IPPROTO_TCP, TCP_USER_TIMEOUT, 10_000);
    let _ = set_timeout(fd, libc::SO_SNDTIMEO, 5);
    let _ = set_timeout(fd, libc::SO_RCVTIMEO, 5);
}

fn set_timeout(fd: RawFd, option: libc::c_int, seconds: i64) -> io::Result<()> {
    let timeout = libc::timeval {
        tv_sec: seconds,
        tv_usec: 0,
    };
    // SAFETY: `timeout` is a live timeval and the length matches the option.
    cvt(unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            &timeout as *const libc::timeval as *const libc::c_void,
            mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    })
    .map(|_| ())
}

pub fn close_fd(fd: RawFd) {
    // SAFETY: the caller owns `fd` and does not use it after this call.
    unsafe { libc::close(fd) };
}

pub fn ignore_sigpipe() {
    // SAFETY: the signal handler is a constant and needs no pointer argument.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn send_all(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` covers `len` readable bytes and `fd` is an open socket.
        let ret = unsafe {
            libc::send(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if ret > 0 {
            data = &data[ret as usize..];
            continue;
        }
        if ret == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "send returned 0"));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
    Ok(())
}

pub fn write_all(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` covers `len` readable bytes and `fd` is open for writing.
        let ret = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if ret > 0 {
            data = &data[ret as usize..];
            continue;
        }
        if ret == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
    Ok(())
}

pub fn read_exact_fd(fd: RawFd, mut buffer: &mut [u8]) -> io::Result<()> {
    while !buffer.is_empty() {
        // SAFETY: `buffer` covers `len` writable bytes and `fd` is an open socket.
        let ret = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                0,
            )
        };
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed the connection",
            ));
        }
        if ret < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        buffer = &mut buffer[ret as usize..];
    }
    Ok(())
}

pub fn open_event_rw(index: u32) -> Option<(RawFd, bool)> {
    let path = CString::new(format!("/dev/input/event{index}")).ok()?;
    // SAFETY: `path` is a NUL-terminated C string.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd >= 0 {
        return Some((fd, true));
    }
    // SAFETY: `path` is a NUL-terminated C string.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    (fd >= 0).then_some((fd, false))
}

fn device_string(fd: RawFd, nr: u8) -> String {
    let mut buffer = [0u8; 256];
    let request = ioc(IOC_READ, b'E', nr, buffer.len());
    // SAFETY: `buffer` is writable for the request size.
    let ret = unsafe { libc::ioctl(fd, request, buffer.as_mut_ptr()) };
    if ret < 0 {
        return String::new();
    }
    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..len]).into_owned()
}

pub fn sysfs_device_name(index: u32) -> Option<String> {
    let path = format!("/sys/class/input/event{index}/device/name");
    std::fs::read_to_string(path)
        .ok()
        .map(|name| name.trim().to_string())
}

pub fn device_phys(fd: RawFd) -> String {
    device_string(fd, 0x07)
}

pub fn device_uniq(fd: RawFd) -> String {
    device_string(fd, 0x08)
}

pub fn read_bitmap(fd: RawFd, event_type: u16, len: usize) -> Vec<u8> {
    let mut bitmap = vec![0u8; len];
    // SAFETY: `bitmap` is writable for `len` bytes and the request matches it.
    let ret = unsafe { libc::ioctl(fd, eviocgbit(event_type, len), bitmap.as_mut_ptr()) };
    if ret < 0 {
        bitmap.clear();
    }
    bitmap
}

pub fn bit_is_set(bitmap: &[u8], bit: usize) -> bool {
    bitmap
        .get(bit / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

pub fn read_device_info(fd: RawFd, name: String, writable: bool) -> io::Result<DeviceInfo> {
    let mut id = InputId::default();
    ioctl_mut(fd, eviocgid(), &mut id)?;

    let key = read_bitmap(fd, EV_KEY, KEY_BITMAP_LEN);
    let abs_bits = read_bitmap(fd, EV_ABS, ABS_BITMAP_LEN);
    let mut ff = read_bitmap(fd, EV_FF, FF_BITMAP_LEN);
    let msc = read_bitmap(fd, EV_MSC, MSC_BITMAP_LEN);
    if !writable {
        // A read-only descriptor cannot play rumble, so clear the FF bitmap.
        ff.clear();
    }

    let mut abs = Vec::new();
    for code in 0..(ABS_BITMAP_LEN * 8) {
        if !bit_is_set(&abs_bits, code) {
            continue;
        }
        let mut info = InputAbsInfo::default();
        if ioctl_mut(fd, eviocgabs(code as u16), &mut info).is_ok() {
            abs.push((code as u16, info));
        }
    }

    Ok(DeviceInfo {
        name,
        id,
        key,
        abs,
        ff,
        msc,
        uniq: device_uniq(fd),
        phys: device_phys(fd),
    })
}

pub fn enable_bits(fd: RawFd, request: libc::Ioctl, bitmap: &[u8]) -> io::Result<()> {
    for code in 0..bitmap.len() * 8 {
        if bit_is_set(bitmap, code) {
            ioctl_int(fd, request, code as libc::c_int)?;
        }
    }
    Ok(())
}

pub fn write_ff_event(fd: RawFd, code: u16, value: i32) -> io::Result<()> {
    let event = InputEvent::new(EV_FF, code, value);
    // SAFETY: `event` is a live InputEvent and the slice covers its bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &event as *const InputEvent as *const u8,
            mem::size_of::<InputEvent>(),
        )
    };
    write_all(fd, bytes)
}

pub fn upload_ff_effect(fd: RawFd, effect: &mut FfEffect) -> io::Result<()> {
    ioctl_mut(fd, eviocsff(), effect)
}

pub fn erase_ff_effect(fd: RawFd, effect_id: u16) -> io::Result<()> {
    ioctl_int(fd, eviocrmff(), effect_id as libc::c_int)
}

pub fn open_uinput() -> io::Result<RawFd> {
    let path = CString::new("/dev/uinput").expect("static path");
    // SAFETY: `path` is a static NUL-terminated C string.
    cvt(unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) })
}

pub fn connect_tcp(host: &str, port: u16) -> io::Result<RawFd> {
    let addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let mut last_error = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                let fd = stream.into_raw_fd();
                tcp_tune(fd);
                return Ok(fd);
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

pub fn create_server(port: u16) -> io::Result<RawFd> {
    // SAFETY: socket has no pointer arguments.
    let fd =
        cvt(unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })?;
    let _ = set_sockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1);

    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    // SAFETY: `address` is a live sockaddr_in of the length passed.
    let ret = unsafe {
        libc::bind(
            fd,
            &address as *const libc::sockaddr_in as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if let Err(error) = cvt(ret) {
        close_fd(fd);
        return Err(error);
    }
    // SAFETY: listen takes only the socket and backlog.
    cvt(unsafe { libc::listen(fd, 4) })?;
    Ok(fd)
}

pub fn accept_client(server: RawFd) -> io::Result<RawFd> {
    loop {
        // SAFETY: accept4 permits null address pointers and `server` is listening.
        let fd =
            unsafe { libc::accept4(server, ptr::null_mut(), ptr::null_mut(), libc::SOCK_CLOEXEC) };
        match cvt(fd) {
            Ok(fd) => return Ok(fd),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ff_ioctl_directions_match_kernel_abi() {
        assert_eq!(
            UI_BEGIN_FF_UPLOAD,
            ioc(IOC_READ | IOC_WRITE, b'U', 200, 104)
        );
        // UI_END_FF_UPLOAD uses _IOW. An _IOWR request returns EINVAL after the
        // effect was uploaded.
        assert_eq!(UI_END_FF_UPLOAD, iow(b'U', 201, 104));
        assert_eq!(UI_BEGIN_FF_ERASE, ioc(IOC_READ | IOC_WRITE, b'U', 202, 12));
        assert_eq!(UI_END_FF_ERASE, iow(b'U', 203, 12));
    }

    #[test]
    fn ff_layout_matches_the_kernel() {
        assert_eq!(mem::size_of::<FfEffect>(), 48);
        assert_eq!(mem::align_of::<FfEffect>(), 8);
        assert_eq!(mem::size_of::<UinputFfUpload>(), 104);
        assert_eq!(mem::size_of::<UinputFfErase>(), 12);
    }
}
