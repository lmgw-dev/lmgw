//! [`super::adapters`]: the display class's driver keys, read with the
//! registry API, for the devices SetupAPI lists as present.

use std::collections::HashSet;
use std::io;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceRegistryPropertyW, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    SPDRP_DRIVER, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_NO_MORE_ITEMS,
    ERROR_NO_SUCH_DEVINST,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_BINARY, REG_DWORD, REG_QWORD, REG_SZ, RRF_RT_ANY,
};

use super::{pci_ids, Adapter, CLASS_KEY};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: opened by RegOpenKeyExW, closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn open(parent: HKEY, path: &str) -> io::Result<Key> {
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated; the key is closed by the Key.
    let rc = unsafe { RegOpenKeyExW(parent, wide(path).as_ptr(), 0, KEY_READ, &mut key) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(Key(key))
}

/// A value as Windows stores it.
enum Value {
    Text(String),
    Number(u64),
    Bytes(Vec<u8>),
}

fn value(key: &Key, name: &str) -> Option<Value> {
    let name = wide(name);
    let (mut kind, mut len) = (0u32, 0u32);
    // SAFETY: a size query; no buffer.
    let rc = unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_ANY,
            &mut kind,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize + 2];
    len = buf.len() as u32;
    // SAFETY: `buf` holds `len` bytes.
    let rc = unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_ANY,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(match kind {
        REG_SZ => {
            let units: Vec<u16> = buf
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|&u| u != 0)
                .collect();
            Value::Text(String::from_utf16_lossy(&units))
        }
        REG_DWORD if buf.len() >= 4 => Value::Number(u64::from(u32::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3],
        ]))),
        REG_QWORD if buf.len() >= 8 => {
            Value::Number(u64::from_le_bytes(buf[..8].try_into().unwrap_or([0; 8])))
        }
        REG_BINARY => Value::Bytes(buf),
        _ => Value::Bytes(buf),
    })
}

fn text(key: &Key, name: &str) -> Option<String> {
    match value(key, name)? {
        Value::Text(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => None,
    }
}

/// A memory size as drivers write it: a QWORD, a DWORD, or 4 or 8 bytes.
fn size(key: &Key, name: &str) -> Option<u64> {
    match value(key, name)? {
        Value::Number(n) => Some(n),
        Value::Bytes(b) if b.len() >= 8 => b[..8].try_into().ok().map(u64::from_le_bytes),
        Value::Bytes(b) if b.len() >= 4 => {
            Some(u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        }
        _ => None,
    }
}

/// A device information set, destroyed on drop.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        // SAFETY: made by SetupDiGetClassDevsW, destroyed once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// The driver keys (`0001`) of the display devices that are present now:
/// each device's `SPDRP_DRIVER` is `{class guid}\NNNN`, its key under the
/// class. A device that was once installed and is gone (a card taken out,
/// a VM's earlier display) keeps its key but is not in this set.
fn present_keys() -> io::Result<HashSet<String>> {
    // SAFETY: the GUID is a constant; no enumerator, no window.
    let set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVCLASS_DISPLAY,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT,
        )
    };
    if set == -1 {
        return Err(io::Error::last_os_error());
    }
    let set = DevInfo(set);
    let mut keys = HashSet::new();
    for index in 0.. {
        let mut dev = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `dev` is the struct the call names, its size set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut dev) } == 0 {
            // SAFETY: read right after the failed call, on its thread.
            match unsafe { GetLastError() } {
                ERROR_NO_MORE_ITEMS => break,
                code => {
                    return Err(io::Error::other(format!(
                        "listing the present display devices stopped at device {index}: {}",
                        io::Error::from_raw_os_error(code as i32)
                    )))
                }
            }
        }
        let Some(driver) = driver_key(&set, &dev).map_err(|e| {
            io::Error::other(format!(
                "the driver key of present display device {index} cannot be read: {e}"
            ))
        })?
        else {
            // A present device with no driver key ([`no_driver_key`]: the
            // property is not set, or the device went since the listing)
            // has no adapter key under the display class to stand for it,
            // so no adapter is left out here.
            continue;
        };
        if let Some((_, key)) = driver.rsplit_once('\\') {
            keys.insert(key.to_string());
        }
    }
    Ok(keys)
}

