//! Bluetooth Low Energy transport: HID over GATT (HOGP). Registers a GATT
//! server (HID + Device Information + Battery services) and an LE advertisement,
//! and pushes input reports as GATT notifications on per-report characteristics.
//! See design/ARCH.md §4.2, §7.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bluer::adv::{Advertisement, AdvertisementHandle, Type};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicNotifier, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod,
    Descriptor, DescriptorRead, Service,
};
use bluer::{Adapter, Uuid, UuidExt};
use futures::FutureExt;
use log::info;
use tokio::sync::{Mutex, mpsc, watch};

/// Build a Bluetooth SIG 16-bit UUID as a full 128-bit [`Uuid`].
fn uuid16(v: u16) -> Uuid {
    <Uuid as UuidExt>::from_u16(v)
}

use super::{Accept, Flow, Step, Transport, dispatch};
use crate::report::{InputState, Outcome, RawEvent};
use crate::sdp::{self, GAMEPAD_REPORT_ID_BASE};
use crate::{AppError, Ctx, Signals};

// Assigned 16-bit UUIDs (Bluetooth SIG).
const HID_SERVICE: u16 = 0x1812;
const DEVICE_INFO_SERVICE: u16 = 0x180A;
const BATTERY_SERVICE: u16 = 0x180F;

const HID_INFORMATION: u16 = 0x2A4A;
const REPORT_MAP: u16 = 0x2A4B;
const REPORT: u16 = 0x2A4D;
const HID_CONTROL_POINT: u16 = 0x2A4C;
const PROTOCOL_MODE: u16 = 0x2A4E;
const REPORT_REFERENCE: u16 = 0x2908;

const PNP_ID: u16 = 0x2A50;
const MANUFACTURER_NAME: u16 = 0x2A29;
const MODEL_NUMBER: u16 = 0x2A24;
const BATTERY_LEVEL: u16 = 0x2A19;

/// GATT Appearance advertised to hosts (they key their device icon off this).
/// A combo keyboard/mouse device advertises the Keyboard appearance, matching
/// the keyboard identity the classic transport sets as its Class of Device.
const APPEARANCE_KEYBOARD: u16 = 0x03C1;

/// Report-type value used in the Report Reference descriptor: Input report.
const REPORT_TYPE_INPUT: u8 = 0x01;

/// State shared between the GATT notify callbacks and [`Transport::send_report`].
/// Maps each report id to the notification session opened when a host subscribes
/// to that Report characteristic's CCCD, and tracks how many are active so the
/// connected/disconnected edge can be signalled to the session loop.
struct Shared {
    notifiers: Mutex<HashMap<u8, CharacteristicNotifier>>,
    subscribers: AtomicUsize,
    connected_tx: watch::Sender<bool>,
}

impl Shared {
    /// Record a new notification session for `report_id`, signalling
    /// "connected" on the first subscription. Spawns a watcher that removes the
    /// session (and signals "disconnected" when the last one goes) once the host
    /// unsubscribes or drops the link.
    async fn subscribe(self: &Arc<Self>, report_id: u8, notifier: CharacteristicNotifier) {
        let stopped = notifier.stopped();
        self.notifiers.lock().await.insert(report_id, notifier);
        if self.subscribers.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self.connected_tx.send(true);
        }
        let this = self.clone();
        tokio::spawn(async move {
            stopped.await;
            this.notifiers.lock().await.remove(&report_id);
            if this.subscribers.fetch_sub(1, Ordering::SeqCst) == 1 {
                let _ = this.connected_tx.send(false);
            }
        });
    }

    /// Notify the subscribed host on the characteristic for `report_id`. No-ops
    /// when the host has not subscribed to that report (design/ARCH.md §4.2).
    async fn notify(&self, report_id: u8, payload: &[u8]) {
        let mut notifiers = self.notifiers.lock().await;
        if let Some(n) = notifiers.get_mut(&report_id)
            && n.notify(payload.to_vec()).await.is_err()
        {
            notifiers.remove(&report_id);
        }
    }
}

/// The LE transport. Holds the registered GATT application and advertisement
/// (dropping the handles unregisters them) plus the shared notification state.
pub struct Le {
    adapter: Adapter,
    shared: Arc<Shared>,
    connected_rx: watch::Receiver<bool>,
    _app: ApplicationHandle,
    _adv: AdvertisementHandle,
}

impl Le {
    /// Power the adapter, register the HOGP GATT tree and an LE advertisement of
    /// the HID service. The pairing agent (needed for the bonded link HOGP
    /// requires) is the shared one registered by `main::run` (design/CONNECTION.md
    /// §5).
    pub async fn new(adapter: Adapter, n_gamepads: usize) -> Result<Self, AppError> {
        adapter
            .set_powered(true)
            .await
            .map_err(|e| AppError::new(1, format!("cannot power adapter: {e}")))?;

        let (connected_tx, connected_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            notifiers: Mutex::new(HashMap::new()),
            subscribers: AtomicUsize::new(0),
            connected_tx,
        });

