//! 内核态 eBPF 程序与用户态共享的数据布局
#![no_std]

/// 内核 comm 缓冲区长度（TASK_COMM_LEN）
pub const COMM_LEN: usize = 16;
/// 白名单键长度，comm 去掉结尾 NUL 后的最大有效字节数
pub const COMM_KEY_LEN: usize = COMM_LEN - 1;

pub const EVENT_FORK: u32 = 1;
pub const EVENT_EXEC: u32 = 2;
pub const EVENT_RENAME: u32 = 3;
pub const EVENT_EXIT: u32 = 4;

/// 进程事件，经 RingBuf 从内核上报
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcEvent {
    pub pid: i32,
    pub tid: i32,
    pub comm: [u8; COMM_LEN],
    pub event_type: u32,
}

impl ProcEvent {
    pub const SIZE: usize = core::mem::size_of::<ProcEvent>();

    /// 从 RingBuf 记录解析，长度不足返回 None
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        let word = |at: usize| [b[at], b[at + 1], b[at + 2], b[at + 3]];
        let mut comm = [0u8; COMM_LEN];
        comm.copy_from_slice(&b[8..8 + COMM_LEN]);
        Some(Self {
            pid: i32::from_ne_bytes(word(0)),
            tid: i32::from_ne_bytes(word(4)),
            comm,
            event_type: u32::from_ne_bytes(word(8 + COMM_LEN)),
        })
    }
}

/// tracepoint 字段偏移，由用户态解析 tracefs format 文件后注入
/// fork_child_pid 为 0 表示尚未注入
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracepointOffsets {
    pub fork_child_pid: u32,
    pub fork_child_comm: u32,
    pub rename_newcomm: u32,
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for TracepointOffsets {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip() {
        let ev = ProcEvent {
            pid: 7,
            tid: -2,
            comm: *b"surfaceflinger\0\0",
            event_type: EVENT_RENAME,
        };
        let raw =
            unsafe { core::slice::from_raw_parts(&ev as *const _ as *const u8, ProcEvent::SIZE) };
        assert_eq!(ProcEvent::from_bytes(raw), Some(ev));
    }

    #[test]
    fn short_record_rejected() {
        assert_eq!(ProcEvent::from_bytes(&[0u8; ProcEvent::SIZE - 1]), None);
    }
}
