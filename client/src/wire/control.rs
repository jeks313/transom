//! The control channel messages (protocol.md §4), decoded from and encoded to the
//! host's exact JSON shapes.
//!
//! **The discriminators here are coded to the host, not to the prose in
//! protocol.md.** The doc lists client→host types as `RequestResize` etc., but the
//! host's `WireProtocol.swift` actually emits and accepts camelCase
//! (`"requestResize"`, `"input"`). Per `AGENTS.md`, when the host and the doc
//! disagree the host wins, so we speak `"requestResize"`. Every field name below
//! is likewise taken from the Swift `CodingKey`s (note `hello` carries the version
//! under the key `"protocol"`, not `"protocolVersion"`).

use super::input::InputEvent;
use super::json::{JsonError, Value};

/// A rectangle in VDS physical pixels: origin top-left, Y down (I-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

/// What kind of surface a window is (protocol.md §4). The client must not give a
/// menu/sheet/popover a resizable frame or an Alt-Tab entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Normal,
    Menu,
    Sheet,
    Popover,
}

impl WindowKind {
    fn from_wire(s: &str) -> WindowKind {
        match s {
            "menu" => WindowKind::Menu,
            "sheet" => WindowKind::Sheet,
            "popover" => WindowKind::Popover,
            // Anything unrecognized is treated as a normal window rather than
            // dropped: forward-compatible with a host that adds a kind.
            _ => WindowKind::Normal,
        }
    }
}

/// Maps to the client's `WM_ENTERSIZEMOVE` / `WM_SIZING` / `WM_EXITSIZEMOVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePhase {
    Begin,
    Live,
    End,
}

impl ResizePhase {
    fn wire(self) -> &'static str {
        match self {
            ResizePhase::Begin => "begin",
            ResizePhase::Live => "live",
            ResizePhase::End => "end",
        }
    }
}

/// One lane of the session: an app (or group) on its own virtual display, with
/// its own video stream (protocol.md §1, v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lane {
    pub lane: u32,
    pub vds: Size,
}

/// One item of an app's menu bar (protocol.md §4 `menuBar`). A separator has
/// `separator` set; a submenu has `items`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub title: String,
    pub enabled: bool,
    pub checked: bool,
    /// Shortcut in this keyboard's terms ("Ctrl+Shift+N"), "" for none.
    pub shortcut: String,
    pub separator: bool,
    pub items: Vec<MenuItem>,
}

/// One window's id + rect, as carried in a `tileLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileWindow {
    pub id: u64,
    pub rect: Rect,
}

/// Host → client. The full set the host can push (`ControlMessage` in Swift).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// First message on a fresh connection: protocol version + every lane and
    /// its virtual display size, so we can sanity-check every rect we receive.
    Hello {
        protocol: u32,
        lanes: Vec<Lane>,
    },
    /// `rect` is in the VDS of `lane`. Ids are unique across lanes, so later
    /// messages carry only the id.
    WindowCreated {
        id: u64,
        lane: u32,
        /// The app that owns the window: the key of its `MenuBar`.
        app: u32,
        rect: Rect,
        title: String,
        kind: WindowKind,
    },
    /// ACTUAL geometry after an AX write or observed move (I-4), never requested.
    WindowMoved {
        id: u64,
        rect: Rect,
    },
    WindowDestroyed {
        id: u64,
    },
    WindowTitle {
        id: u64,
        title: String,
    },
    WindowFocused {
        id: u64,
    },
    TileLayout {
        lane: u32,
        windows: Vec<TileWindow>,
        display: Size,
    },
    Error {
        code: u32,
        message: String,
    },
    /// `app`'s menu bar, the Apple menu left out: the first menu's path index is 1.
    MenuBar {
        app: u32,
        menus: Vec<MenuItem>,
    },
    /// The answer to `Ping`: our stamp back and the host clock when it read it.
    Pong {
        client_micros: u64,
        host_micros: u64,
    },
    /// Text copied on the Mac, for our clipboard.
    Clipboard {
        text: String,
    },
}

