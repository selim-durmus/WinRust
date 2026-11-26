//! Bluetooth and airplane mode through Windows.Devices.Radios / Devices.Enumeration (no admin, no Settings app): the radio switch,
//! paired devices with their connection state, pairing a nearby device and removing a paired one. The WinRT calls wait, so they
//! run in a thread and leave their result in `STATE`, which the window reads on its next refresh.
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering::Relaxed},
};
use windows::{
    Devices::{
        Bluetooth::BluetoothDevice,
        Enumeration::{DeviceInformation, DevicePairingKinds, DevicePairingResultStatus, DeviceUnpairingResultStatus},
        Radios::{Radio, RadioKind, RadioState},
    },
    Foundation::TypedEventHandler,
    core::HSTRING,
};

#[derive(Clone, Debug, PartialEq)]
pub struct Dev {
    pub id: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Default, Clone)]
pub struct State {
    pub bt: Option<bool>,        // a Bluetooth radio: on or off; None without one
    pub air: Option<bool>,       // every radio is off
    pub paired: Vec<Dev>,
    pub nearby: Vec<Dev>,
    pub scanning: bool,
    pub note: String,            // the outcome of the last pairing / removal
}

static STATE: Mutex<State> = Mutex::new(State { bt: None, air: None, paired: vec![], nearby: vec![], scanning: false, note: String::new() });
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn state() -> State {
    STATE.lock().map(|s| s.clone()).unwrap_or_default()
}

fn with<T>(f: impl FnOnce(&mut State) -> T) -> Option<T> {
    STATE.lock().ok().map(|mut s| f(&mut s))
}

fn radios() -> Vec<Radio> {
    let _ = Radio::RequestAccessAsync().and_then(|a| a.join());
    Radio::GetRadiosAsync().and_then(|a| a.join()).map(|v| v.into_iter().collect()).unwrap_or_default()
}

/// Radio and device lists, read again in a thread (one at a time); `done` runs afterwards (to repaint).
pub fn refresh(done: impl FnOnce() + Send + 'static) {
    if BUSY.swap(true, Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        let rs = radios();
        let bt: Vec<_> = rs.iter().filter(|r| r.Kind().is_ok_and(|k| k == RadioKind::Bluetooth)).collect();
        let bt_on = (!bt.is_empty()).then(|| bt.iter().any(|r| r.State().is_ok_and(|s| s == RadioState::On)));
        let air = (!rs.is_empty()).then(|| rs.iter().all(|r| r.State().is_ok_and(|s| s != RadioState::On)));
        let paired = if bt_on == Some(true) { list(true) } else { vec![] };
        with(|s| (s.bt, s.air, s.paired) = (bt_on, air, paired));
        BUSY.store(false, Relaxed);
        done();
    });
}

fn list(paired: bool) -> Vec<Dev> {
    let Ok(sel) = BluetoothDevice::GetDeviceSelectorFromPairingState(paired) else { return vec![] };
    let Ok(all) = DeviceInformation::FindAllAsyncAqsFilter(&sel).and_then(|a| a.join()) else { return vec![] };
    let mut out: Vec<Dev> = all
        .into_iter()
        .filter_map(|d| {
            let (id, name) = (d.Id().ok()?.to_string(), d.Name().ok()?.to_string());
            let connected = paired && BluetoothDevice::FromIdAsync(&HSTRING::from(&id)).and_then(|a| a.join()).and_then(|b| b.ConnectionStatus()).is_ok_and(|c| c.0 == 1);
            (!name.trim().is_empty()).then_some(Dev { id, name, connected })
        })
        .collect();
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

/// Switches every radio of that kind (`None`: all radios, which is airplane mode) and re-reads the state.
pub fn set(kind: Option<RadioKind>, on: bool, done: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        for r in radios().into_iter().filter(|r| kind.is_none_or(|k| r.Kind().is_ok_and(|rk| rk == k))) {
            let _ = r.SetStateAsync(if on { RadioState::On } else { RadioState::Off }).and_then(|a| a.join());
        }
        BUSY.store(false, Relaxed);
        refresh(done);
    });
}

