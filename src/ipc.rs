//! Hyprland 0.57 data queries. Workspace addresses, not display names, identify windows' destinations.
use crate::dispatch::command_socket_path;
use hyprland::data::FullscreenMode;
use hyprland::error::HyprError;
use hyprland::shared::Address;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceKind {
    Normal,
    Special,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Workspace {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i32>,
    #[serde(rename = "type")]
    pub kind: WorkspaceKind,
    pub name: String,
    // Qualify named normal workspaces once, including names resembling special selectors.
    #[serde(skip)]
    qualified_name: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceWire {
    address: String,
    #[serde(default)]
    id: Option<i32>,
    #[serde(rename = "type")]
    kind: WorkspaceKind,
    name: String,
}

impl<'de> Deserialize<'de> for Workspace {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WorkspaceWire::deserialize(deserializer)?;
        if wire.address.is_empty() {
            return Err(serde::de::Error::custom("workspace address is empty"));
        }
        let qualified_name = (wire.kind == WorkspaceKind::Normal
            && wire.address.parse::<i32>().is_err())
        .then(|| format!("name:{}", wire.address));
        Ok(Self {
            address: wire.address,
            id: wire.id,
            kind: wire.kind,
            name: wire.name,
            qualified_name,
        })
    }
}

impl Workspace {
    pub fn numbered(id: i32) -> Self {
        let address = id.to_string();
        Self {
            name: address.clone(),
            address,
            id: Some(id),
            kind: WorkspaceKind::Normal,
            qualified_name: None,
        }
    }

    pub fn from_selector(selector: &str) -> Self {
        if selector.starts_with("special:") {
            return Self {
                address: selector.to_string(),
                id: None,
                kind: WorkspaceKind::Special,
                name: selector.to_string(),
                qualified_name: None,
            };
        }
        let address = selector.strip_prefix("name:").unwrap_or(selector);
        match address.parse::<i32>() {
            Ok(id) => Self::numbered(id),
            Err(_) => Self {
                address: address.to_string(),
                id: None,
                kind: WorkspaceKind::Normal,
                name: address.to_string(),
                qualified_name: Some(format!("name:{address}")),
            },
        }
    }

    pub fn selector(&self) -> &str {
        self.qualified_name.as_deref().unwrap_or(&self.address)
    }

    pub fn matches_selector(&self, selector: &str) -> bool {
        self.selector() == selector
            || (self.kind == WorkspaceKind::Normal && self.address == selector)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: Address,
    pub at: (i32, i32),
    pub size: (i32, i32),
    pub workspace: Workspace,
    pub monitor: Option<i128>,
    pub class: String,
    pub initial_class: String,
    pub title: String,
    pub initial_title: String,
    pub pid: i32,
    pub mapped: bool,
    pub floating: bool,
    pub pinned: bool,
    pub fullscreen: FullscreenMode,
}

pub fn request<T: DeserializeOwned>(query: &str) -> hyprland::Result<T> {
    let socket = command_socket_path().ok_or_else(|| {
        HyprError::Other("HYPRLAND_INSTANCE_SIGNATURE is not set; is Hyprland running?".into())
    })?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(query.as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    serde_json::from_slice(&reply).map_err(HyprError::SerdeError)
}

pub fn clients() -> hyprland::Result<Vec<Client>> {
    request("j/clients")
}

pub fn active_workspace() -> hyprland::Result<Workspace> {
    request("j/activeworkspace")
}

pub fn restorable(client: &Client) -> bool {
    // Compositor helpers and wallet-unlock dialogs are not persistent user applications.
    client.mapped
        && ![
            "hyprland",
            "aquamarine",
            "hyprland-donate-screen",
            "org.kde.ksecretd",
        ]
        .iter()
        .any(|class| client.class.eq_ignore_ascii_case(class))
}
