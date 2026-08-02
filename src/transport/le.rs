//! Bluetooth Low Energy transport: HID over GATT (HOGP). Registers a GATT
//! server (HID + Device Information + Battery services) and an LE advertisement,
//! and pushes input reports as GATT notifications on per-report characteristics.
//! See design/ARCH.md §4.2, §7.
//!
//! A host is "connected" once it subscribes to a Report characteristic's CCCD.
//!
//! Unlike Classic, this transport is **purely an acceptor**. HOGP puts the HID
//! device in the GAP Peripheral role and the host in the Central role, so the
//! host is the only side that can open a link or start pairing; advertising
//! connectably *is* blooter's half of reconnecting. While waiting, the
//! interactive menu is up as a manager for hosts already bonded — not as a
//! picker (design/CONNECTION.md §4, §6).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bluer::adv::{Advertisement, AdvertisementHandle, Type};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicNotifier, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod,
    Descriptor, DescriptorRead, Service,
};
use bluer::{Adapter, Address, Uuid, UuidExt};
use futures::FutureExt;
use log::{info, warn};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::sleep;

/// Build a Bluetooth SIG 16-bit UUID as a full 128-bit [`Uuid`].
fn uuid16(v: u16) -> Uuid {
    <Uuid as UuidExt>::from_u16(v)
}

use super::{Accept, Flow, Outbox, Step, Transport, step};
use crate::menu::Pick;
use crate::report::{InputState, RawEvent};
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

/// Vendor UUID base for blooter's own attributes ("blot"/"er"/"LAYOUT"), with
/// the low 32 bits left free to carry the descriptor fingerprint
/// (design/CONNECTION.md §7.2b).
const LAYOUT_UUID_BASE: u128 = 0x626c_6f74_6572_4c41_594f_5554_0000_0000;

/// Vendor UUID of the transient service registered and unregistered to make
/// bluetoothd indicate Service Changed ("blot"/"er"/"CHUN").
const CHURN_UUID: u128 = 0x626c_6f74_6572_4348_554e_0000_0000_0000;

/// How long to leave the transient service registered, and to wait after
/// removing it, so each Service Changed indication reaches the host.
const CHURN_SETTLE: Duration = Duration::from_secs(1);

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
/// There is no outgoing-connection target: see [`Le::wait_connected`].
pub struct Le {
    adapter: Adapter,
    shared: Arc<Shared>,
    connected_rx: watch::Receiver<bool>,
    /// Whether to run the interactive menu, (re)spawned each `wait_connected`
    /// cycle so it re-opens after a disconnect (§6).
    interactive: bool,
    /// Terminal-ownership coordinator shared with the pairing agent (§5/§6).
    term_coord: crate::menu::TermCoord,
    /// Recorded per-host descriptor fingerprints, so hosts holding a stale
    /// cached GATT database can be flagged and fixed (§7).
    hosts: Arc<std::sync::Mutex<crate::state::Hosts>>,
    /// Fingerprint of the descriptor this run advertises.
    descriptor_fp: u32,
    _app: ApplicationHandle,
    _adv: AdvertisementHandle,
}

