//! Stable identifiers shared by control-plane events and diagnostics.
use std::{fmt, sync::Arc};

macro_rules! numeric_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

numeric_id!(SessionId);
numeric_id!(TaskId);
numeric_id!(StreamEpoch);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MountId(Arc<str>);

impl MountId {
    pub fn new(path: impl Into<Arc<str>>) -> Self {
        Self(path.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<Arc<str>> for MountId {
    fn from(value: Arc<str>) -> Self {
        Self(value)
    }
}

impl From<&str> for MountId {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}
