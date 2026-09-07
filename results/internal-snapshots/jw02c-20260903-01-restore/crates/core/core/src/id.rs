//! 三级关联 ID 体系（writing-rust 已验证的模型）。
//!
//! - `SessionId`：会话。
//! - `TurnId`：一轮用户交互。
//! - `GenerationId`：一次生成（可含多次模型调用/多章）。
//! - `ModelCallId`：一次模型调用，独立配对请求与结果。
//! - `MessageId`：前端消息关联。
//! - `EventId`：事件幂等去重键（journal append 幂等）。
//! - `TaskId`：可跨 Turn 恢复的逻辑任务，不等同于会话或回合。
//! - `StepId`：任务的一次决策步骤，不直接作为外部副作用幂等键。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            /// 生成一个带前缀的新 ID。
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4()))
            }

            pub fn from(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_string()))
            }
        }
    };
}

id_newtype!(SessionId, "sess");
id_newtype!(TurnId, "turn");
id_newtype!(GenerationId, "gen");
id_newtype!(ModelCallId, "mcall");
id_newtype!(MessageId, "msg");
id_newtype!(EventId, "evt");
id_newtype!(TaskId, "task");
id_newtype!(StepId, "step");