impl Le {
    /// Power the adapter, register the HOGP GATT tree and an LE advertisement of
    /// the HID service. The pairing agent (needed for the bonded link HOGP
    /// requires) is the shared one registered by `main::run` (design/CONNECTION.md
    /// §5); `interactive` enables the menu, respawned each accept cycle (§6).
    pub async fn new(
        adapter: Adapter,
        layout: sdp::Layout,
        interactive: bool,
        hosts: Arc<std::sync::Mutex<crate::state::Hosts>>,
        term_coord: crate::menu::TermCoord,
    ) -> Result<Self, AppError> {
        // Same fingerprint `main` warns about at startup, derived here from the
        // descriptor this transport actually serves (§7).
        let descriptor_fp = sdp::descriptor_fingerprint(layout);
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
                hid_service(&shared, layout),
                layout_service(descriptor_fp),
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
            interactive,
            term_coord,
            hosts,
            descriptor_fp,
            _app,
            _adv,
        })
    }

    /// Best-effort address of a connected host. `None` when bluetoothd lists no
    /// connected device (the subscription is still proof of a link, so this is
    /// only ever a naming problem).
    async fn peer(&self) -> Option<Address> {
        if let Ok(addrs) = self.adapter.device_addresses().await {
            for addr in addrs {
                if let Ok(dev) = self.adapter.device(addr)
                    && dev.is_connected().await.unwrap_or(false)
                {
                    return Some(addr);
                }
            }
        }
        None
    }

    /// A host subscribed: record which descriptor it is now bonded under, so a
    /// later change to it can be detected (§7), and name it for the session log.
    async fn connected(&self) -> Accept {
        let peer = self.peer().await;
        if let Some(addr) = peer {
            self.hosts.lock().unwrap().set(addr, self.descriptor_fp);
        }
        Accept::Connected(peer.map_or_else(|| "BLE host".to_string(), |a| a.to_string()))
    }

    /// Drop our own bond to `addr` and forget its recorded fingerprint.
    ///
    /// Only ever reached from the menu's explicit `[u]`. It used to be the
    /// fallback when `[f]` could not reach a host, which was the wrong trade:
    /// `remove_device` destroys the D-Bus object, and a host — being a central
    /// that never advertises — can never be rediscovered, so a transient failure
    /// removed it from the only UI that could act on it, permanently (§7.2b).
    async fn forget_host(&mut self, addr: Address) {
        self.hosts.lock().unwrap().forget(addr);
        match self.adapter.remove_device(addr).await {
            Ok(()) => info!("removed our bond to {addr}"),
            Err(e) => warn!("could not remove our bond to {addr}: {e}"),
        }
        println!(
            "Dropped the bond to {addr}. Remove blooter from that host's Bluetooth \
             settings too, then pair again from there to reconnect."
        );
    }

    /// Register and then unregister a throwaway service, so bluetoothd's local
    /// attribute database changes twice and it indicates Service Changed
    /// (0x2A05) over every connected link (design/CONNECTION.md §7.2b).
    async fn churn_database(&self) -> bluer::Result<()> {
        let app = Application {
            services: vec![Service {
                uuid: Uuid::from_u128(CHURN_UUID),
                primary: true,
                characteristics: vec![read_char128(CHURN_UUID + 1, vec![0])],
                ..Default::default()
            }],
            ..Default::default()
        };
        let handle = self.adapter.serve_gatt_application(app).await?;
        sleep(CHURN_SETTLE).await;
        drop(handle); // unregisters, indicating the range as changed again
        sleep(CHURN_SETTLE).await;
        Ok(())
    }

    /// Whether `addr` currently has a link up, which is the precondition for
    /// fixing it: a Service Changed indication has no queue for a client that is
    /// away.
    async fn is_connected(&self, addr: Address) -> bool {
        match self.adapter.device(addr) {
            Ok(dev) => dev.is_connected().await.unwrap_or(false),
            Err(_) => false,
        }
    }

    /// "Fix connection" on BLE (design/CONNECTION.md §7.2b): make `addr` drop the
    /// GATT database it cached under its bond — the Report Map with it — so it
    /// re-reads the current HID layout.
    ///
    /// Service Changed only reaches a *connected* client, and blooter cannot
    /// bring that link up itself (§4), so an absent host is told to connect and
    /// try again rather than being unbonded behind the user's back.
    async fn fix_host(&mut self, addr: Address) {
        if !self.is_connected(addr).await {
            println!(
                "{addr} is not connected, and as a Bluetooth LE peripheral blooter \
                 cannot call it — the host has to connect.\n\
                 Connect from {addr}, then press [f] again. Many hosts pick the change \
                 up by themselves on the next connection; if yours does not, and [f] \
                 does not help either, use [u] to drop the bond and pair again."
            );
            return;
        }
        match self.churn_database().await {
            Ok(()) => {
                info!("indicated Service Changed to {addr}");
                println!(
                    "Told {addr} its cached copy of blooter's GATT database is stale.\n\
                     It should re-read the HID layout by itself; if the new layout still \
                     does not show up, use [u] to drop the bond, remove blooter from that \
                     host's Bluetooth settings, and pair again."
                );
            }
            Err(e) => warn!("could not change the GATT database to fix {addr}: {e}"),
        }
    }
}

