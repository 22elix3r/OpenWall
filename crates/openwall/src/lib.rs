// SPDX-License-Identifier: AGPL-3.0-or-later
//! Internal runtime components shared by the worker and its conformance programs.
//! These APIs are experimental and do not provide a stable external plugin ABI.
#[cfg(all(target_os = "linux", feature = "native-egl"))]
pub mod native_egl;