pub fn set_bluetooth(on: bool, done: impl FnOnce() + Send + 'static) {
    set(Some(RadioKind::Bluetooth), on, done);
}

pub fn set_airplane(on: bool, done: impl FnOnce() + Send + 'static) {
    // airplane mode: everything off; back from it: Wi-Fi and Bluetooth on (the radios that have a switch of their own)
    std::thread::spawn(move || {
        for r in radios() {
            let kind = r.Kind().ok();
            if on || matches!(kind, Some(RadioKind::WiFi | RadioKind::Bluetooth)) {
                let _ = r.SetStateAsync(if on { RadioState::Off } else { RadioState::On }).and_then(|a| a.join());
            }
        }
        BUSY.store(false, Relaxed);
        refresh(done);
    });
}

/// Looks for devices in range (it takes about ten seconds).
pub fn scan(done: impl FnOnce() + Send + 'static) {
    if with(|s| std::mem::replace(&mut s.scanning, true)) != Some(false) {
        return;
    }
    std::thread::spawn(move || {
        let found = list(false);
        with(|s| (s.nearby, s.scanning) = (found, false));
        done();
    });
}

fn pairing_text(code: i32) -> String {
    match DevicePairingResultStatus(code) {
        DevicePairingResultStatus::Paired => "Device paired".into(),
        DevicePairingResultStatus::AlreadyPaired => "Already paired".into(),
        DevicePairingResultStatus::AuthenticationFailure | DevicePairingResultStatus::AuthenticationNotAllowed => "The device refused to pair".into(),
        DevicePairingResultStatus::RequiredHandlerNotRegistered | DevicePairingResultStatus::RejectedByHandler => "This device needs a PIN: pair it in Windows' own settings".into(),
        _ => format!("Could not pair (code {})", code),
    }
}

/// Pairs a device from `nearby`: confirmation and a displayed PIN are accepted here (the PIN is shown in the note so it can be
/// compared with the device); a device that wants a PIN typed in is reported.
pub fn pair(id: String, done: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let msg = (|| -> windows::core::Result<String> {
            let info = DeviceInformation::CreateFromIdAsync(&HSTRING::from(&id))?.join()?;
            let custom = info.Pairing()?.Custom()?;
            custom.PairingRequested(&TypedEventHandler::new(|_, args: windows::core::Ref<windows::Devices::Enumeration::DevicePairingRequestedEventArgs>| {
                if let Some(a) = args.as_ref() {
                    if a.PairingKind().is_ok_and(|k| k == DevicePairingKinds::ConfirmPinMatch || k == DevicePairingKinds::DisplayPin) {
                        let pin = a.Pin().map(|p| p.to_string()).unwrap_or_default();
                        with(|s| s.note = format!("PIN: {}  (compare with the device)", pin));
                    }
                    a.Accept()?;
                }
                Ok(())
            }))?;
            let kinds = DevicePairingKinds::ConfirmOnly | DevicePairingKinds::DisplayPin | DevicePairingKinds::ConfirmPinMatch;
            let r = custom.PairAsync(kinds)?.join()?;
            Ok(pairing_text(r.Status()?.0))
        })()
        .unwrap_or_else(|e| format!("Could not pair: {}", e.message()));
        with(|s| s.note = msg);
        BUSY.store(false, Relaxed);
        refresh(|| {});
        done();
    });
}

/// Removes a paired device.
pub fn unpair(id: String, done: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let msg = (|| -> windows::core::Result<String> {
            let info = DeviceInformation::CreateFromIdAsync(&HSTRING::from(&id))?.join()?;
            let r = info.Pairing()?.UnpairAsync()?.join()?;
            Ok(if r.Status()? == DeviceUnpairingResultStatus::Unpaired { "Device removed".into() } else { format!("Could not remove (code {})", r.Status()?.0) })
        })()
        .unwrap_or_else(|e| format!("Could not remove: {}", e.message()));
        with(|s| s.note = msg);
        BUSY.store(false, Relaxed);
        refresh(|| {});
        done();
    });
}

pub fn take_note() -> Option<String> {
    with(|s| std::mem::take(&mut s.note)).filter(|n| !n.is_empty())
}
