use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;
use tokio_gpiod::{Chip, Lines};

use crate::{
    registry::{self, Properties},
    Server,
};

#[derive(Deserialize, Debug)]
struct Line {
    line_name: Option<String>,
    line_number: Option<tokio_gpiod::LineId>,
    name: String,
}

#[derive(Deserialize, Debug)]
struct GpioParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
    lines: Vec<Line>,
}

impl GpioParameters {
    fn lines_by_number(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter().filter(|l| l.line_number.is_some())
    }

    fn lines_by_name(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter().filter(|l| l.line_name.is_some())
    }
}

pub fn start_provider(_name: String, parameters: serde_yaml::Value, server: Server) {
    let parameters: GpioParameters = serde_yaml::from_value(parameters).unwrap();

    let mut enumerator = tokio_udev::Enumerator::new().unwrap();
    enumerator.match_subsystem("gpio").unwrap();
    let devices = enumerator.scan_devices().unwrap();
    for d in devices {
        if let Some(path) = d.devnode() {
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy().into_owned();
                let properties = crate::udev::properties_from_device(name, &d);
                if properties.matches(&parameters.match_) {
                    tokio::spawn(setup_gpio_chip(
                        path.to_path_buf(),
                        parameters,
                        properties,
                        server,
                    ));
                    break;
                }
            }
        }
    }
}

async fn setup_gpio_chip(
    path: PathBuf,
    parameters: GpioParameters,
    mut properties: Properties,
    server: Server,
) {
    let chip = Chip::new(path).await.unwrap();
    let label = chip.label();
    properties.insert("gpio.chip_label", label);

    for line in parameters.lines_by_number() {
        let line_number = line.line_number.unwrap();
        setup_gpio_line(&chip, line_number, &line.name, properties.clone(), &server).await;
    }

    if parameters.lines_by_name().next().is_some() {
        for i in 1..chip.num_lines() {
            let info = chip.line_info(i).await.unwrap();
            if let Some(line) = parameters
                .lines_by_name()
                .find(|l| l.line_name.as_deref().unwrap() == info.name)
            {
                setup_gpio_line(&chip, i, &line.name, properties.clone(), &server).await;
            }
        }
    }
}

async fn setup_gpio_line(
    chip: &Chip,
    line: tokio_gpiod::LineId,
    name: &str,
    mut properties: Properties,
    server: &Server,
) {
    let info = chip.line_info(line).await.unwrap();
    properties.insert(registry::NAME, name);
    properties.insert("gpio.line_name", info.name);
    properties.insert("gpio.line_number", format!("{}", line));
    let opts = tokio_gpiod::Options::output([line])
        .drive(tokio_gpiod::Drive::PushPull)
        .consumer("boardswarm");
    let line = chip.request_lines(opts).await.unwrap();
    server.register_actuator(properties, GpioLine { line });
}

struct GpioLine {
    line: Lines<tokio_gpiod::Output>,
}

impl std::fmt::Debug for GpioLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO make more meaningful
        f.debug_struct("GpioLine").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl crate::Actuator for GpioLine {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        #[derive(Deserialize)]
        struct ModeParameters {
            value: bool,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        self.line.set_values([parameters.value]).await.unwrap();
        Ok(())
    }
}
