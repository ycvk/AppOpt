//! AppOpt：Android 应用线程 CPU 亲和性管理
//!
//! 非 Linux 平台仅编译纯逻辑模块，用于在开发机上运行单元测试。
#![cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]

pub mod cli;
pub mod config;
pub mod cpuset;
mod fsutil;
mod glob;
mod grammar;
pub mod rule_edit;
pub mod settings;
pub mod shared;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod cache;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod daemon;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod ebpf_mode;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod proc_mode;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod procfs;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod watcher;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod web;
