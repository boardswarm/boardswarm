use std::time::Duration;

use tokio::{fs::File, sync::watch};
use tracing::instrument;
use usb_gadget::{Class, Id, Strings, UdcState, function::hid::Hid};

use crate::Server;

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
// Maximal size of the input descriptor
const KEYBOARD_DESC_INPUT_SIZE: u8 = 6;

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

struct Keyboard {
    hidg: File,
    udc_state: watch::Receiver<UdcState>,
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

        let keyboard = Keyboard {
            hidg,
            udc_state: tx.clone(),
        };

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
            watcher.send_if_modified(|&mut s| {
                if s == state {
                    false
                } else {
                    s = state;
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
