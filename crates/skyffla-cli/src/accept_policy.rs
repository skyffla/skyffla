use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum AutoAcceptTarget {
    File,
    Folder,
    Clipboard,
}

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

    #[cfg(test)]
    pub(crate) fn from_targets(targets: &[AutoAcceptTarget]) -> Self {
        let mut policy = Self::none();
        for target in targets {
            match target {
                AutoAcceptTarget::File => policy.file = true,
                AutoAcceptTarget::Folder => policy.folder = true,
                AutoAcceptTarget::Clipboard => policy.clipboard = true,
            }
        }
        policy
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.file && !self.folder && !self.clipboard
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoAcceptPolicy, AutoAcceptTarget};

    #[test]
    fn policy_from_targets_sets_only_requested_kinds() {
        let policy =
            AutoAcceptPolicy::from_targets(&[AutoAcceptTarget::File, AutoAcceptTarget::Clipboard]);

        assert!(policy.file);
        assert!(!policy.folder);
        assert!(policy.clipboard);
    }
}