/// Whether a failed `SPDRP_DRIVER` read says the device has no driver key:
/// the property is not set (`ERROR_INVALID_DATA`), or the device is gone
/// between the listing and the read (`ERROR_NO_SUCH_DEVINST`, unplugged or
/// removed), so it stands for no adapter either.
fn no_driver_key(code: u32) -> bool {
    matches!(code, ERROR_INVALID_DATA | ERROR_NO_SUCH_DEVINST)
}

/// The `SPDRP_DRIVER` property of `dev` (`{class GUID}\<key>`), or `None`
/// when the device has none ([`no_driver_key`]); any other failure is the
/// error. The buffer grows to the size the call asks for.
fn driver_key(set: &DevInfo, dev: &SP_DEVINFO_DATA) -> io::Result<Option<String>> {
    let mut buf = vec![0u16; 256];
    loop {
        let mut kind = 0u32;
        let mut needed = 0u32;
        // SAFETY: `buf` holds the byte size passed.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                dev,
                SPDRP_DRIVER,
                &mut kind,
                buf.as_mut_ptr().cast(),
                (buf.len() * 2) as u32,
                &mut needed,
            )
        };
        if ok != 0 {
            let units: Vec<u16> = buf.iter().copied().take_while(|&u| u != 0).collect();
            return Ok(Some(String::from_utf16_lossy(&units)));
        }
        // SAFETY: read right after the failed call, on its thread.
        match unsafe { GetLastError() } {
            code if no_driver_key(code) => return Ok(None),
            ERROR_INSUFFICIENT_BUFFER if (needed as usize).div_ceil(2) > buf.len() => {
                buf.resize((needed as usize).div_ceil(2), 0);
            }
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
}

/// Every adapter key under the display class that names an adapter whose
/// device is present ([`present_keys`]); keys that cannot be read
/// (`Properties`, which only the system may) are left out. An error when the
/// class key itself cannot be opened, or the present devices not listed.
pub fn adapters() -> io::Result<Vec<Adapter>> {
    let present = present_keys()?;
    let class = open(HKEY_LOCAL_MACHINE, CLASS_KEY)?;
    let mut out = Vec::new();
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` UTF-16 units; the other outputs are
        // not asked for.
        let rc = unsafe {
            RegEnumKeyExW(
                class.0,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            // ERROR_NO_MORE_ITEMS, or a key that went in between: done.
            break;
        }
        let sub = String::from_utf16_lossy(&name[..len as usize]);
        if !sub.chars().all(|c| c.is_ascii_digit()) || !present.contains(&sub) {
            continue;
        }
        let Ok(key) = open(class.0, &sub) else {
            continue;
        };
        let Some(desc) = text(&key, "DriverDesc") else {
            continue;
        };
        let (vendor, device) = text(&key, "MatchingDeviceId")
            .map(|m| pci_ids(&m))
            .unwrap_or((None, None));
        out.push(Adapter {
            key: sub,
            name: desc,
            provider: text(&key, "ProviderName"),
            driver_version: text(&key, "DriverVersion"),
            vendor,
            device,
            memory_bytes: size(&key, "HardwareInformation.qwMemorySize")
                .or_else(|| size(&key, "HardwareInformation.MemorySize"))
                .filter(|&b| b > 0),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_gone_since_the_listing_has_no_driver_key_like_one_without() {
        assert!(no_driver_key(ERROR_INVALID_DATA));
        assert!(no_driver_key(ERROR_NO_SUCH_DEVINST));
        assert!(!no_driver_key(ERROR_INSUFFICIENT_BUFFER));
        assert!(!no_driver_key(
            windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED
        ));
    }
}
