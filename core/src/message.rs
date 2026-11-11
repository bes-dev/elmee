use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Opaque provider reasoning blocks (Anthropic thinking) that must be
    /// replayed unchanged on the next turn. Absent in old sessions.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub thinking: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "MessageOrigin::is_user")]
    pub origin: MessageOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Pictures attached to this message.
    ///
    /// Kept beside the text rather than replacing `content` with a parts array:
    /// the stored shape is the OpenAI wire shape, so restructuring it would
    /// break every session on disk and both adapters at once, for a field that
    /// is empty in almost every message ever written. Absent in old sessions,
    /// skipped in new ones that have none, and turned into whatever parts each
    /// API wants at its own boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

/// One image, as base64 with the type that says how to read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub media_type: String,
    pub data: String,
}

impl Image {
    /// The providers' wire form; they live in a plugin crate, so this is API.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    User,
    Background,
}

impl MessageOrigin {
    fn is_user(&self) -> bool {
        *self == Self::User
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(deserialize_with = "deserialize_arguments")]
    pub arguments: String,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: function_kind(),
            function: FunctionCall { name: name.into(), arguments: arguments.into() },
        }
    }
}

fn function_kind() -> String {
    "function".into()
}

fn deserialize_arguments<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        value => serde_json::to_string(&value).map_err(serde::de::Error::custom),
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }
    pub fn background(id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut message = Self::text(Role::User, content);
        message.origin = MessageOrigin::Background;
        message.event_id = Some(id.into());
        message
    }
    /// A user-role turn the loop itself authored — a finish-gate complaint or
    /// a continuation prompt. The non-User origin keeps it out of `/rewind`
    /// candidates and stops the compactor treating it as the user's words.
    pub fn kernel(content: impl Into<String>) -> Self {
        let mut message = Self::text(Role::User, content);
        message.origin = MessageOrigin::Background;
        message
    }
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
            thinking: Vec::new(),
            origin: MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            thinking: Vec::new(),
            origin: MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_object_tool_arguments_from_local_servers() {
        let call: ToolCall =
            serde_json::from_str(r#"{"id":"1","function":{"name":"read","arguments":{"path":"a"}}}"#)
                .unwrap();
        assert_eq!(call.kind, "function");
        assert_eq!(call.function.arguments, r#"{"path":"a"}"#);
    }
}
