use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    registry::{self, Properties, RegistryChange},
    ActuatorError, Console, DeviceConfigItem, DeviceMonitor, DeviceSetModeError, Server,
};

// TODO deal with closing
struct DeviceNotifier {
    sender: broadcast::Sender<()>,
}

impl DeviceNotifier {
    fn new() -> Self {
        Self {
            sender: broadcast::channel(1).0,
        }
    }

    async fn notify(&self) {
        let _ = self.sender.send(());
    }

    fn watch(&self) -> DeviceMonitor {
        DeviceMonitor {
            receiver: self.sender.subscribe(),
        }
    }
}
struct DeviceMode {
    name: String,
    depends: Option<String>,
    sequence: Vec<DeviceItem<crate::config::ModeStep>>,
}

impl From<crate::config::Mode> for DeviceMode {
    fn from(config: crate::config::Mode) -> Self {
        let sequence = config.sequence.into_iter().map(DeviceItem::new).collect();
        DeviceMode {
            name: config.name,
            depends: config.depends,
            sequence,
        }
    }
}

struct DeviceItem<C> {
    config: C,
    id: Mutex<Option<u64>>,
}

impl<C> DeviceItem<C>
where
    C: DeviceConfigItem,
{
    fn new(config: C) -> Self {
        Self {
            config,
            id: Mutex::new(None),
        }
    }

    fn config(&self) -> &C {
        &self.config
    }

    fn set(&self, id: Option<u64>) {
        *self.id.lock().unwrap() = id;
    }

    fn get(&self) -> Option<u64> {
        *self.id.lock().unwrap()
    }

    fn unset_if_matches(&self, id: u64) -> bool {
        let mut i = self.id.lock().unwrap();
        match *i {
            Some(item_id) if item_id == id => {
                *i = None;
                true
            }
            _ => false,
        }
    }

    fn set_if_matches(&self, id: u64, properties: &Properties) -> bool {
        if self.config.matches(properties) {
            self.set(Some(id));
            true
        } else {
            false
        }
    }
}

struct DeviceInner {
    notifier: DeviceNotifier,
    name: String,
    current_mode: std::sync::Mutex<Option<String>>,
    consoles: Vec<DeviceItem<crate::config::Console>>,
    volumes: Vec<DeviceItem<crate::config::Volume>>,
    modes: Vec<DeviceMode>,
    server: Server,
}

