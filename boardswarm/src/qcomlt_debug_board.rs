use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn};

use crate::{ActuatorError, ActuatorId, Server, registry, serial::SerialProvider, udev::Device};

pub const PROVIDER: &str = "qcomlt-debug-board";

pub struct QCOMLTDebugBoardProvider {
    name: String,
    server: Server,
    registrations: HashMap<PathBuf, Vec<ActuatorId>>,
}

impl QCOMLTDebugBoardProvider {
    pub fn new(name: String, server: Server) -> Self {
        Self {
            name,
            server,
            registrations: Default::default(),
        }
    }
}

impl SerialProvider for QCOMLTDebugBoardProvider {
    fn handle(&mut self, device: &Device, _seqnum: u64) -> bool {
        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];

        /* QCOMLT Debug Board uses the default Atmel devkit VID/PIDs
        but advertises a custom ID_VENDOR and ID_MODEL. */
        if device.property_u64("ID_VENDOR_ID", 16) != Some(0x03eb) {
            return false;
        };
        if device.property_u64("ID_MODEL_ID", 16) != Some(0x2404) {
            return false;
        };
        if device.property("ID_VENDOR") != Some("Linaro") {
            return false;
        };
        if device.property("ID_MODEL") != Some("DebugBoard") {
            return false;
        };

        // We only care about the debug board's "management" serial port, e.g.
        // the first serial port the device enumerates. This is used for functions
        // such as turning the DUT power on, enabling USB VBUS, pressing buttons.
        //
        // The second serial port is used for the DUT console - that
        // can be used by the regular serial provider (and matching by
        // ID_USB_SERIAL_SHORT)
        if device.property("ID_USB_INTERFACE_NUM") != Some("00") {
            return false;
        };

        if let Some(node) = device.devnode()
            && let Some(name) = node.file_name()
        {
            let Some(serial_number) = device.property("ID_USB_SERIAL_SHORT") else {
                warn!(
                    "Skipping QCOMLT Debug Board on {}: missing ID_USB_SERIAL_SHORT",
                    node.display()
                );
                return false;
            };

            info!(
                "Reserving console {} for QCOMLT Debug Board {}",
                node.display(),
                serial_number
            );

            let port = match tokio_serial::new(node.to_string_lossy(), 115200).open_native_async() {
                Ok(port) => port,
                Err(e) => {
                    warn!("Failed to open QCOMLT Debug Board management serial port: {e}");
                    return false;
                }
            };

            let mut base_properties = device.properties(name.to_string_lossy());
            base_properties.extend(provider_properties);
            base_properties.insert("qcomlt_debug_board.serial", serial_number);

            let (tx, exec) = tokio::sync::mpsc::channel(16);
            tokio::spawn(process(port, exec));

            let actuator_types = [
                ("power", QCOMLTActuatorType::Power),
                ("usb-vbus", QCOMLTActuatorType::UsbVbus),
                ("power-button", QCOMLTActuatorType::PowerButton),
                ("reset-button", QCOMLTActuatorType::ResetButton),
            ];

            let ids = actuator_types
                .into_iter()
                .map(|(actuator_name, actuator_type)| {
                    let mut props = base_properties.clone();
                    props.insert(
                        registry::NAME,
                        format!("{}.{}.{}", self.name, serial_number, actuator_name),
                    );
                    self.server.register_actuator(
                        props,
                        QCOMLTDebugBoardActuator {
                            device: tx.clone(),
                            actuator_type,
                        },
                    )
                })
                .collect();

            self.registrations
                .insert(device.syspath().to_path_buf(), ids);

            return true;
        }
        false
    }

    fn remove(&mut self, device: &Device) {
        if let Some(ids) = self.registrations.remove(device.syspath()) {
            for id in ids {
                self.server.unregister_actuator(id);
            }
        }
    }
}

// TODO: Create a standalone rust library for the QCOMLTDebugBoard
// similar to https://github.com/boardswarm/mediatek-brom rather than
// sending raw bytes
struct QCOMLTDebugBoardCommand(u8, tokio::sync::oneshot::Sender<Result<(), std::io::Error>>);

async fn process(
    mut port: tokio_serial::SerialStream,
    mut exec: tokio::sync::mpsc::Receiver<QCOMLTDebugBoardCommand>,
) {
    while let Some(QCOMLTDebugBoardCommand(byte, sender)) = exec.recv().await {
        let r = port.write_all(&[byte]).await;
        let _ = sender.send(r);
    }
}

#[derive(Debug, Clone, Copy)]
enum QCOMLTActuatorType {
    Power,
    UsbVbus,
    PowerButton,
    ResetButton,
}

#[derive(Debug)]
struct QCOMLTDebugBoardActuator {
    device: tokio::sync::mpsc::Sender<QCOMLTDebugBoardCommand>,
    actuator_type: QCOMLTActuatorType,
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
        let byte = match self.actuator_type {
            QCOMLTActuatorType::Power => match parameters.mode.as_str() {
                "on" => b'P',
                "off" => b'p',
                _ => {
                    warn!(
                        "Unsupported mode '{}' for actuator {:?}",
                        parameters.mode, self.actuator_type
                    );
                    return Err(ActuatorError());
                }
            },
            QCOMLTActuatorType::UsbVbus => match parameters.mode.as_str() {
                "on" => b'U',
                "off" => b'u',
                _ => {
                    warn!(
                        "Unsupported mode '{}' for actuator {:?}",
                        parameters.mode, self.actuator_type
                    );
                    return Err(ActuatorError());
                }
            },
            QCOMLTActuatorType::PowerButton => match parameters.mode.as_str() {
                "on" => b'B',
                "off" => b'b',
                _ => {
                    warn!(
                        "Unsupported mode '{}' for actuator {:?}",
                        parameters.mode, self.actuator_type
                    );
                    return Err(ActuatorError());
                }
            },
            QCOMLTActuatorType::ResetButton => match parameters.mode.as_str() {
                "on" => b'R',
                "off" => b'r',
                _ => {
                    warn!(
                        "Unsupported mode '{}' for actuator {:?}",
                        parameters.mode, self.actuator_type
                    );
                    return Err(ActuatorError());
                }
            },
        };
        info!(
            "Sending {:?} {} command",
            self.actuator_type, parameters.mode
        );
        let (tx, rx) = oneshot::channel();
        self.device
            .send(QCOMLTDebugBoardCommand(byte, tx))
            .await
            .map_err(|e| {
                warn!("Failed to send command to debug board: {e}");
                ActuatorError()
            })?;
        rx.await
            .map_err(|e| {
                warn!("Debug board command response channel closed: {e}");
                ActuatorError()
            })?
            .map_err(|e| {
                warn!("Failed to write command to debug board: {e}");
                ActuatorError()
            })
    }
}