        let app = Application {
            services: vec![
                device_info_service(),
                battery_service(),
                hid_service(&shared, n_gamepads),
            ],
            ..Default::default()
        };
        let _app = adapter
            .serve_gatt_application(app)
            .await
            .map_err(|e| AppError::new(1, format!("cannot register GATT application: {e}")))?;

        let adv = Advertisement {
            advertisement_type: Type::Peripheral,
            service_uuids: [uuid16(HID_SERVICE)].into_iter().collect(),
            local_name: Some("blooter".to_string()),
            appearance: Some(APPEARANCE_KEYBOARD),
            discoverable: Some(true),
            ..Default::default()
        };
        let _adv = adapter
            .advertise(adv)
            .await
            .map_err(|e| AppError::new(1, format!("cannot start LE advertisement: {e}")))?;

        info!("BLE HOGP device advertising as \"blooter\"");
        Ok(Self {
            adapter,
            shared,
            connected_rx,
            _app,
            _adv,
        })
    }

    /// Best-effort address of a connected host, for logging.
    async fn peer(&self) -> String {
        if let Ok(addrs) = self.adapter.device_addresses().await {
            for addr in addrs {
                if let Ok(dev) = self.adapter.device(addr)
                    && dev.is_connected().await.unwrap_or(false)
                {
                    return addr.to_string();
                }
            }
        }
        "BLE host".to_string()
    }
}

impl Transport for Le {
    async fn send_report(&self, report: &[u8]) -> bool {
        // Strip the 0xA1 HIDP header + report id; the id routes to a
        // characteristic and the payload is the notification value.
        if report.len() < 2 {
            return true;
        }
        self.shared.notify(report[1], &report[2..]).await;
        true
    }

    async fn on_connected(&self, state: &InputState) {
        // Give the host initial state for every report it may have subscribed
        // to (design/ARCH.md §4.2). Unsubscribed reports no-op.
        self.send_report(&state.mouse_report(0, 0, 0)).await;
        self.send_report(&InputState::keys_up_report()).await;
        for r in state.gamepad_neutral_reports() {
            self.send_report(&r).await;
        }
    }

    async fn wait_connected(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Accept {
        if *self.connected_rx.borrow_and_update() {
            return Accept::Connected(self.peer().await);
        }
        loop {
            tokio::select! {
                r = self.connected_rx.changed() => {
                    if r.is_err() {
                        return Accept::Shutdown; // sender gone (shutting down)
                    }
                    if *self.connected_rx.borrow_and_update() {
                        return Accept::Connected(self.peer().await);
                    }
                }
                Some(ev) = rx.recv() => {
                    if matches!(ctx.translate(state, ev), Outcome::Exit) {
                        return Accept::Shutdown;
                    }
                }
                _ = signals.term.recv() => return Accept::Shutdown,
                _ = signals.hup.recv() => return Accept::Shutdown,
                _ = signals.int.recv() => return Accept::Shutdown, // no session active
            }
        }
    }

    async fn run_session(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Flow {
        // Clone the watch receiver so the select can await it without borrowing
        // `self`, leaving `self` free for the shared per-event dispatch.
        let mut connected = self.connected_rx.clone();
        loop {
            tokio::select! {
                r = connected.changed() => {
                    if r.is_err() || !*connected.borrow_and_update() {
                        return Flow::Continue; // host unsubscribed / disconnected
                    }
                }
                Some(ev) = rx.recv() => {
                    if let Step::Return(f) = dispatch(self, ctx, state, ev).await {
                        return f;
                    }
                }
                _ = signals.term.recv() => return Flow::Shutdown,
                _ = signals.hup.recv() => return Flow::Shutdown,
                // SIGINT deliberately not selected: ignored during a session.
            }
        }
    }
}

/// The HID service (0x1812): HID Information, Report Map, Protocol Mode, HID
/// Control Point, and one Report characteristic per report id (design/ARCH.md §4.2).
fn hid_service(shared: &Arc<Shared>, n_gamepads: usize) -> Service {
    let mut characteristics = vec![
        // HID Information: bcdHID 1.11, country 0, flags NormallyConnectable.
        read_char(HID_INFORMATION, vec![0x11, 0x01, 0x00, 0x02], true),
        // Report Map: the exact HID report descriptor (reused from the classic
        // SDP path, design/ARCH.md §4.2).
        read_char(REPORT_MAP, sdp::report_descriptor(n_gamepads), true),
        protocol_mode_char(),
        hid_control_point_char(),
    ];

    // Report characteristics: mouse (id 1), keyboard (id 2), gamepads (3+).
    let mut report_ids = vec![1u8, 2u8];
    report_ids.extend((0..n_gamepads).map(|i| GAMEPAD_REPORT_ID_BASE + i as u8));
    for id in report_ids {
        characteristics.push(report_char(shared.clone(), id));
    }

    Service {
        uuid: uuid16(HID_SERVICE),
        primary: true,
        characteristics,
        ..Default::default()
    }
}

/// A Report characteristic (0x2A4D): Read + Notify, with a Report Reference
/// descriptor tying it to `report_id` as an Input report and a CCCD (auto-added
/// by BlueZ because Notify is set). Reads require encryption (HOGP, design/ARCH.md §4.2).
fn report_char(shared: Arc<Shared>, report_id: u8) -> Characteristic {
    Characteristic {
        uuid: uuid16(REPORT),
        read: Some(CharacteristicRead {
            read: true,
            encrypt_read: true,
            // Initial read returns nothing until the input pipeline pushes; a
            // zero-length value is a valid empty report.
            fun: Box::new(|_| async move { Ok(Vec::new()) }.boxed()),
            ..Default::default()
        }),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
                let shared = shared.clone();
                async move {
                    shared.subscribe(report_id, notifier).await;
                }
                .boxed()
            })),
            ..Default::default()
        }),
        descriptors: vec![report_reference_descriptor(report_id)],
        ..Default::default()
    }
}

