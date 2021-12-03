const MIN_RECOVERY_READ_BLOCK_SIZE: usize = 512;
const MIN_RECOVERY_THREADS: usize = 1;

pub enum RecoveryMode {
    AbsoluteConsistency,

    TolerateTailCorruption,
    TolerateAnyCorruption,
}