use std::{
    fmt, fs, io,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{AffinityMode, ExecutionPolicy, HardwareProfile, WorkloadClass};

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug)]
pub enum TemporalError {
    Io(io::Error),
    Invalid(String),
    Expired { expired_at: u64 },
    HardwareMismatch { expected: String, actual: String },
}

impl fmt::Display for TemporalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "temporal policy I/O failed: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid temporal policy: {reason}"),
            Self::Expired { expired_at } => {
                write!(formatter, "temporal policy expired at Unix time {expired_at}")
            }
            Self::HardwareMismatch { expected, actual } => write!(
                formatter,
                "temporal policy targets hardware {expected}, but this machine is {actual}"
            ),
        }
    }
}

impl std::error::Error for TemporalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TemporalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A hardware-specific execution policy with an explicit lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalOptimization {
    pub created_unix_secs: u64,
    pub expires_unix_secs: u64,
    pub hardware_fingerprint: String,
    pub policy: ExecutionPolicy,
}

impl TemporalOptimization {
    pub fn capture(
        hardware: &HardwareProfile,
        policy: ExecutionPolicy,
        ttl: Duration,
    ) -> Result<Self, TemporalError> {
        let created = unix_now()?;
        Ok(Self::new_at(
            hardware,
            policy,
            created,
            ttl.as_secs().max(1),
        ))
    }

    pub fn new_at(
        hardware: &HardwareProfile,
        policy: ExecutionPolicy,
        created_unix_secs: u64,
        ttl_secs: u64,
    ) -> Self {
        Self {
            created_unix_secs,
            expires_unix_secs: created_unix_secs.saturating_add(ttl_secs.max(1)),
            hardware_fingerprint: hardware.fingerprint(),
            policy,
        }
    }

    pub fn default_path(&self) -> PathBuf {
        PathBuf::from(".kashmir/spins").join(format!(
            "{}-{}.spin",
            self.policy.workload.as_str(),
            self.created_unix_secs
        ))
    }

    pub fn validate(&self, hardware: &HardwareProfile) -> Result<(), TemporalError> {
        self.validate_at(hardware, unix_now()?)
    }