/// Client → host. What we're allowed to ask the host to do.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    RequestResize {
        id: u64,
        size: Size,
        phase: ResizePhase,
    },
    RequestFocus {
        id: u64,
    },
    RequestClose {
        id: u64,
    },
    /// Ask the host to emit a fresh intra frame of window `id`'s stream (every
    /// window when None) after a decoder dropped stale compressed frames to stay
    /// within the latency budget.
    RequestKeyframe {
        id: Option<u64>,
    },
    Input {
        id: u64,
        event: InputEvent,
        /// Client's monotonic timestamp in milliseconds; opaque to the host in v1.
        ts: u64,
    },
    /// Choose the menu item at `path` of window `id`'s app; pressed only if its
    /// title is still `title`.
    MenuPick {
        id: u64,
        path: Vec<u32>,
        title: String,
    },
    /// A menu of window `id`'s app is opening: send its menu bar again.
    MenuRefresh {
        id: u64,
    },
    /// A clock probe; the host answers at once with `Pong`.
    Ping {
        client_micros: u64,
    },
    /// Text copied here, for the Mac's pasteboard.
    Clipboard {
        text: String,
    },
}

/// A decode failure with enough context to log which message shape was malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Json(JsonError),
    /// Structurally valid JSON, but not a message we understand or missing a field.
    Shape(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Json(e) => write!(f, "{e}"),
            DecodeError::Shape(m) => write!(f, "malformed control message: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<JsonError> for DecodeError {
    fn from(e: JsonError) -> Self {
        DecodeError::Json(e)
    }
}

// --- decode helpers ------------------------------------------------------

fn field<'a>(obj: &'a Value, key: &str) -> Result<&'a Value, DecodeError> {
    obj.get(key)
        .ok_or_else(|| DecodeError::Shape(format!("missing field `{key}`")))
}

fn u64_field(obj: &Value, key: &str) -> Result<u64, DecodeError> {
    field(obj, key)?
        .as_u64()
        .ok_or_else(|| DecodeError::Shape(format!("field `{key}` is not a u64")))
}

fn u32_field(obj: &Value, key: &str) -> Result<u32, DecodeError> {
    field(obj, key)?
        .as_u32()
        .ok_or_else(|| DecodeError::Shape(format!("field `{key}` is not a u32")))
}

fn str_field(obj: &Value, key: &str) -> Result<String, DecodeError> {
    Ok(field(obj, key)?
        .as_str()
        .ok_or_else(|| DecodeError::Shape(format!("field `{key}` is not a string")))?
        .to_string())
}

fn rect_field(obj: &Value, key: &str) -> Result<Rect, DecodeError> {
    let r = field(obj, key)?;
    Ok(Rect {
        x: u32_field(r, "x")?,
        y: u32_field(r, "y")?,
        w: u32_field(r, "w")?,
        h: u32_field(r, "h")?,
    })
}

fn bool_field(obj: &Value, key: &str) -> Result<bool, DecodeError> {
    match field(obj, key)? {
        Value::Bool(b) => Ok(*b),
        _ => Err(DecodeError::Shape(format!("field `{key}` is not a bool"))),
    }
}

fn menu_items(obj: &Value, key: &str, depth: usize) -> Result<Vec<MenuItem>, DecodeError> {
    // The host sends at most four levels; anything deeper is not a menu bar.
    if depth > 8 {
        return Err(DecodeError::Shape("menu nested too deeply".to_string()));
    }
    let arr = field(obj, key)?
        .as_array()
        .ok_or_else(|| DecodeError::Shape(format!("`{key}` is not an array")))?;
    arr.iter()
        .map(|m| {
            Ok(MenuItem {
                title: str_field(m, "title")?,
                enabled: bool_field(m, "enabled")?,
                checked: bool_field(m, "checked")?,
                shortcut: str_field(m, "shortcut")?,
                separator: bool_field(m, "separator")?,
                items: menu_items(m, "items", depth + 1)?,
            })
        })
        .collect()
}

fn size_field(obj: &Value, key: &str) -> Result<Size, DecodeError> {
    let s = field(obj, key)?;
    Ok(Size {
        w: u32_field(s, "w")?,
        h: u32_field(s, "h")?,
    })
}