/// Report Reference descriptor (0x2908): `[report_id, type=Input]`.
fn report_reference_descriptor(report_id: u8) -> Descriptor {
    Descriptor {
        uuid: uuid16(REPORT_REFERENCE),
        read: Some(DescriptorRead {
            read: true,
            encrypt_read: true,
            fun: Box::new(move |_| async move { Ok(vec![report_id, REPORT_TYPE_INPUT]) }.boxed()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Protocol Mode (0x2A4E): Read + WriteWithoutResponse. Report Protocol (0x01)
/// is the only supported mode; writes (boot-mode switching) are accepted and
/// ignored (design/ARCH.md §4.2).
fn protocol_mode_char() -> Characteristic {
    Characteristic {
        uuid: uuid16(PROTOCOL_MODE),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(|_| async move { Ok(vec![0x01]) }.boxed()),
            ..Default::default()
        }),
        write: Some(CharacteristicWrite {
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(|_, _| async move { Ok(()) }.boxed())),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// HID Control Point (0x2A4C): WriteWithoutResponse. Suspend/exit-suspend
/// commands are accepted and ignored (design/ARCH.md §4.2).
fn hid_control_point_char() -> Characteristic {
    Characteristic {
        uuid: uuid16(HID_CONTROL_POINT),
        write: Some(CharacteristicWrite {
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(|_, _| async move { Ok(()) }.boxed())),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Device Information service (0x180A): PnP ID plus Manufacturer/Model strings
/// (design/ARCH.md §4.2).
fn device_info_service() -> Service {
    Service {
        uuid: uuid16(DEVICE_INFO_SERVICE),
        primary: true,
        characteristics: vec![
            // PnP ID: source USB (0x02), VID 0x1D6B (Linux Foundation),
            // PID 0x0001, version 1.0.0.
            read_char(
                PNP_ID,
                vec![0x02, 0x6B, 0x1D, 0x01, 0x00, 0x00, 0x01],
                false,
            ),
            read_char(MANUFACTURER_NAME, b"blooter".to_vec(), false),
            read_char(MODEL_NUMBER, b"blooter HID".to_vec(), false),
        ],
        ..Default::default()
    }
}

/// Battery service (0x180F): a constant 100% Battery Level, Read + Notify
/// (mandatory HOGP companion, design/ARCH.md §4.2). blooter never pushes a notification.
fn battery_service() -> Service {
    Service {
        uuid: uuid16(BATTERY_SERVICE),
        primary: true,
        characteristics: vec![Characteristic {
            uuid: uuid16(BATTERY_LEVEL),
            read: Some(CharacteristicRead {
                read: true,
                fun: Box::new(|_| async move { Ok(vec![100]) }.boxed()),
                ..Default::default()
            }),
            notify: Some(CharacteristicNotify {
                notify: true,
                method: CharacteristicNotifyMethod::Fun(Box::new(|_| async move {}.boxed())),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// A read-only characteristic returning a fixed value, optionally requiring an
/// encrypted link to read.
fn read_char(uuid: u16, value: Vec<u8>, encrypt: bool) -> Characteristic {
    Characteristic {
        uuid: uuid16(uuid),
        read: Some(CharacteristicRead {
            read: true,
            encrypt_read: encrypt,
            fun: Box::new(move |_| {
                let value = value.clone();
                async move { Ok(value) }.boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