    pub fn validate_at(
        &self,
        hardware: &HardwareProfile,
        now_unix_secs: u64,
    ) -> Result<(), TemporalError> {
        if now_unix_secs >= self.expires_unix_secs {
            return Err(TemporalError::Expired {
                expired_at: self.expires_unix_secs,
            });
        }
        let actual = hardware.fingerprint();
        if actual != self.hardware_fingerprint {
            return Err(TemporalError::HardwareMismatch {
                expected: self.hardware_fingerprint.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn environment(&self) -> Vec<(&'static str, String)> {
        vec![
            ("RAYON_NUM_THREADS", self.policy.worker_threads.to_string()),
            (
                "TOKIO_WORKER_THREADS",
                self.policy.worker_threads.to_string(),
            ),
            ("KASHMIR_IO_THREADS", self.policy.io_threads.to_string()),
            ("KASHMIR_WORKLOAD", self.policy.workload.as_str().into()),
            ("KASHMIR_SPIN_CREATED", self.created_unix_secs.to_string()),
            ("KASHMIR_SPIN_EXPIRES", self.expires_unix_secs.to_string()),
            (
                "KASHMIR_HARDWARE_FINGERPRINT",
                self.hardware_fingerprint.clone(),
            ),
        ]
    }

    pub fn encode(&self) -> String {
        format!(
            "schema={SCHEMA_VERSION}\ncreated_unix_secs={}\nexpires_unix_secs={}\nhardware_fingerprint={}\nworkload={}\nworker_threads={}\nio_threads={}\nreserved_cpus={}\ncpu_set={}\naffinity={}\n",
            self.created_unix_secs,
            self.expires_unix_secs,
            self.hardware_fingerprint,
            self.policy.workload.as_str(),
            self.policy.worker_threads,
            self.policy.io_threads,
            self.policy.reserved_cpus,
            comma_separated(&self.policy.cpu_set),
            self.policy.affinity,
        )
    }

    pub fn decode(input: &str) -> Result<Self, TemporalError> {
        let mut fields = std::collections::BTreeMap::new();
        for (line_number, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                TemporalError::Invalid(format!("line {} has no '='", line_number + 1))
            })?;
            if fields.insert(key.trim(), value.trim()).is_some() {
                return Err(TemporalError::Invalid(format!(
                    "duplicate field '{}'",
                    key.trim()
                )));
            }
        }

        let schema = required(&fields, "schema")?
            .parse::<u8>()
            .map_err(|_| TemporalError::Invalid("schema must be an integer".into()))?;
        if schema != SCHEMA_VERSION {
            return Err(TemporalError::Invalid(format!(
                "unsupported schema version {schema}"
            )));
        }

        let created_unix_secs = parse_u64(&fields, "created_unix_secs")?;
        let expires_unix_secs = parse_u64(&fields, "expires_unix_secs")?;
        if expires_unix_secs <= created_unix_secs {
            return Err(TemporalError::Invalid(
                "expiry must be later than creation".into(),
            ));
        }
        let worker_threads = parse_usize(&fields, "worker_threads")?;
        if worker_threads == 0 {
            return Err(TemporalError::Invalid(
                "worker_threads must be greater than zero".into(),
            ));
        }
        let cpu_set = parse_cpu_set(required(&fields, "cpu_set")?)?;
        if cpu_set.len() < worker_threads {
            return Err(TemporalError::Invalid(
                "cpu_set must contain at least worker_threads entries".into(),
            ));
        }

        Ok(Self {
            created_unix_secs,
            expires_unix_secs,
            hardware_fingerprint: required(&fields, "hardware_fingerprint")?.to_owned(),
            policy: ExecutionPolicy {
                workload: required(&fields, "workload")?
                    .parse::<WorkloadClass>()
                    .map_err(TemporalError::Invalid)?,
                worker_threads,
                io_threads: parse_usize(&fields, "io_threads")?,
                reserved_cpus: parse_usize(&fields, "reserved_cpus")?,
                cpu_set,
                affinity: parse_affinity(required(&fields, "affinity")?)?,
                rationale: vec!["restored from a time-bounded Kashmir spin".into()],
            },
        })
    }

    pub fn read(path: &Path) -> Result<Self, TemporalError> {
        Self::decode(&fs::read_to_string(path)?)
    }

    pub fn write(&self, path: &Path) -> Result<(), TemporalError> {
        write_atomic(path, self.encode().as_bytes())?;
        Ok(())
    }

    /// Install the validated policy without modifying user-owned build files.
    pub fn install(&self, target: &Path) -> Result<(PathBuf, PathBuf), TemporalError> {
        if !target.is_dir() {
            return Err(TemporalError::Invalid(format!(
                "target '{}' is not a directory",
                target.display()
            )));
        }
        let root = target.join(".kashmir");
        let policy_path = root.join("active.spin");
        let environment_path = root.join("active.env");
        self.write(&policy_path)?;

        let mut environment = String::new();
        for (key, value) in self.environment() {
            environment.push_str(key);
            environment.push('=');
            environment.push_str(&value);
            environment.push('\n');
        }
        write_atomic(&environment_path, environment.as_bytes())?;
        Ok((policy_path, environment_path))
    }
}

fn unix_now() -> Result<u64, TemporalError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| TemporalError::Invalid(format!("system clock precedes Unix epoch: {error}")))
}

fn required<'a>(
    fields: &'a std::collections::BTreeMap<&str, &str>,
    key: &str,
) -> Result<&'a str, TemporalError> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| TemporalError::Invalid(format!("missing field '{key}'")))
}

fn parse_u64(
    fields: &std::collections::BTreeMap<&str, &str>,
    key: &str,
) -> Result<u64, TemporalError> {
    required(fields, key)?
        .parse()
        .map_err(|_| TemporalError::Invalid(format!("field '{key}' must be an integer")))
}

fn parse_usize(
    fields: &std::collections::BTreeMap<&str, &str>,
    key: &str,
) -> Result<usize, TemporalError> {
    required(fields, key)?
        .parse()
        .map_err(|_| TemporalError::Invalid(format!("field '{key}' must be an integer")))
}

