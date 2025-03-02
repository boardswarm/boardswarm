use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, instrument, warn};

use crate::{registry, serial::SerialProvider, udev::Device, ActuatorError, Properties, Server};

pub const PROVIDER: &str = "qcomlt-debug-board";

/* this section exists because I cannot figure out how to add a second Actuator trait to:

impl<IT> Registrations<IT> for DeviceRegistrations<IT>
where
    IT: crate::Volume + 'static,


so I simply copied it all here instead with the prefix Actuator. Ooops!!!!!!!!!!

I am happy to fix the general traight as a commit before this PR, but I cannot see how to. Handholding needed.
*/
#[derive(Debug)]
enum ActuatorRegistrationState {
    /// Pending registration for a given udev sequence number
    Pending(u64),
    /// Registered with actuator id
    Registered(u64),
}

#[derive(Clone)]
struct ActuatorRegistrations {
    server: Server,
    registrations: Arc<Mutex<HashMap<PathBuf, ActuatorRegistrationState>>>,
}

impl ActuatorRegistrations {
    fn pre_register(&self, syspath: &Path, seqnum: u64) {
        let mut registrations = self.registrations.lock().unwrap();
        if let Some(existing) = registrations.insert(
            syspath.to_path_buf(),
            ActuatorRegistrationState::Pending(seqnum),
        ) {
            warn!("Pre-registering with known previous item: {:?}", existing);
            if let ActuatorRegistrationState::Registered(id) = existing {
                self.server.unregister_actuator(id)
            }
        }
    }

    // TODO Unused function copied blindly, maybe we don't need it?
    /*fn failed(&self, syspath: &Path, seqnum: u64) {
        let mut registrations = self.registrations.lock().unwrap();
        match registrations.get(syspath) {
            Some(ActuatorRegistrationState::Pending(s)) if *s == seqnum => {
                registrations.remove(syspath);
            }
            _ => {
                info!("Ignoring outdated failure (seqnum: {seqnum})")
            }
        }
    }*/

    fn register(
        &self,
        syspath: &Path,
        seqnum: u64,
        actuator: QCOMLTDebugBoardActuator,
        properties: Properties,
    ) {
        let mut registrations = self.registrations.lock().unwrap();
        match registrations.get(syspath) {
            Some(ActuatorRegistrationState::Pending(s)) if *s == seqnum => {
                let id = self.server.register_actuator(properties, actuator);
                registrations.insert(
                    syspath.to_path_buf(),
                    ActuatorRegistrationState::Registered(id),
                );
            }
            _ => {
                info!("Ignoring outdated registration (seqnum: {seqnum})")
            }
        }
    }

    fn remove(&self, syspath: &Path) {
        let mut registrations = self.registrations.lock().unwrap();
        if let Some(ActuatorRegistrationState::Registered(id)) = registrations.remove(syspath) {
            self.server.unregister_actuator(id);
        }
    }
}
/* end of section */

pub struct QCOMLTDebugBoardProvider {
    name: String,
    registrations: ActuatorRegistrations,
}

impl QCOMLTDebugBoardProvider {
    pub fn new(name: String, server: Server) -> Self {
        Self {
            name,
            registrations: ActuatorRegistrations {
                server,
                registrations: Default::default(),
            },
        }
    }
}

impl SerialProvider for QCOMLTDebugBoardProvider {
    fn handle(&mut self, device: &Device, seqnum: u64) -> bool {
        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];

        // The QCOMLT Debug Board uses off-the-shelf Atmel VID/PIDs...
        if device.property_u64("ID_VENDOR_ID", 16) != Some(0x03eb) {
            return false;
        };
        if device.property_u64("ID_MODEL_ID", 16) != Some(0x2404) {
            return false;
        };

        // ... but also advertises some additional string properties
        // to allow further identification of the device
        if device.property("ID_VENDOR") != Some("Linaro") {
            return false;
        };
        if device.property("ID_MODEL") != Some("DebugBoard") {
            return false;
        };

        // TODO: Perhaps we can link the two later?
        // We only care about the debug board's "management" serial port, e.g.
        // the first serial port the device enumerates. This is used for functions
        // such as turning the DUT power on, getting the DUT current etc.
        //
        // The second serial port is used for the DUT console, that
        // can be used by the regular serial provider (and matching by
        // ID_USB_SERIAL_SHORT)
        if device.property("ID_USB_INTERFACE_NUM") != Some("00") {
            return false;
        };

        if let Some(node) = device.devnode() {
            if let Some(name) = node.file_name() {
                let serial_number = device.property("ID_USB_SERIAL_SHORT").unwrap();
                info!(
                    "Reserving console {} for QCOMLT Debug Board {}",
                    node.display(),
                    serial_number
                );

                // This only allows us to register ONE actuator.
                // TODO create an array of actuators for each instance. May want Sjoerd's help on this one.
                self.registrations.pre_register(device.syspath(), seqnum);

                let mut properties = device.properties(name.to_string_lossy());
                properties.extend(provider_properties);
                properties.insert("qcomlt_debug_board.serial", serial_number);

                tokio::spawn(setup_subdevices(
                    self.registrations.clone(),
                    device.syspath().to_path_buf(),
                    node.to_path_buf(),
                    seqnum,
                    properties,
                ));

                return true;
            }
        }
        false
    }

    fn remove(&mut self, device: &Device) {
        self.registrations.remove(device.syspath());
    }
}