impl ServerMessage {
    /// Decode one control-channel JSON payload (already de-framed).
    pub fn decode(payload: &[u8]) -> Result<ServerMessage, DecodeError> {
        let text = std::str::from_utf8(payload)
            .map_err(|_| DecodeError::Shape("payload is not UTF-8".to_string()))?;
        let v = Value::parse(text)?;
        let ty = field(&v, "type")?
            .as_str()
            .ok_or_else(|| DecodeError::Shape("`type` is not a string".to_string()))?;
        match ty {
            "hello" => {
                let protocol = u32_field(&v, "protocol")?;
                // A v1 host has no lanes; report its version so the handshake
                // can say which side to update.
                if protocol != super::PROTOCOL_VERSION {
                    return Ok(ServerMessage::Hello {
                        protocol,
                        lanes: Vec::new(),
                    });
                }
                let arr = field(&v, "lanes")?
                    .as_array()
                    .ok_or_else(|| DecodeError::Shape("`lanes` is not an array".to_string()))?;
                let mut lanes = Vec::with_capacity(arr.len());
                for l in arr {
                    lanes.push(Lane {
                        lane: u32_field(l, "lane")?,
                        vds: size_field(l, "vdsSize")?,
                    });
                }
                Ok(ServerMessage::Hello { protocol, lanes })
            }
            "windowCreated" => Ok(ServerMessage::WindowCreated {
                id: u64_field(&v, "id")?,
                lane: u32_field(&v, "lane")?,
                app: u32_field(&v, "app")?,
                rect: rect_field(&v, "rect")?,
                title: str_field(&v, "title")?,
                kind: WindowKind::from_wire(field(&v, "kind")?.as_str().unwrap_or("normal")),
            }),
            "windowMoved" => Ok(ServerMessage::WindowMoved {
                id: u64_field(&v, "id")?,
                rect: rect_field(&v, "rect")?,
            }),
            "windowDestroyed" => Ok(ServerMessage::WindowDestroyed {
                id: u64_field(&v, "id")?,
            }),
            "windowTitle" => Ok(ServerMessage::WindowTitle {
                id: u64_field(&v, "id")?,
                title: str_field(&v, "title")?,
            }),
            "windowFocused" => Ok(ServerMessage::WindowFocused {
                id: u64_field(&v, "id")?,
            }),
            "tileLayout" => {
                let arr = field(&v, "windows")?
                    .as_array()
                    .ok_or_else(|| DecodeError::Shape("`windows` is not an array".to_string()))?;
                let mut windows = Vec::with_capacity(arr.len());
                for w in arr {
                    windows.push(TileWindow {
                        id: u64_field(w, "id")?,
                        rect: rect_field(w, "rect")?,
                    });
                }
                Ok(ServerMessage::TileLayout {
                    lane: u32_field(&v, "lane")?,
                    windows,
                    display: size_field(&v, "displaySize")?,
                })
            }
            "error" => Ok(ServerMessage::Error {
                code: u32_field(&v, "code")?,
                message: str_field(&v, "message")?,
            }),
            "menuBar" => Ok(ServerMessage::MenuBar {
                app: u32_field(&v, "app")?,
                menus: menu_items(&v, "menus", 0)?,
            }),
            "pong" => Ok(ServerMessage::Pong {
                client_micros: u64_field(&v, "clientMicros")?,
                host_micros: u64_field(&v, "hostMicros")?,
            }),
            "clipboard" => Ok(ServerMessage::Clipboard {
                text: str_field(&v, "text")?,
            }),
            other => Err(DecodeError::Shape(format!(
                "unknown control message type `{other}`"
            ))),
        }
    }
}

impl ClientMessage {
    /// Encode to a JSON `Value` matching the host's `ClientMessage` decoder.
    pub fn to_value(&self) -> Value {
        match self {
            ClientMessage::RequestResize { id, size, phase } => Value::object(vec![
                ("type", Value::str("requestResize")),
                ("id", Value::uint(*id)),
                (
                    "size",
                    Value::object(vec![
                        ("w", Value::uint(size.w as u64)),
                        ("h", Value::uint(size.h as u64)),
                    ]),
                ),
                ("phase", Value::str(phase.wire())),
            ]),
            ClientMessage::RequestFocus { id } => Value::object(vec![
                ("type", Value::str("requestFocus")),
                ("id", Value::uint(*id)),
            ]),
            ClientMessage::RequestClose { id } => Value::object(vec![
                ("type", Value::str("requestClose")),
                ("id", Value::uint(*id)),
            ]),
            ClientMessage::RequestKeyframe { id } => {
                let mut fields = vec![("type", Value::str("requestKeyframe"))];
                if let Some(id) = id {
                    fields.push(("id", Value::uint(*id)));
                }
                Value::object(fields)
            }
            ClientMessage::Input { id, event, ts } => Value::object(vec![
                ("type", Value::str("input")),
                ("id", Value::uint(*id)),
                ("event", event.to_value()),
                ("ts", Value::uint(*ts)),
            ]),
            ClientMessage::MenuPick { id, path, title } => Value::object(vec![
                ("type", Value::str("menuPick")),
                ("id", Value::uint(*id)),
                (
                    "path",
                    Value::Array(path.iter().map(|i| Value::uint(*i as u64)).collect()),
                ),
                ("title", Value::str(title.clone())),
            ]),
            ClientMessage::MenuRefresh { id } => Value::object(vec![
                ("type", Value::str("menuRefresh")),
                ("id", Value::uint(*id)),
            ]),
            ClientMessage::Ping { client_micros } => Value::object(vec![
                ("type", Value::str("ping")),
                ("clientMicros", Value::uint(*client_micros)),
            ]),
            ClientMessage::Clipboard { text } => Value::object(vec![
                ("type", Value::str("clipboard")),
                ("text", Value::str(text.clone())),
            ]),
        }
    }