/// How a `wait_connected` cycle ended. Fixing and forgetting need `&mut self`,
/// so they are performed once the select's borrows are gone.
enum Done {
    Accept(Accept),
    Fix(Address),
    Forget(Address),
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
        self.send_report(state.mouse_report(0, 0, 0).as_slice())
            .await;
        self.send_report(InputState::keys_up_report().as_slice())
            .await;
        for r in state.gamepad_neutral_reports() {
            self.send_report(r.as_slice()).await;
        }
        if let Some(r) = state.consumer_up_report() {
            self.send_report(r.as_slice()).await;
        }
    }

    /// LE connection intervals are typically 7.5–30 ms, and a notify is queued
    /// on D-Bus rather than paced by the radio, so batch a little harder than
    /// Classic (design/ARCH.md §7.2c).
    fn flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(15)
    }

    /// Wait for a host to subscribe to a Report characteristic's CCCD.
    ///
    /// There is nothing to race it against. blooter is the GAP Peripheral, so
    /// the host is the only side that can open the link; the advertisement
    /// registered in [`Le::new`] is the whole of blooter's contribution, and a
    /// bonded host reconnects to it on its own. What runs alongside is the menu,
    /// which manages bonded hosts rather than picking one to dial
    /// (design/CONNECTION.md §4, §6).
    async fn wait_connected(
        &mut self,
        rx: &mut mpsc::Receiver<RawEvent>,
        state: &mut InputState,
        ctx: &Ctx<'_>,
        signals: &mut Signals,
    ) -> Accept {
        if *self.connected_rx.borrow_and_update() {
            return self.connected().await;
        }

        // The watch receiver is cloned rather than borrowed from `self`, so the
        // select's arms do not hold a borrow across the loop.
        let mut connected = self.connected_rx.clone();
        // Hosts bonded under a different descriptor are flagged in the list and
        // fixable with `[f]` (§7); `known` keeps a host in the list even after
        // bluetoothd drops its device object (§6).
        let (stale, known) = {
            let hosts = self.hosts.lock().unwrap();
            (hosts.stale(self.descriptor_fp), hosts.addresses())
        };
        let mut menu = crate::menu::Session::spawn(
            Some(&self.adapter),
            self.interactive,
            crate::menu::Kind::Ble,
            stale,
            known,
            &self.term_coord,
        );

        let done = loop {
            tokio::select! {
                r = connected.changed() => {
                    if r.is_err() {
                        break Done::Accept(Accept::Shutdown); // sender gone (shutting down)
                    }
                    if *connected.borrow_and_update() {
                        if menu.is_open() {
                            info!("a host subscribed; using it and closing the menu");
                        }
                        break Done::Accept(self.connected().await);
                    }
                }
                // Menu action. Both need `&mut self`, so leave the loop and
                // perform them once the menu task has been joined.
                picked = menu.recv() => {
                    match picked {
                        Some(Pick::Fix(addr)) => {
                            info!("menu selected {addr}; fixing connection");
                            break Done::Fix(addr);
                        }
                        Some(Pick::Forget(addr)) => {
                            info!("menu selected {addr}; dropping our bond to it");
                            break Done::Forget(addr);
                        }
                        // The BLE menu never offers a host to connect out to.
                        Some(Pick::Connect(addr)) => {
                            warn!("ignoring a connect pick for {addr}: BLE cannot initiate");
                        }
                        None => {}
                    }
                }
                Some(ev) = rx.recv() => {
                    if ctx.translate_exits(state, ev) {
                        break Done::Accept(Accept::Shutdown);
                    }
                }
                _ = signals.term.recv() => break Done::Accept(Accept::Shutdown),
                _ = signals.hup.recv() => break Done::Accept(Accept::Shutdown),
                // No session active.
                _ = signals.int.recv() => break Done::Accept(Accept::Shutdown),
            }
        };

        // Preempt the menu and wait for it to restore the terminal before
        // returning — ordered ahead of any further output (§6).
        menu.finish().await;

        // Perform the menu's action now the menu task is joined and nothing
        // borrows `self` any more, then go back to waiting (the menu re-opens).
        match done {
            Done::Fix(addr) => {
                self.fix_host(addr).await;
                Box::pin(self.wait_connected(rx, state, ctx, signals)).await
            }
            Done::Forget(addr) => {
                self.forget_host(addr).await;
                Box::pin(self.wait_connected(rx, state, ctx, signals)).await
            }
            Done::Accept(accept) => accept,
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
        // Allocated once for the connection (design/ARCH.md §7.2c).
        let mut out = Outbox::new(ctx.buffer, ctx.batch, self.flush_interval(), ctx.overflow);
        loop {
            tokio::select! {
                r = connected.changed() => {
                    if r.is_err() || !*connected.borrow_and_update() {
                        return Flow::Continue; // host unsubscribed / disconnected
                    }
                }
                inc = out.next(rx) => {
                    if let Step::Return(f) = step(self, ctx, state, &mut out, inc).await {
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
fn hid_service(shared: &Arc<Shared>, layout: sdp::Layout) -> Service {
    let mut characteristics = vec![
        // HID Information: bcdHID 1.11, country 0, flags NormallyConnectable.
        read_char(HID_INFORMATION, vec![0x11, 0x01, 0x00, 0x02], true),
        // Report Map: the exact HID report descriptor (reused from the classic
        // SDP path, design/ARCH.md §4.2).
        read_char(REPORT_MAP, sdp::report_descriptor(layout), true),
        protocol_mode_char(),
        hid_control_point_char(),
    ];

    // Report characteristics: mouse (id 1), keyboard (id 2), gamepads (3+) and,
    // with `[remote]` on, the consumer collection last (design/REMOTE.md §3.1).
    let mut report_ids = vec![1u8, 2u8];
    report_ids.extend((0..layout.n_gamepads).map(|i| GAMEPAD_REPORT_ID_BASE + i as u8));
    if layout.remote {
        report_ids.push(sdp::consumer_report_id(layout.n_gamepads));
    }
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

/// Vendor service whose single characteristic *is* the descriptor fingerprint:
/// its UUID carries the fingerprint in its low 32 bits, and it reads back the
/// same value (design/CONNECTION.md §7.2b).
///
/// A characteristic declaration's value (properties, value handle and UUID) is
/// one of the few things the GATT Database Hash covers, so encoding the
/// fingerprint there makes *any* descriptor change visible to a host doing
/// robust caching — including a change of `[pointer] axis_bits`, which alters
/// only the Report Map's value and would otherwise leave the database, its
/// handles and its hash untouched.
fn layout_service(descriptor_fp: u32) -> Service {
    Service {
        uuid: Uuid::from_u128(LAYOUT_UUID_BASE),
        primary: true,
        characteristics: vec![read_char128(
            LAYOUT_UUID_BASE | u128::from(descriptor_fp),
            descriptor_fp.to_le_bytes().to_vec(),
        )],
        ..Default::default()
    }
}

/// A read-only characteristic on a vendor (128-bit) UUID returning a fixed value.
fn read_char128(uuid: u128, value: Vec<u8>) -> Characteristic {
    Characteristic {
        uuid: Uuid::from_u128(uuid),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(move |_| {
                let value = value.clone();
                async move { Ok(value) }.boxed()
            }),
            ..Default::default()
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AxisBits::{Eight, Sixteen};

    /// The whole point of the layout service is that a descriptor change reaches
    /// the *characteristic declaration*, which is what the GATT Database Hash
    /// covers — including an axis-width change, which leaves the attribute
    /// layout identical (design/CONNECTION.md §7.2b).
    #[test]
    fn layout_uuid_carries_the_fingerprint() {
        let uuid = |n, bits, remote| {
            let svc = layout_service(sdp::descriptor_fingerprint(sdp::Layout {
                n_gamepads: n,
                axis_bits: bits,
                remote,
            }));
            assert_eq!(svc.uuid, Uuid::from_u128(LAYOUT_UUID_BASE));
            svc.characteristics[0].uuid
        };
        // Enabling the remote is a descriptor change like any other, so it must
        // reach the UUID too (design/REMOTE.md §3.2).
        let distinct = [
            uuid(0, Eight, false),
            uuid(1, Eight, false),
            uuid(0, Sixteen, false),
            uuid(0, Eight, true),
        ];
        for (i, a) in distinct.iter().enumerate() {
            for b in &distinct[i + 1..] {
                assert_ne!(a, b, "each descriptor needs its own characteristic UUID");
            }
        }
        // Same descriptor, same UUID: an unchanged run must not look like a change.
        assert_eq!(uuid(2, Sixteen, false), uuid(2, Sixteen, false));
    }
}
