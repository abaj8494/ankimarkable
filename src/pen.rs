//! Raw-evdev reader for the reMarkable Paper Pro pen ("Elan marker input").
//!
//! The QTFB socket only forwards the *touchscreen* (finger) as `INPUT_TOUCH_*`,
//! which the UI uses for buttons. The Wacom-style marker is a separate evdev
//! device, so the whiteboard reads it directly here — the same approach
//! `fastterm/src/input.rs` uses for the keyd virtual keyboard.
//!
//! We resolve the device by `EVIOCGNAME` (the event index is NOT stable across
//! OTA/reboot — see memory `keyd event-index NOT stable`), open it `O_NONBLOCK`
//! so it can be `poll()`-multiplexed with the QTFB socket, read the ABS ranges at
//! runtime via `EVIOCGABS`, and map digitizer coords → the 1620×2160 portrait
//! panel. Pressure drives stroke width; we ink only while the pen is in contact.

use std::io;
use std::os::unix::io::RawFd;

// evdev event/axis codes.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const BTN_TOUCH: u16 = 0x14a; // 330 — pen in contact with the surface
const BTN_TOOL_PEN: u16 = 0x140; // 320 — pen tip in proximity (hover)
const BTN_TOOL_RUBBER: u16 = 0x141; // 321 — pen flipped: eraser end in proximity

// EVIOCGRAB — take the marker exclusively so backgrounded xochitl doesn't also
// process it (mirrors fastScratch). `_IOW('E', 0x90, int)`.
const EVIOCGRAB: libc::Ioctl = 0x4004_4590u64 as libc::Ioctl;

// Panel geometry (native portrait). Matches ui::WIDTH / ui::HEIGHT.
const PANEL_W: i32 = 1620;
const PANEL_H: i32 = 2160;

// Measured fallbacks (EVIOCGABS on this panel's marker); overridden at runtime.
const FALLBACK_XMAX: i32 = 11180;
const FALLBACK_YMAX: i32 = 15340;
const PRESSURE_MAX: i32 = 4096;

// Axis orientation. The marker origin matches the panel's on the rMPP; flip
// constants are here so a live test can correct an inverted axis with one edit.
const FLIP_X: bool = false;
const FLIP_Y: bool = false;

fn eviocgname(len: usize) -> libc::Ioctl {
    (0x8000_0000u64 | ((len as u64) << 16) | (0x45 << 8) | 0x06) as libc::Ioctl
}
fn eviocgabs(axis: u16) -> libc::Ioctl {
    // _IOR('E', 0x40+axis, struct input_absinfo) — 6× i32 = 24 bytes.
    ((2u64 << 30) | (24 << 16) | (0x45 << 8) | (0x40 + axis as u64)) as libc::Ioctl
}

#[repr(C)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

/// One inking sample in panel coordinates, emitted on each `SYN_REPORT` while the
/// pen is (or just became / just stopped being) in contact.
#[derive(Clone, Copy, Debug)]
pub struct PenSample {
    pub x: i32,
    pub y: i32,
    pub w: u8,
    pub down: bool,  // first contact sample of a stroke
    pub up: bool,    // pen lifted (x/y are the last known point)
    pub erase: bool, // the eraser end (flipped pen) is in use
}

pub struct Pen {
    fd: RawFd,
    grabbed: bool,
    xmax: i32,
    ymax: i32,
    // accumulated axis state within the current SYN frame
    x: i32,
    y: i32,
    pressure: i32,
    contact: bool,     // currently touching
    was_contact: bool, // contact at last emitted sample
    rubber: bool,      // eraser end (BTN_TOOL_RUBBER) in proximity
    prox: bool,        // either end reported in proximity (hovering)
    activity: bool,    // any marker traffic since the last take_activity()
    pub dev_path: String,
}