    /// Serialize to a UTF-8 JSON payload (not yet length-prefixed).
    pub fn encode(&self) -> Vec<u8> {
        self.to_value().to_json().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::input::MouseButton;

    #[test]
    fn decodes_hello() {
        let msg = ServerMessage::decode(
            br#"{"type":"hello","protocol":5,"lanes":[{"lane":0,"vdsSize":{"w":5120,"h":2880}},{"lane":1,"vdsSize":{"w":4096,"h":2160}}]}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMessage::Hello {
                protocol: 5,
                lanes: vec![
                    Lane {
                        lane: 0,
                        vds: Size { w: 5120, h: 2880 }
                    },
                    Lane {
                        lane: 1,
                        vds: Size { w: 4096, h: 2160 }
                    },
                ]
            }
        );
    }

    #[test]
    fn decodes_v1_hello_as_its_version_only() {
        let msg = ServerMessage::decode(
            br#"{"type":"hello","protocol":1,"vdsSize":{"w":5120,"h":2880}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMessage::Hello {
                protocol: 1,
                lanes: Vec::new()
            }
        );
    }

    #[test]
    fn decodes_window_moved_from_protocol_example() {
        // The exact JSON payload from protocol.md §4's on-the-wire example.
        let msg = ServerMessage::decode(
            br#"{"type":"windowMoved","id":1,"rect":{"x":2300,"y":500,"w":1312,"h":844}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMessage::WindowMoved {
                id: 1,
                rect: Rect {
                    x: 2300,
                    y: 500,
                    w: 1312,
                    h: 844
                }
            }
        );
    }

    #[test]
    fn decodes_window_created_with_kind_and_unicode_title() {
        let msg = ServerMessage::decode(
            r#"{"type":"windowCreated","id":42,"lane":3,"app":77,"rect":{"x":0,"y":0,"w":800,"h":600},"title":"café — Xcode","kind":"normal"}"#
                .as_bytes(),
        )
        .unwrap();
        match msg {
            ServerMessage::WindowCreated {
                id,
                lane,
                title,
                kind,
                ..
            } => {
                assert_eq!(id, 42);
                assert_eq!(lane, 3);
                assert_eq!(title, "café — Xcode");
                assert_eq!(kind, WindowKind::Normal);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_falls_back_to_normal() {
        let msg = ServerMessage::decode(
            br#"{"type":"windowCreated","id":1,"lane":0,"app":1,"rect":{"x":0,"y":0,"w":1,"h":1},"title":"","kind":"future"}"#,
        )
        .unwrap();
        assert!(matches!(
            msg,
            ServerMessage::WindowCreated {
                kind: WindowKind::Normal,
                ..
            }
        ));
    }

    #[test]
    fn decodes_tile_layout() {
        let msg = ServerMessage::decode(
            br#"{"type":"tileLayout","lane":1,"windows":[{"id":1,"rect":{"x":0,"y":0,"w":100,"h":100}},{"id":2,"rect":{"x":110,"y":0,"w":100,"h":100}}],"displaySize":{"w":5120,"h":2880}}"#,
        )
        .unwrap();
        match msg {
            ServerMessage::TileLayout {
                lane,
                windows,
                display,
            } => {
                assert_eq!(lane, 1);
                assert_eq!(windows.len(), 2);
                assert_eq!(windows[1].id, 2);
                assert_eq!(windows[1].rect.x, 110);
                assert_eq!(display, Size { w: 5120, h: 2880 });
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn decodes_menu_bar_as_the_host_encodes_it() {
        let msg = ServerMessage::decode(
            br#"{"type":"menuBar","app":7,"menus":[{"title":"File","enabled":true,"checked":false,"shortcut":"","separator":false,"items":[{"title":"New Window","enabled":true,"checked":false,"shortcut":"Ctrl+N","separator":false,"items":[]},{"title":"","enabled":false,"checked":false,"shortcut":"","separator":true,"items":[]}]}]}"#,
        )
        .unwrap();
        let ServerMessage::MenuBar { app, menus } = msg else {
            panic!("wrong variant");
        };
        assert_eq!(app, 7);
        assert_eq!(menus[0].title, "File");
        assert_eq!(menus[0].items[0].shortcut, "Ctrl+N");
        assert!(menus[0].items[1].separator);
    }

    #[test]
    fn encodes_menu_pick_and_refresh() {
        assert_eq!(
            ClientMessage::MenuPick {
                id: 3,
                path: vec![2, 0],
                title: "New Window".into()
            }
            .to_value()
            .to_json(),
            r#"{"type":"menuPick","id":3,"path":[2,0],"title":"New Window"}"#
        );
        assert_eq!(
            ClientMessage::MenuRefresh { id: 3 }.to_value().to_json(),
            r#"{"type":"menuRefresh","id":3}"#
        );
    }

    #[test]
    fn unknown_type_is_a_shape_error_not_a_panic() {
        let err = ServerMessage::decode(br#"{"type":"somethingNew","id":1}"#).unwrap_err();
        assert!(matches!(err, DecodeError::Shape(_)));
    }

    #[test]
    fn missing_field_is_a_shape_error() {
        let err = ServerMessage::decode(br#"{"type":"windowMoved","id":1}"#).unwrap_err();
        assert!(matches!(err, DecodeError::Shape(_)));
    }

    #[test]
    fn encodes_request_resize() {
        let json = ClientMessage::RequestResize {
            id: 3,
            size: Size { w: 2560, h: 1440 },
            phase: ResizePhase::End,
        }
        .to_value()
        .to_json();
        assert_eq!(
            json,
            r#"{"type":"requestResize","id":3,"size":{"w":2560,"h":1440},"phase":"end"}"#
        );
    }

    #[test]
    fn encodes_input_matching_protocol_example() {
        // protocol.md §4: the full framed input example's JSON body.
        let json = ClientMessage::Input {
            id: 7,
            event: InputEvent::MouseDown {
                x: 400,
                y: 300,
                button: MouseButton::Left,
            },
            ts: 12897,
        }
        .to_value()
        .to_json();
        assert_eq!(
            json,
            r#"{"type":"input","id":7,"event":{"kind":"mouseDown","x":400,"y":300,"button":"left"},"ts":12897}"#
        );
    }

    #[test]
    fn encodes_focus_and_close() {
        assert_eq!(
            ClientMessage::RequestFocus { id: 9 }.to_value().to_json(),
            r#"{"type":"requestFocus","id":9}"#
        );
        assert_eq!(
            ClientMessage::RequestClose { id: 9 }.to_value().to_json(),
            r#"{"type":"requestClose","id":9}"#
        );
        assert_eq!(
            ClientMessage::RequestKeyframe { id: None }
                .to_value()
                .to_json(),
            r#"{"type":"requestKeyframe"}"#
        );
        assert_eq!(
            ClientMessage::RequestKeyframe { id: Some(2) }
                .to_value()
                .to_json(),
            r#"{"type":"requestKeyframe","id":2}"#
        );
    }

    #[test]
    fn clipboard_both_ways() {
        let msg = ServerMessage::decode(br#"{"type":"clipboard","text":"a\nb \ud83d\ude00"}"#)
            .unwrap();
        assert_eq!(
            msg,
            ServerMessage::Clipboard {
                text: "a\nb \u{1F600}".into()
            }
        );
        assert_eq!(
            ClientMessage::Clipboard {
                text: "say \"hi\"".into()
            }
            .to_value()
            .to_json(),
            r#"{"type":"clipboard","text":"say \"hi\""}"#
        );
    }

    #[test]
    fn decodes_pong_and_encodes_ping_as_the_host_does() {
        let msg = ServerMessage::decode(
            br#"{"type":"pong","clientMicros":1234567,"hostMicros":987654321012}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMessage::Pong {
                client_micros: 1_234_567,
                host_micros: 987_654_321_012
            }
        );
        let json = String::from_utf8(
            ClientMessage::Ping {
                client_micros: 42,
            }
            .encode(),
        )
        .unwrap();
        assert!(json.contains(r#""type":"ping""#), "{json}");
        assert!(json.contains(r#""clientMicros":42"#), "{json}");
    }
}
