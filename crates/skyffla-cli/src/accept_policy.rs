use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AutoAcceptPolicy {
    #[serde(default)]
    pub(crate) file: bool,
    #[serde(default)]
    pub(crate) folder: bool,
    #[serde(default)]
    pub(crate) clipboard: bool,
}

impl AutoAcceptPolicy {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    pub(crate) fn files_and_clipboard() -> Self {
        Self {
            file: true,
            folder: false,
            clipboard: true,
        }
    }

    pub(crate) fn all() -> Self {
        Self {
            file: true,
            folder: true,
            clipboard: true,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.file && !self.folder && !self.clipboard
    }
}

#[cfg(test)]
mod tests {
    use super::AutoAcceptPolicy;

    #[test]
    fn all_policy_accepts_every_kind() {
        let policy = AutoAcceptPolicy::all();

        assert!(policy.file);
        assert!(policy.folder);
        assert!(policy.clipboard);
    }
}