fn parse_cpu_set(value: &str) -> Result<Vec<usize>, TemporalError> {
    if value.is_empty() {
        return Err(TemporalError::Invalid("cpu_set must not be empty".into()));
    }
    let mut cpus = Vec::new();
    for part in value.split(',') {
        let cpu = part
            .parse::<usize>()
            .map_err(|_| TemporalError::Invalid(format!("invalid CPU id '{part}'")))?;
        if cpus.contains(&cpu) {
            return Err(TemporalError::Invalid(format!("duplicate CPU id {cpu}")));
        }
        cpus.push(cpu);
    }
    Ok(cpus)
}

fn parse_affinity(value: &str) -> Result<AffinityMode, TemporalError> {
    match value {
        "none" => Ok(AffinityMode::None),
        "prefer" => Ok(AffinityMode::Prefer),
        "pin" => Ok(AffinityMode::Pin),
        _ => Err(TemporalError::Invalid(format!(
            "unknown affinity mode '{value}'"
        ))),
    }
}

fn comma_separated(values: &[usize]) -> String {
    values
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("policy");
    let temp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(error)
                if path.exists()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
            {
                fs::remove_file(path)?;
                fs::rename(&temp, path)
            }
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cpu, Memory, NumaNode};

    fn hardware(architecture: &str) -> HardwareProfile {
        HardwareProfile {
            architecture: architecture.into(),
            cpus: (0..4)
                .map(|id| Cpu {
                    id,
                    core_id: Some(id),
                    package_id: Some(0),
                    online: true,
                    allowed: true,
                    capacity: Some(100),
                    max_frequency_khz: None,
                    core_type: None,
                })
                .collect(),
            caches: vec![],
            memory: Memory {
                total_bytes: None,
                numa_nodes: vec![NumaNode {
                    id: 0,
                    cpus: vec![0, 1, 2, 3],
                    memory_bytes: None,
                }],
            },
            simd_features: vec![],
            load_average_1m: None,
        }
    }

    fn policy() -> ExecutionPolicy {
        ExecutionPolicy {
            workload: WorkloadClass::CpuBound,
            worker_threads: 2,
            io_threads: 1,
            reserved_cpus: 2,
            cpu_set: vec![0, 1],
            affinity: AffinityMode::Prefer,
            rationale: vec!["test".into()],
        }
    }

    #[test]
    fn manifest_round_trips() {
        let optimization = TemporalOptimization::new_at(&hardware("test"), policy(), 100, 60);
        let decoded = TemporalOptimization::decode(&optimization.encode()).unwrap();
        assert_eq!(decoded.created_unix_secs, 100);
        assert_eq!(decoded.expires_unix_secs, 160);
        assert_eq!(decoded.policy.worker_threads, 2);
        assert_eq!(decoded.policy.cpu_set, vec![0, 1]);
    }

    #[test]
    fn expiry_and_hardware_are_validated() {
        let source = hardware("source");
        let optimization = TemporalOptimization::new_at(&source, policy(), 100, 60);
        assert!(optimization.validate_at(&source, 159).is_ok());
        assert!(matches!(
            optimization.validate_at(&source, 160),
            Err(TemporalError::Expired { .. })
        ));
        assert!(matches!(
            optimization.validate_at(&hardware("other"), 120),
            Err(TemporalError::HardwareMismatch { .. })
        ));
    }

    #[test]
    fn install_writes_policy_and_environment() {
        let root = std::env::temp_dir().join(format!(
            "kashmir-temporal-test-{}-{}",
            std::process::id(),
            unix_now().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let optimization = TemporalOptimization::new_at(&hardware("test"), policy(), 100, 60);
        let (policy_path, env_path) = optimization.install(&root).unwrap();
        assert_eq!(TemporalOptimization::read(&policy_path).unwrap().policy, policy());
        let environment = fs::read_to_string(env_path).unwrap();
        assert!(environment.contains("RAYON_NUM_THREADS=2"));
        fs::remove_dir_all(root).unwrap();
    }
}
