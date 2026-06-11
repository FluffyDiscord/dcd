/// Every failure mode maps to a stable process exit code (spec §8.1).
#[derive(Debug, thiserror::Error)]
pub enum DcdError {
    #[error("config: {0}")]
    Config(String),

    #[error("{0}")]
    PreCutover(String),

    #[error("{0}")]
    PostCutover(String),

    #[error("another deploy holds {stage} ({holder})")]
    LockHeld { stage: String, holder: String },

    #[error("stage {stage} expects host {expected}, this is {actual}")]
    HostMismatch {
        stage: String,
        expected: String,
        actual: String,
    },

    #[error("lua: {0}")]
    Lua(String),

    #[error("interrupted")]
    Interrupted,
}

impl DcdError {
    pub fn exit_code(&self) -> u8 {
        match self {
            DcdError::PreCutover(_) => 1,
            DcdError::Config(_) => 2,
            DcdError::LockHeld { .. } => 3,
            DcdError::PostCutover(_) => 4,
            DcdError::HostMismatch { .. } => 5,
            DcdError::Lua(_) => 10,
            DcdError::Interrupted => 130,
        }
    }
}

pub type Result<T> = std::result::Result<T, DcdError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(DcdError::PreCutover("x".into()).exit_code(), 1);
        assert_eq!(DcdError::Config("x".into()).exit_code(), 2);
        assert_eq!(
            DcdError::LockHeld {
                stage: "prod".into(),
                holder: "pid 1".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(DcdError::PostCutover("x".into()).exit_code(), 4);
        assert_eq!(
            DcdError::HostMismatch {
                stage: "prod".into(),
                expected: "a".into(),
                actual: "b".into()
            }
            .exit_code(),
            5
        );
        assert_eq!(DcdError::Lua("x".into()).exit_code(), 10);
        assert_eq!(DcdError::Interrupted.exit_code(), 130);
    }
}