#[derive(Clone)]
pub struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    pub fn from_config(config: crate::config::Device, server: Server) -> Device {
        let name = config.name;
        let consoles = config.consoles.into_iter().map(DeviceItem::new).collect();
        let volumes = config.volumes.into_iter().map(DeviceItem::new).collect();
        let notifier = DeviceNotifier::new();
        let modes = config.modes.into_iter().map(Into::into).collect();
        let device = Device {
            inner: Arc::new(DeviceInner {
                notifier,
                name,
                current_mode: Mutex::new(None),
                consoles,
                volumes,
                modes,
                server,
            }),
        };
        let d = device.clone();
        tokio::spawn(async move {
            loop {
                d.monitor_items().await
            }
        });
        device
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    async fn monitor_items(&self) {
        fn add_item_with<'a, C, I, F, IT>(items: I, id: u64, item: registry::Item<IT>, f: F) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &IT),
        {
            items.fold(false, |changed, i| {
                if i.set_if_matches(id, &item.properties()) {
                    f(i, item.inner());
                    true
                } else {
                    changed
                }
            })
        }

        fn add_item<'a, T, C, I>(items: I, id: u64, item: registry::Item<T>) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
        {
            add_item_with(items, id, item, |_, _| {})
        }

        fn change_with<'a, T, C, I, F>(items: I, change: RegistryChange<T>, f: F) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &T),
        {
            match change {
                registry::RegistryChange::Added { id, item } => add_item_with(items, id, item, f),
                registry::RegistryChange::Removed(id) => {
                    items.fold(false, |changed, c| c.unset_if_matches(id) || changed)
                }
            }
        }
        fn change<'a, T, C: DeviceConfigItem + 'a, I: Iterator<Item = &'a DeviceItem<C>>>(
            items: I,
            change: RegistryChange<T>,
        ) -> bool {
            change_with(items, change, |_, _| {})
        }
        fn setup_console(dev: &DeviceItem<crate::config::Console>, console: &Arc<dyn Console>) {
            if let Err(e) = console.configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                dev.config().parameters.clone(),
            ))) {
                warn!("Failed to configure console: {}", e);
            }
        }

        let mut actuator_monitor = self.inner.server.inner.actuators.monitor();
        let mut console_monitor = self.inner.server.inner.consoles.monitor();
        let mut volume_monitor = self.inner.server.inner.volumes.monitor();
        let mut changed = false;

        for (id, item) in self.inner.server.inner.actuators.contents() {
            changed |= add_item(
                self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                id,
                item,
            );
        }

        for (id, item) in self.inner.server.inner.consoles.contents() {
            changed |= add_item_with(self.inner.consoles.iter(), id, item, setup_console);
        }

        for (id, item) in self.inner.server.inner.volumes.contents() {
            changed |= add_item(self.inner.volumes.iter(), id, item);
        }

        if changed {
            self.inner.notifier.notify().await;
        }

        loop {
            let changed = tokio::select! {
                msg = console_monitor.recv() => {
                    match msg {
                        Ok(c) => change_with(self.inner.consoles.iter(), c, setup_console),
                        Err(e) => {
                            warn!("Issue with monitoring consoles: {:?}", e); return },
                    }
                }
                msg = actuator_monitor.recv() => {
                    match msg {
                        Ok(c) => change(
                            self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                            c),
                        Err(e) => {
                            warn!("Issue with monitoring actuators: {:?}", e); return },
                        }
                }
                msg = volume_monitor.recv() => {
                    match msg {
                        Ok(c) => change(self.inner.volumes.iter(), c),
                        Err(e) => {
                            warn!("Issue with monitoring volumes: {:?}", e); return },
                    }
                }
            };
            if changed {
                self.inner.notifier.notify().await;
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::Device for Device {
    async fn set_mode(&self, mode: &str) -> Result<(), DeviceSetModeError> {
        let target = self
            .inner
            .modes
            .iter()
            .find(|m| m.name == mode)
            .ok_or(DeviceSetModeError::ModeNotFound)?;
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            if let Some(depend) = &target.depends {
                if current.as_ref() != Some(depend) {
                    return Err(DeviceSetModeError::WrongCurrentMode);
                }
            }
            *current = None;
        }

        for step in &target.sequence {
            let step = step.config();
            if let Some(provider) = self.inner.server.find_actuator(&step.match_) {
                provider
                    .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                        step.parameters.clone(),
                    )))
                    .await?;
            } else {
                warn!("Provider {:?} not found", &step.match_);
                return Err(ActuatorError {}.into());
            }
            if let Some(duration) = step.stabilisation {
                tokio::time::sleep(duration).await;
            }
        }
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            *current = Some(mode.to_string());
        }
        self.inner.notifier.notify().await;
        Ok(())
    }

    fn updates(&self) -> DeviceMonitor {
        self.inner.notifier.watch()
    }

    fn consoles(&self) -> Vec<crate::DeviceConsole> {
        self.inner
            .consoles
            .iter()
            .map(|c| crate::DeviceConsole {
                name: c.config().name.clone(),
                id: c.get(),
            })
            .collect()
    }

    fn volumes(&self) -> Vec<crate::DeviceVolume> {
        self.inner
            .volumes
            .iter()
            .map(|v| crate::DeviceVolume {
                name: v.config().name.clone(),
                id: v.get(),
            })
            .collect()
    }

    fn modes(&self) -> Vec<crate::DeviceMode> {
        self.inner
            .modes
            .iter()
            .map(|m| crate::DeviceMode {
                name: m.name.clone(),
                depends: m.depends.clone(),
                available: m.sequence.iter().all(|s| s.get().is_some()),
            })
            .collect()
    }

    fn current_mode(&self) -> Option<String> {
        let mode = self.inner.current_mode.lock().unwrap();
        mode.clone()
    }
}
