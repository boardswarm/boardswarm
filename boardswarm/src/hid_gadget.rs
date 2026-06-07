use std::{future::Future, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tokio_stream::wrappers::WatchStream;
use tracing::instrument;
use usb_gadget::{Class, Id, Strings, UdcState, function::hid::Hid};

use crate::{
    KeyboardError, KeyboardEvent, KeyboardState, Server,
    registry::{self, Properties},
};

pub const PROVIDER: &str = "hid_gadget";

const KEYBOARD_DESC: &[u8] = &[
    0x05, 0x01, // 1 byte size , global, tag 0 (usage) -  Generic desktop
    0x09, 0x06, // 1 byte size , local, tag 0 (usage), keyboard
    //  -------- application Collection start
    0xa1, 0x01, // 1 byte size, main, tag 0xa, collection applicatoin
    // //////// INPUT: key codes from left control (0xe0) - keyboard right gui aka meta (0x07) aka
    // modifiers
    // report is size 1 bit per usage item, 8 items things are reported
    0x05, 0x07, // 1 byte size, global, tag 0 (usge) - keyboard/keypad page
    0x19, 0xe0, // 1 byte size, local, tag 1 (usage min) - 224
    0x29, 0xe7, // 1 byte size, local, tag 2 (usage max) - 231
    0x15, 0x00, // 1 byte size, global, tag 1 (localal min) - 0
    0x25, 0x01, // 1 byte size, global, tag 2 (logical max) - 1
    0x75, 0x01, // 1 byte sie, global, tag 7 (report size ) - 1
    0x95, 0x08, // 1 byte size, global, tag 9 (report count) - 8
    0x81, 0x02, // 1 byte size, main, tag 8 (Input) -  variable, absolute
    // // PADDING ? 1 byte why?? -> defined for boot keyboard
    0x95, 0x01, // 1 byte size, global, tag 9 (report count) - 1
    0x75, 0x08, // 1 byte size, global, tag 7 (report size) - 8
    0x81, 0x03, // 1 byte size, main, tag 8 (Input) - constant, variable - padding?
    // ////////// OUTPUT: leds, numlock - kana
    // localal min/max 0/1, 1 bit per led, report is 5 bits
    0x95, 0x05, // 1 byte size, global, tag 9 (report count) - 5
    0x75, 0x01, // 1 byte size, global, tag 7 (report size) - 1
    0x05, 0x08, // 1 byte size, global, tag 0 (usage) - led page
    0x19, 0x01, // 1 byte size, local, tag 1, (usage min) - 1
    0x29, 0x05, // 1 byte size, local, tag 2 (usage max) - 5
    0x91, 0x02, // 1 byte size, main, tag 9 Output - Variable
    // // ////////// PAD report to 1 byte (3 + 5 bits)
    0x95, 0x01, // 1 byte size, global, tag 9, report count - 1,
    0x75, 0x03, // 1 byte size, global, tag 7, report size - 3,
    0x91, 0x03, // 1 byte size, main, tag 9, Output - Constant, Variable - padding?
    // ////////// INPUT: key codes - 0? - 0x65 keyboard application
    // each report 8 bits for each key, 6 items in the report
    0x95, 0x06, // 1 byte size, global, tag 9, report count - 6
    0x75, 0x08, // 1 byte size, global, tag 7, report size - 8,
    0x15, 0x00, // 1 byte size, global, tag 1 (logical min) - 0
    0x25, 0x65, // 1 byte size, global, tag 2 (logical max) - 0x65/101
    0x05, 0x07, // 1 byte size, global, tag 0 (usage) - keyboard/keypad page
    0x19, 0x00, // 1 byte size, local, tag 1 (usage min) - 0
    0x29, 0x65, // 1 byte size, local, tag 2 (usage max) - 0x65 / 101
    0x81, 0x00, // 1 byte size, main, tag 8, Input - Data Array
    0xc0, // 0 bytes, main, tag 0xc - end collection
          // ------------ application collection end
];
// Maximal size of the input descriptor
const KEYBOARD_DESC_INPUT_SIZE: u8 = 8;

const MOUSE_DESC: &[u8] = &[
    // Absolute mouse
    0x05, 0x01, // USAGE_PAGE (Generic Desktop)
    0x09, 0x02, // USAGE (Mouse)
    0xA1, 0x01, // COLLECTION (Application)
    // Pointer and Physical are required by Apple Recovery
    0x09, 0x01, // USAGE (Pointer)
    0xA1, 0x00, // COLLECTION (Physical)
    // 8 Buttons
    0x05, 0x09, // USAGE_PAGE (Button)
    0x19, 0x01, // USAGE_MINIMUM (Button 1)
    0x29, 0x08, // USAGE_MAXIMUM (Button 8)
    0x15, 0x00, // LOGICAL_MINIMUM (0)
    0x25, 0x01, // LOGICAL_MAXIMUM (1)
    0x95, 0x08, // REPORT_COUNT (8)
    0x75, 0x01, // REPORT_SIZE (1)
    0x81, 0x02, // INPUT (Data,Var,Abs)
    // X, Y
    0x05, 0x01, // USAGE_PAGE (Generic Desktop)
    0x09, 0x30, // USAGE (X)
    0x09, 0x31, // USAGE (Y)
    0x16, 0x00, 0x00, // LOGICAL_MINIMUM (0)
    0x26, 0xFF, 0x7F, // LOGICAL_MAXIMUM (32767)
    0x75, 0x10, // REPORT_SIZE (16)
    0x95, 0x02, // REPORT_COUNT (2)
    0x81, 0x02, // INPUT (Data,Var,Abs)
    // Wheel
    0x09, 0x38, // USAGE (Wheel)
    0x15, 0x81, // LOGICAL_MINIMUM (-127)
    0x25, 0x7F, // LOGICAL_MAXIMUM (127)
    0x75, 0x08, // REPORT_SIZE (8)
    0x95, 0x01, // REPORT_COUNT (1)
    0x81, 0x06, // INPUT (Data,Var,Rel)
    // Horizontal Wheel
    0x05, 0x0c, // USAGE PAGE ( consumer)
    0x0A, 0x38, 0x02, // USAGE (AC PAN)
    0x15, 0x81, // LOGICAL_MINIMUM (-127)
    0x25, 0x7F, // LOGICAL_MAXIMUM (127)
    0x75, 0x08, // REPORT_SIZE (8)
    0x95, 0x01, // REPORT_COUNT (1)
    0x81, 0x06, // INPUT (Data,Var,Rel)
    // End
    0xC0, // END_COLLECTION (Physical)
    0xC0, // END_COLLECTION
];
// Maximal size of the input descriptor
const MOUSE_DESC_INPUT_SIZE: u8 = 7;

#[instrument(skip(_parameters, server))]
pub fn start_provider(name: String, _parameters: serde_yaml::Value, server: Server) {
    let gadget = Gadget::new(name, server);
    tokio::spawn(gadget.run());
}

struct Mouse {
    hidg: File,
    udc_state: watch::Receiver<UdcState>,
}

#[derive(Debug, Default)]
struct KeyboardData {
    down: [u8; 6],
    modifiers: u8,
}

const KEYS: usize = 6;
impl KeyboardData {
    fn new() -> Self {
        Self {
            down: [0; KEYS],
            modifiers: 0x0,
        }
    }

    fn key_down(&mut self, key: u8) {
        for i in 0..self.down.len() {
            if self.down[i] == key {
                return;
            }
            if self.down[i] == 0 {
                self.down[i] = key;
                return;
            }
        }

        // all keys are down; drop the first
        self.down[0] = key;
        self.down.rotate_left(1);
    }

    fn key_up(&mut self, key: u8) {
        for i in 0..self.down.len() {
            if self.down[i] == key {
                self.down[i] = 0;
                self.down[i..].rotate_left(1);
            }
        }
    }

    fn event(&mut self, event: KeyboardEvent) {
        if event.is_modifier() {
            match event {
                KeyboardEvent::Down(d) => {
                    self.modifiers |= 1 << (d & 0x7);
                }
                KeyboardEvent::Up(u) => {
                    self.modifiers &= !(1 << (u & 0x7));
                }
            }
        }

        if event.is_key() {
            match event {
                KeyboardEvent::Down(d) => self.key_down(d),
                KeyboardEvent::Up(u) => self.key_up(u),
            }
        }
    }

    fn merge(&mut self, mut others: Vec<&mut KeyboardData>) {
        // merge all modifiers
        self.modifiers = 0x0;
        for o in &others {
            self.modifiers |= o.modifiers;
        }

        // Up all keys that are no longer down
        for k in self.down {
            if !others.iter().any(|o| o.down.contains(&k)) {
                self.key_up(k);
            }
        }

        // ensure all keys are down, with an overflow based on addition ordering
        // TODO ensure fairness between remote clients
        let mut dropped = vec![];
        for o in &others {
            for k in o.down {
                if !self.down.contains(&k) && !dropped.contains(&k) {
                    if self.down[KEYS - 1] != 0 {
                        // Doing to drop the first
                        dropped.push(self.down[0]);
                    }
                    self.key_down(k);
                }
            }
        }

        for k in dropped {
            for o in others.iter_mut() {
                o.key_down(k);
            }
        }
    }
}

#[derive(Debug, Default)]
struct HidKeyboardData {
    clients: Vec<(mpsc::Receiver<KeyboardEvent>, KeyboardData)>,
    state: KeyboardData,
}

impl HidKeyboardData {
    async fn recv_event(&mut self) -> (usize, Option<KeyboardEvent>) {
        std::future::poll_fn(|cx| {
            for (i, c) in self.clients.iter_mut().enumerate() {
                if let std::task::Poll::Ready(r) = c.0.poll_recv(cx) {
                    return Poll::Ready((i, r));
                }
            }
            std::task::Poll::Pending
        })
        .await
    }
}

// TODO error handling
async fn keyboard_process(
    mut hidg: File,
    mut new_client_rx: mpsc::Receiver<mpsc::Receiver<KeyboardEvent>>,
    keyboard_state_tx: watch::Sender<KeyboardState>,
    udc_state_rx: watch::Receiver<UdcState>,
) {
    // TODO
    let mut state = HidKeyboardData::default();

    loop {
        select! {
            client = new_client_rx.recv() => {
                if let Some(client)  = client {
                    state.clients.push((client, KeyboardData::new()));
                }
            },
            (index, event)= state.recv_event() => {
                // TODO drain later ones?
                match event {
                    Some(event) => state.clients[index].1.event(event),
                    None => { state.clients.swap_remove(index); }
                }
            }
        }
        // TODO ensure all client event are drained?
        // Always assuming something has change
        state
            .state
            .merge(state.clients.iter_mut().map(|x| &mut x.1).collect());

        // create report
        let mut report = [0u8; KEYBOARD_DESC_INPUT_SIZE as usize];
        report[0] = state.state.modifiers;
        report[2..].copy_from_slice(&state.state.down);

        hidg.write_all(&report).await.unwrap();

        sleep(Duration::from_millis(5)).await;
    }
}

#[derive(Debug)]
struct Keyboard {
    new_client_tx: mpsc::Sender<mpsc::Receiver<KeyboardEvent>>,
    state: watch::Receiver<KeyboardState>,
}

impl Keyboard {
    fn new(hidg: File, udc_state: watch::Receiver<UdcState>) -> Self {
        let (state_tx, state_rx) = watch::channel(Default::default());

        let (new_client_tx, new_client_rx) = mpsc::channel(1);

        tokio::spawn(keyboard_process(hidg, new_client_rx, state_tx, udc_state));

        Keyboard {
            new_client_tx,
            state: state_rx,
        }
    }
}

#[async_trait::async_trait]
impl crate::Keyboard for Keyboard {
    async fn open(
        &self,
    ) -> Result<
        (
            mpsc::Sender<KeyboardEvent>,
            futures::stream::BoxStream<'static, KeyboardState>,
        ),
        KeyboardError,
    > {
        let (tx, rx) = mpsc::channel(16);
        let state = WatchStream::new(self.state.clone());

        let _ = self.new_client_tx.send(rx).await;

        Ok((tx, state.boxed()))
    }
}

struct Gadget {
    name: String,
    server: Server,
}
impl Gadget {
    fn new(name: String, server: Server) -> Self {
        Gadget { name, server }
    }

    async fn run(self) {
        // Drop all existing gadgets
        usb_gadget::remove_all().unwrap();
        let udc = usb_gadget::default_udc().unwrap();

        let mut keyboard_builder = Hid::builder();
        keyboard_builder.protocol = 0; // HID Protocol -- boot protocol none
        keyboard_builder.sub_class = 0; // HID subclass - why not 0 why?
        // Length of input report
        keyboard_builder.report_len = KEYBOARD_DESC_INPUT_SIZE;
        keyboard_builder.report_desc = KEYBOARD_DESC.to_vec();
        keyboard_builder.no_out_endpoint = false;

        let (k_hid, k_hid_f) = keyboard_builder.build();

        let mut mouse_builder = Hid::builder();
        mouse_builder.protocol = 0; // HID Protocol -- boot protocol none
        mouse_builder.sub_class = 0; // HID subclass - why not 0 why?
        // Length of input report
        mouse_builder.report_len = MOUSE_DESC_INPUT_SIZE;
        mouse_builder.report_desc = MOUSE_DESC.to_vec();
        mouse_builder.no_out_endpoint = false;

        let (m_hid, m_hid_f) = mouse_builder.build();

        let reg = usb_gadget::Gadget::new(
            Class::INTERFACE_SPECIFIC,
            Id::LINUX_FOUNDATION_COMPOSITE,
            Strings::new("Boardswarm", "Input test", "123456"),
        )
        .with_config(
            usb_gadget::Config::new("Config 1")
                .with_function(k_hid_f)
                .with_function(m_hid_f),
        )
        .bind(&udc)
        .unwrap();

        let (watcher, tx) = watch::channel(UdcState::NotAttached);

        let hidg = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(k_hid.device_path().unwrap())
            .await
            .unwrap();

        let keyboard = Keyboard::new(hidg, tx.clone());

        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];

        let mut properties = Properties::new("HID Keyboard");
        properties.extend(provider_properties);
        self.server.register_keyboard(properties, keyboard);

        let hidg = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(m_hid.device_path().unwrap())
            .await
            .unwrap();
        let mouse = Mouse {
            hidg,
            udc_state: tx.clone(),
        };

        loop {
            let state = udc.state().unwrap();
            watcher.send_if_modified(|s| {
                if *s == state {
                    false
                } else {
                    *s = state;
                    true
                }
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/*
fn hid_ga


use std::{
    io::{Read, Write},
    time::Duration,
};

use hidreport::ReportDescriptor;
use hut::KeyboardKeypad;
use usb_gadget::{Class, Config, Gadget, Id, Strings, function::hid::Hid};

const HELLO: &[KeyboardKeypad; 6] = &[
    KeyboardKeypad::KeyboardH,
    KeyboardKeypad::KeyboardE,
    KeyboardKeypad::KeyboardL,
    KeyboardKeypad::KeyboardL,
    KeyboardKeypad::KeyboardO,
    KeyboardKeypad::KeyboardReturnEnter,
];

fn key_report(key: Option<KeyboardKeypad>) -> [u8; 8] {
    [
        0, // modifiers
        0, // reserved
        // 6 keys
        key.map(|k| k as u8).unwrap_or_default(),
        0,
        0,
        0,
        0,
        0,
    ]
}

fn main() {
    println!("Hello, world!");

    // Compatible with fixed boot report
    let report_desc = vec![
        0x05, 0x01, // 1 byte size , global, tag 0 (usage) -  Generic desktop
        0x09, 0x06, // 1 byte size , local, tag 0 (usage), keyboard
        //  -------- application Collection start
        0xa1, 0x01, // 1 byte size, main, tag 0xa, collection applicatoin
        // //////// INPUT: key codes from left control (0xe0) - keyboard right gui aka meta (0x07) aka
        // modifiers
        // report is size 1 bit per usage item, 8 items things are reported
        0x05, 0x07, // 1 byte size, global, tag 0 (usge) - keyboard/keypad page
        0x19, 0xe0, // 1 byte size, local, tag 1 (usage min) - 224
        0x29, 0xe7, // 1 byte size, local, tag 2 (usage max) - 231
        0x15, 0x00, // 1 byte size, global, tag 1 (localal min) - 0
        0x25, 0x01, // 1 byte size, global, tag 2 (logical max) - 1
        0x75, 0x01, // 1 byte sie, global, tag 7 (report size ) - 1
        0x95, 0x08, // 1 byte size, global, tag 9 (report count) - 8
        0x81, 0x02, // 1 byte size, main, tag 8 (Input) -  variable, absolute
        // // PADDING ? 1 byte why??
        0x95, 0x01, // 1 byte size, global, tag 9 (report count) - 1
        0x75, 0x08, // 1 byte size, global, tag 7 (report size) - 8
        0x81, 0x03, // 1 byte size, main, tag 8 (Input) - constant, variable - padding?
        // ////////// OUTPUT: leds, numlock - kana
        // localal min/max 0/1, 1 bit per led, report is 5 bits
        0x95, 0x05, // 1 byte size, global, tag 9 (report count) - 5
        0x75, 0x01, // 1 byte size, global, tag 7 (report size) - 1
        0x05, 0x08, // 1 byte size, global, tag 0 (usage) - led page
        0x19, 0x01, // 1 byte size, local, tag 1, (usage min) - 1
        0x29, 0x05, // 1 byte size, local, tag 2 (usage max) - 5
        0x91, 0x02, // 1 byte size, main, tag 9 Output - Variable
        // // ////////// PAD report to 1 byte (3 + 5 bits)
        0x95, 0x01, // 1 byte size, global, tag 9, report count - 1,
        0x75, 0x03, // 1 byte size, global, tag 7, report size - 3,
        0x91, 0x03, // 1 byte size, main, tag 9, Output - Constant, Variable - padding?
        // ////////// INPUT: key codes - 0? - 0x65 keyboard application
        // each report 8 bits for each key, 6 items in the report
        0x95, 0x06, // 1 byte size, global, tag 9, report count - 6
        0x75, 0x08, // 1 byte size, global, tag 7, report size - 8,
        0x15, 0x00, // 1 byte size, global, tag 1 (logical min) - 0
        0x25, 0x65, // 1 byte size, global, tag 2 (logical max) - 0x65/101
        0x05, 0x07, // 1 byte size, global, tag 0 (usage) - keyboard/keypad page
        0x19, 0x00, // 1 byte size, local, tag 1 (usage min) - 0
        0x29, 0x65, // 1 byte size, local, tag 2 (usage max) - 0x65 / 101
        0x81, 0x00, // 1 byte size, main, tag 8, Input - Data Array
        0xc0, // 0 bytes, main, tag 0xc - end collection
              // ------------ application collection end
    ];

    //let rdesc = ReportDescriptor::try_from(&report_desc).unwrap();
    //println!("=> {rdesc:x?}");

    let mouse_desc = vec![
        // Absolute mouse
        0x05, 0x01, // USAGE_PAGE (Generic Desktop)
        0x09, 0x02, // USAGE (Mouse)
        0xA1, 0x01, // COLLECTION (Application)
        // Pointer and Physical are required by Apple Recovery
        0x09, 0x01, // USAGE (Pointer)
        0xA1, 0x00, // COLLECTION (Physical)
        // 8 Buttons
        0x05, 0x09, // USAGE_PAGE (Button)
        0x19, 0x01, // USAGE_MINIMUM (Button 1)
        0x29, 0x08, // USAGE_MAXIMUM (Button 8)
        0x15, 0x00, // LOGICAL_MINIMUM (0)
        0x25, 0x01, // LOGICAL_MAXIMUM (1)
        0x95, 0x08, // REPORT_COUNT (8)
        0x75, 0x01, // REPORT_SIZE (1)
        0x81, 0x02, // INPUT (Data,Var,Abs)
        // X, Y
        0x05, 0x01, // USAGE_PAGE (Generic Desktop)
        0x09, 0x30, // USAGE (X)
        0x09, 0x31, // USAGE (Y)
        0x16, 0x00, 0x00, // LOGICAL_MINIMUM (0)
        0x26, 0xFF, 0x7F, // LOGICAL_MAXIMUM (32767)
        0x75, 0x10, // REPORT_SIZE (16)
        0x95, 0x02, // REPORT_COUNT (2)
        0x81, 0x02, // INPUT (Data,Var,Abs)
        // Wheel
        0x09, 0x38, // USAGE (Wheel)
        0x15, 0x81, // LOGICAL_MINIMUM (-127)
        0x25, 0x7F, // LOGICAL_MAXIMUM (127)
        0x75, 0x08, // REPORT_SIZE (8)
        0x95, 0x01, // REPORT_COUNT (1)
        0x81, 0x06, // INPUT (Data,Var,Rel)
        // End
        0xC0, // END_COLLECTION (Physical)
        0xC0, // END_COLLECTION
    ];

    // Drop all existing gadgets
    usb_gadget::remove_all().unwrap();
    let udc = usb_gadget::default_udc().unwrap();

    let mut builder = Hid::builder();

    builder.protocol = 0; // HID Protocol -- boot protocol none
    builder.sub_class = 0; // HID subclass - why not 0 why?
    // Length of input report
    builder.report_len = 8; //???
    builder.report_desc = report_desc;
    builder.no_out_endpoint = false;

    let (hid, hid_f) = builder.build();

    // Mouse builder
    let mut builder = Hid::builder();
    builder.protocol = 0;
    builder.sub_class = 0;
    // 1 byte, buttons, 2x2 bytes x,y, 1 byte wheel
    builder.report_len = 6;
    builder.report_desc = mouse_desc;
    builder.no_out_endpoint = false;

    let (m_hid, m_hid_f) = builder.build();

    let reg = Gadget::new(
        Class::INTERFACE_SPECIFIC,
        Id::LINUX_FOUNDATION_COMPOSITE,
        Strings::new("Boardswarm", "Input test", "123456"),
    )
    .with_config(
        Config::new("Config 1")
            .with_function(hid_f)
            .with_function(m_hid_f),
    )
    .bind(&udc)
    .unwrap();

    eprintln!("Registgered: {reg:?}");

    loop {
        match udc.state().unwrap() {
            usb_gadget::UdcState::Configured => {
                eprintln!("udc configured");
                break;
            }
            usb_gadget::UdcState::Suspended => todo!(),
            usb_gadget::UdcState::Unknown => todo!(),
            s => eprintln!("udc state: {:?}", s),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give the host some time to setup the keyboard
    std::thread::sleep(Duration::from_millis(800));

    println!("hidg: {:?} - status: {:?}", hid.device_path(), hid.status(),);
    let d = hid.device_path().unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(d)
        .unwrap();

    println!("hidg: {:?} - status: {:?}", hid.device_path(), hid.status());
    for k in HELLO {
        let report = key_report(Some(*k));
        f.write_all(&report).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let report = key_report(None);
        f.write_all(&report).unwrap();
    }

    println!(
        "mouse  hidg: {:?} - status: {:?}",
        m_hid.device_path(),
        m_hid.status(),
    );
    let d = m_hid.device_path().unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(d)
        .unwrap();

    for i in (0..u16::MAX).step_by(u16::MAX as usize / 200) {
        let x = i.to_le_bytes();
        let y = i.to_le_bytes();
        let report: [u8; 6] = [
            0x0, // no Buttons
            x[0], x[1], // X
            y[0], y[1], // Y
            0x0,  // no wheel
        ];
        f.write_all(&report).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }

    /*
    println!("Trying to read");
    let mut buf = [0; 8];
    let r = f.read(&mut buf).unwrap();
    println!("r: {r}, buf: {buf:x?}");
    */

    eprintln!("Done: {reg:?}");
    std::thread::sleep(Duration::from_hours(1));
}
*/

mod test {
    #[test]
    fn keyboard_keys() {
        let mut client = super::KeyboardData::new();
        let mut expected = [0u8; super::KEYS];

        assert_eq!(client.down, [0, 0, 0, 0, 0, 0]);
        assert_eq!(client.modifiers, 0x0);

        // Fill all slots
        for k in 0x1..=0x6 {
            client.event(crate::KeyboardEvent::Down(k));
            expected[k as usize - 1] = k;
            assert_eq!(client.down, expected);
            assert_eq!(client.modifiers, 0x0);
        }

        // Overflow should cause the oldest to drop
        client.event(crate::KeyboardEvent::Down(0x7));
        assert_eq!(client.down, [0x2, 0x3, 0x4, 0x5, 0x6, 0x7]);
        assert_eq!(client.modifiers, 0x0);

        client.event(crate::KeyboardEvent::Up(0x5));
        assert_eq!(client.down, [0x2, 0x3, 0x4, 0x6, 0x7, 0x0]);
        assert_eq!(client.modifiers, 0x0);

        client.event(crate::KeyboardEvent::Down(0x8));
        assert_eq!(client.down, [0x2, 0x3, 0x4, 0x6, 0x7, 0x8]);
        assert_eq!(client.modifiers, 0x0);

        client.event(crate::KeyboardEvent::Down(0x9));
        assert_eq!(client.down, [0x3, 0x4, 0x6, 0x7, 0x8, 0x9]);
        assert_eq!(client.modifiers, 0x0);

        client.event(crate::KeyboardEvent::Up(0x4));
        assert_eq!(client.down, [0x3, 0x6, 0x7, 0x8, 0x9, 0x0]);

        client.event(crate::KeyboardEvent::Up(0x3));
        client.event(crate::KeyboardEvent::Up(0x6));
        client.event(crate::KeyboardEvent::Up(0x7));
        client.event(crate::KeyboardEvent::Up(0x8));
        client.event(crate::KeyboardEvent::Up(0x9));
        assert_eq!(client.down, [0; super::KEYS]);
    }

    #[test]
    fn keyboard_modifiers() {
        let mut client = super::KeyboardData::new();

        client.event(crate::KeyboardEvent::Down(0xe1));
        assert_eq!(client.modifiers, 0x2);

        client.event(crate::KeyboardEvent::Down(0xe3));
        assert_eq!(client.modifiers, 0xa);

        client.event(crate::KeyboardEvent::Up(0xe1));
        assert_eq!(client.modifiers, 0x8);

        client.event(crate::KeyboardEvent::Up(0xe2));
        assert_eq!(client.modifiers, 0x8);

        client.event(crate::KeyboardEvent::Up(0xe3));
        assert_eq!(client.modifiers, 0);
    }
}