impl Pen {
    /// Resolve + open the marker device (non-blocking). `Err` if not found — the
    /// caller degrades to a touch-only UI.
    pub fn open() -> io::Result<Self> {
        let (fd, dev_path) = resolve_marker()?;
        // Non-blocking so drain() never stalls the poll loop.
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let xmax = abs_max(fd, ABS_X).unwrap_or(FALLBACK_XMAX).max(1);
        let ymax = abs_max(fd, ABS_Y).unwrap_or(FALLBACK_YMAX).max(1);
        // Grab exclusively (best-effort). Released on close / process exit.
        let grabbed = unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) } == 0;
        Ok(Self {
            fd,
            grabbed,
            xmax,
            ymax,
            x: 0,
            y: 0,
            pressure: 0,
            contact: false,
            was_contact: false,
            rubber: false,
            prox: false,
            activity: false,
            dev_path,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// True while either pen end is in proximity of the glass (hovering), i.e.
    /// `BTN_TOOL_PEN`/`BTN_TOOL_RUBBER` are asserted but the tip may not be down
    /// yet. This is the signal that makes palm rejection work *before* the first
    /// ink sample: the hand lands on the panel while the pen is still approaching,
    /// so a hover means any concurrent finger contact is a palm.
    ///
    /// Callers must pair this with a recency check on `take_activity()` — if the
    /// pen leaves without a closing `BTN_TOOL_*` report, proximity would otherwise
    /// stay latched and lock out every finger touch.
    pub fn in_proximity(&self) -> bool {
        self.prox
    }

    /// Whether any marker event at all (including a bare hover move, which emits no
    /// `PenSample`) arrived since the last call. Clears the flag.
    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }

    /// Toggle the exclusive grab: `true` re-takes EVIOCGRAB, `false` releases it
    /// (so another process — the scratchpad overlay — can grab the marker). No-op
    /// when already in the wanted state. Returns the resulting grab state (`false`
    /// if taking the grab failed, e.g. someone else currently holds it).
    pub fn set_grab(&mut self, want: bool) -> bool {
        if want != self.grabbed {
            let arg: libc::c_int = if want { 1 } else { 0 };
            if unsafe { libc::ioctl(self.fd, EVIOCGRAB, arg) } == 0 {
                self.grabbed = want;
            }
        }
        self.grabbed
    }

    fn map_x(&self, raw: i32) -> i32 {
        let v = (raw.clamp(0, self.xmax) as i64 * PANEL_W as i64 / self.xmax as i64) as i32;
        if FLIP_X {
            PANEL_W - 1 - v
        } else {
            v
        }
    }
    fn map_y(&self, raw: i32) -> i32 {
        let v = (raw.clamp(0, self.ymax) as i64 * PANEL_H as i64 / self.ymax as i64) as i32;
        if FLIP_Y {
            PANEL_H - 1 - v
        } else {
            v
        }
    }
    fn width(&self) -> u8 {
        // 2..=6 px from pressure.
        (2 + (self.pressure.clamp(0, PRESSURE_MAX) * 4 / PRESSURE_MAX)) as u8
    }

    /// Read all pending events (non-blocking) and invoke `f` once per completed
    /// `SYN_REPORT` that involves pen contact (a down/move/up sample). Returns
    /// `Ok(())` on EAGAIN (nothing more to read); `Err` on a real read error/EOF.
    pub fn drain<F: FnMut(PenSample)>(&mut self, mut f: F) -> io::Result<()> {
        let sz = std::mem::size_of::<InputEvent>();
        let mut buf = [0u8; std::mem::size_of::<InputEvent>() * 64];
        loop {
            let n = unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(c) if c == libc::EAGAIN || c == libc::EWOULDBLOCK => Ok(()),
                    _ => Err(e),
                };
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "marker closed"));
            }
            let count = (n as usize) / sz;
            for i in 0..count {
                let ev: &InputEvent =
                    unsafe { &*(buf.as_ptr().add(i * sz) as *const InputEvent) };
                self.activity = true;
                match ev.type_ {
                    EV_ABS => match ev.code {
                        ABS_X => self.x = ev.value,
                        ABS_Y => self.y = ev.value,
                        ABS_PRESSURE => self.pressure = ev.value,
                        _ => {}
                    },
                    EV_KEY => match ev.code {
                        BTN_TOUCH => self.contact = ev.value != 0,
                        // Flipped pen: the eraser end. Track which end is in proximity.
                        BTN_TOOL_RUBBER => {
                            self.rubber = ev.value != 0;
                            self.prox = ev.value != 0;
                        }
                        BTN_TOOL_PEN => {
                            self.prox = ev.value != 0;
                            if ev.value != 0 {
                                self.rubber = false;
                            } else {
                                // Some firmwares report contact only via pressure.
                                self.contact = false;
                            }
                        }
                        _ => {}
                    },
                    EV_SYN if ev.code == SYN_REPORT => {
                        let contact = self.contact || self.pressure > 0;
                        let px = self.map_x(self.x);
                        let py = self.map_y(self.y);
                        if contact {
                            f(PenSample {
                                x: px,
                                y: py,
                                w: self.width(),
                                down: !self.was_contact,
                                up: false,
                                erase: self.rubber,
                            });
                            self.was_contact = true;
                        } else if self.was_contact {
                            f(PenSample {
                                x: px,
                                y: py,
                                w: self.width(),
                                down: false,
                                up: true,
                                erase: self.rubber,
                            });
                            self.was_contact = false;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe {
            if self.grabbed {
                libc::ioctl(self.fd, EVIOCGRAB, 0 as libc::c_int);
            }
            libc::close(self.fd);
        }
    }
}

fn abs_max(fd: RawFd, axis: u16) -> Option<i32> {
    let mut info = [0i32; 6]; // value,min,max,fuzz,flat,resolution
    let rc = unsafe {
        libc::ioctl(fd, eviocgabs(axis), info.as_mut_ptr() as *mut libc::c_void)
    };
    if rc >= 0 && info[2] > info[1] {
        Some(info[2])
    } else {
        None
    }
}

fn resolve_marker() -> io::Result<(RawFd, String)> {
    for i in 0..32 {
        let path = format!("/dev/input/event{i}");
        let c = std::ffi::CString::new(path.clone()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            continue;
        }
        let mut name = [0u8; 256];
        let rc = unsafe {
            libc::ioctl(
                fd,
                eviocgname(name.len()),
                name.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if rc >= 0 {
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            let n = String::from_utf8_lossy(&name[..end]).to_lowercase();
            // "Elan marker input" — exclude the "Elan touch input" digitizer.
            if n.contains("marker") || (n.contains("elan") && n.contains("pen")) {
                return Ok((fd, path));
            }
        }
        unsafe { libc::close(fd) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no Elan marker input device found",
    ))
}
