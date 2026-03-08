use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use skyffla_protocol::TransferKind;

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

    pub(crate) fn allows_kind(&self, kind: &TransferKind) -> bool {
        match kind {
            TransferKind::File => self.file,
            TransferKind::FolderArchive => self.folder,
            TransferKind::Clipboard => self.clipboard,
            TransferKind::Stdio => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.file {
            parts.push("file");
        }
        if self.folder {
            parts.push("folder");
        }
        if self.clipboard {
            parts.push("clipboard");
        }
        if parts.is_empty() {
            "off".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoAcceptPolicy, AutoAcceptTarget};
    use skyffla_protocol::TransferKind;

    #[test]
    fn policy_from_targets_sets_only_requested_kinds() {
        let policy =
            AutoAcceptPolicy::from_targets(&[AutoAcceptTarget::File, AutoAcceptTarget::Clipboard]);

        assert!(policy.file);
        assert!(!policy.folder);
        assert!(policy.clipboard);
    }

    #[test]
    fn policy_describe_is_human_readable() {
        assert_eq!(AutoAcceptPolicy::none().describe(), "off");
        assert_eq!(
            AutoAcceptPolicy::files_and_clipboard().describe(),
            "file, clipboard"
        );
    }

    #[test]
    fn policy_matches_transfer_kinds() {
        let policy = AutoAcceptPolicy {
            file: true,
            folder: false,
            clipboard: true,
        };

        assert!(policy.allows_kind(&TransferKind::File));
        assert!(!policy.allows_kind(&TransferKind::FolderArchive));
        assert!(policy.allows_kind(&TransferKind::Clipboard));
        assert!(!policy.allows_kind(&TransferKind::Stdio));
    }
}