#[instrument(skip(r, properties))]
async fn setup_subdevices(
    r: ActuatorRegistrations,
    syspath: PathBuf,
    node: PathBuf,
    seqnum: u64,
    properties: Properties,
) {
    info!(
        "Setting up QCOMLTDebugBoard actuators for {}",
        node.display()
    );

    // Open the serial port
    let port = match tokio_serial::new(node.to_string_lossy(), 115200).open_native_async() {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };

    // TODO: Add actuator name as a property to each actuator
    // TODO: Do we need to register some kind of "type" to each actuator?

    // Now we are connected, actually register the actuators

    // Power actuator
    // TODO: The registered name is the serial port name rather than the node ?!?!?!
    let power_actuator = QCOMLTDebugBoardActuator::new(port, "power".to_string());
    r.register(&syspath, seqnum, power_actuator, properties);

    /*
    TODO:

    2025-02-06T14:29:25.710019Z  INFO start: boardswarm::qcomlt_debug_board: Reserving console /dev/ttyACM0 for QCOMLT Debug Board 75eaaac075a445150202022343a460ff
    2025-02-06T14:29:25.710237Z  INFO setup_subdevices{node="/dev/ttyACM0" seqnum=3}: boardswarm::qcomlt_debug_board: Setting up QCOMLTDebugBoard actuators for /dev/ttyACM0

    2025-02-06T14:29:25.713803Z  INFO setup_subdevices{node="/dev/ttyACM0" seqnum=3}: boardswarm::qcomlt_debug_board: Ignoring outdated registration (seqnum: 3)
    2025-02-06T14:29:25.713881Z  INFO setup_subdevices{node="/dev/ttyACM2" seqnum=1}: boardswarm::qcomlt_debug_board: Ignoring outdated registration (seqnum: 1)

        */

    // TODO: USB vbus actuator
    // TODO: Power button actuator
    // TODO: Reset button actuator

    // TODO: Much later; power measurement (voltage, current). The syntax is:
    /*
    0mV 0mA
    [Pp]ower [Uu]sb [s]tatus pwr[Bb]tn [Rr]stbtn
    */
}

// TODO Handle other debug board commands
enum QCOMLTDebugBoardCommand {
    PowerOn(tokio::sync::oneshot::Sender<Result<(), std::io::Error>>),
    PowerOff(tokio::sync::oneshot::Sender<Result<(), std::io::Error>>),
}

async fn process(
    mut port: tokio_serial::SerialStream,
    mut exec: tokio::sync::mpsc::Receiver<QCOMLTDebugBoardCommand>,
) {
    while let Some(command) = exec.recv().await {
        match command {
            /* TODO: Create a standalone rust library for the QCOMLTDebugBoard
            similar to https://github.com/boardswarm/mediatek-brom rather than
            sending raw bytes here. */
            QCOMLTDebugBoardCommand::PowerOn(sender) => {
                info!("Sending PowerOn command");
                let r = port.write_all(b"P").await;
                let _ = sender.send(r);
            }
            QCOMLTDebugBoardCommand::PowerOff(sender) => {
                info!("Sending PowerOff command");
                let r = port.write_all(b"p").await;
                let _ = sender.send(r);
            }
        }
    }
}

// TODO: dead_code is because name is never read
#[allow(dead_code)]
#[derive(Debug)]
struct QCOMLTDebugBoardActuator {
    device: tokio::sync::mpsc::Sender<QCOMLTDebugBoardCommand>,
    name: String,
}

impl QCOMLTDebugBoardActuator {
    fn new(port: tokio_serial::SerialStream, name: String) -> Self {
        let (device, exec) = tokio::sync::mpsc::channel(16);
        tokio::spawn(process(port, exec));
        Self { device, name }
    }
}

#[async_trait::async_trait]
impl crate::Actuator for QCOMLTDebugBoardActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError> {
        #[derive(Deserialize)]
        struct ModeParameters {
            mode: String,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        // TODO: Change commands based on actuator type ?
        match parameters.mode.as_str() {
            "on" => {
                let (tx, _rx) = oneshot::channel();
                let exec = QCOMLTDebugBoardCommand::PowerOn(tx);

                self.device.send(exec).await
            }
            "off" => {
                let (tx, _rx) = oneshot::channel();
                let exec = QCOMLTDebugBoardCommand::PowerOff(tx);

                self.device.send(exec).await
            }
            _ => todo!(),
        }
        // TODO map the error better and pass back the reason
        .map_err(|_e| ActuatorError {})
    }
}
