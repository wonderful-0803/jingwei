use jingwei::id::TaskId;
use jingwei::reference::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryContract {
    pub submission_tools: Vec<String>,
    pub artifacts: Vec<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}
pub struct Delivery {
    pub task: TaskId,
    pub root: PathBuf,
    pub contract: DeliveryContract,
    receipt: Mutex<Option<Vec<Artifact>>>,
}
impl Delivery {
    pub fn new(task: TaskId, root: PathBuf, contract: DeliveryContract) -> Self {
        Self {
            task,
            root,
            contract,
            receipt: Mutex::new(None),
        }
    }
    pub fn invalidate(&self) {
        *self.receipt.lock().unwrap() = None;
    }
    fn snapshot(&self) -> Option<Vec<Artifact>> {
        if self.contract.artifacts.is_empty() || self.contract.artifacts.len() > 32 {
            return None;
        }
        self.contract
            .artifacts
            .iter()
            .map(|rel| {
                if rel.as_os_str().is_empty() {
                    return None;
                }
                let mut full = self.root.clone();
                for component in rel.components() {
                    let Component::Normal(name) = component else {
                        return None;
                    };
                    full.push(name);
                    if std::fs::symlink_metadata(&full)
                        .ok()?
                        .file_type()
                        .is_symlink()
                    {
                        return None;
                    }
                }
                let metadata = std::fs::metadata(&full).ok()?;
                if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
                    return None;
                }
                let mut file = std::fs::File::open(full).ok()?.take(32 * 1024 * 1024 + 1);
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).ok()?;
                if bytes.len() > 32 * 1024 * 1024 {
                    return None;
                }
                Some(Artifact {
                    path: rel.clone(),
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                })
            })
            .collect()
    }
    pub fn accept(&self, tool: &str) -> bool {
        if !self.contract.submission_tools.iter().any(|n| n == tool) {
            return false;
        }
        let snapshot = self.snapshot();
        let ok = snapshot.is_some();
        *self.receipt.lock().unwrap() = snapshot;
        ok
    }
    pub fn artifacts(&self) -> Option<Vec<Artifact>> {
        self.receipt.lock().unwrap().clone()
    }
}
impl CompletionChecker for Delivery {
    fn check(
        &self,
        input: CompletionInput<'_>,
    ) -> Result<CompletionDecision, ReferenceConfigError> {
        let receipt = self.artifacts();
        if input.context.task_id == &self.task && receipt.is_some() && self.snapshot() == receipt {
            Ok(CompletionDecision::Verified {
                evidence: "host:delivery-integrity:sha256 (not answer correctness)".into(),
            })
        } else {
            Ok(CompletionDecision::Rejected {
                reason: "missing or changed delivery".into(),
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use jingwei::id::TurnId;
    fn fixture() -> Delivery {
        let task = TaskId::new();
        let root = std::env::temp_dir().join(format!("e005-{task}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("answer.txt"), "answer").unwrap();
        Delivery::new(
            task,
            root,
            DeliveryContract {
                submission_tools: vec!["save".into()],
                artifacts: vec!["answer.txt".into()],
            },
        )
    }
    fn verified(d: &Delivery, task: &TaskId) -> bool {
        matches!(
            d.check(CompletionInput {
                context: CheckContext {
                    task_id: task,
                    turn_id: &TurnId::new()
                },
                claim: "done",
                state_version: None
            })
            .unwrap(),
            CompletionDecision::Verified { .. }
        )
    }
    #[test]
    fn receipt_requires_designated_tool_current_files_and_task() {
        let d = fixture();
        assert!(!verified(&d, &d.task));
        assert!(!d.accept("inspect"));
        assert!(d.accept("save"));
        assert!(verified(&d, &d.task));
        assert!(!verified(&d, &TaskId::new()));
        std::fs::write(d.root.join("answer.txt"), "changed").unwrap();
        assert!(!verified(&d, &d.task));
        assert!(d.accept("save"));
        d.invalidate();
        assert!(!verified(&d, &d.task));
        std::fs::remove_dir_all(d.root).unwrap();
    }
    #[test]
    fn receipt_rejects_missing_and_symlinked_artifacts() {
        let d = fixture();
        std::fs::remove_file(d.root.join("answer.txt")).unwrap();
        assert!(!d.accept("save"));
        std::os::unix::fs::symlink("/etc/hosts", d.root.join("answer.txt")).unwrap();
        assert!(!d.accept("save"));
        std::fs::remove_dir_all(d.root).unwrap();
    }
}
