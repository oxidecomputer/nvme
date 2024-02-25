// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{ffi::CStr, ops::Deref};

use crate::{
    controller_info::ControllerInfo,
    error::LibraryError,
    namespace::{NamespaceDiscovery, NamespaceDiscoveryLevel},
    util::FfiPtr,
    Nvme, NvmeError, NvmeErrorCode,
};

use libnvme_sys::nvme::*;

enum ControllerLockLevel {
    Read = NVME_LOCK_L_READ as isize,
    Write = NVME_LOCK_L_WRITE as isize,
}

enum ControllerLockFlags {
    Block = 0,
    DontBlock = NVME_LOCK_F_DONT_BLOCK as isize,
}

pub enum TryLockResult<L, T, E> {
    Ok(L),
    Locked(T),
    Err(E),
}

pub struct Controller<'a> {
    pub(crate) inner: *mut nvme_ctrl_t,
    _nvme: &'a Nvme,
}

impl<'a> Controller<'a> {
    pub fn get_info(&self) -> Result<ControllerInfo<'_>, NvmeError> {
        let mut ctrl_info: *mut nvme_ctrl_info_t = std::ptr::null_mut();
        self.check_result(
            unsafe { nvme_ctrl_info_snap(self.inner, &mut ctrl_info) },
            || "failed to get controller snapshot",
        )
        .map(|_| unsafe { ControllerInfo::from_raw(ctrl_info) })
    }

    fn lock_impl(
        self,
        level: ControllerLockLevel,
        flags: ControllerLockFlags,
    ) -> Result<LockedController<'a>, (Self, NvmeError)> {
        if let Err(e) = self.check_result(
            unsafe { nvme_ctrl_lock(self.inner, level as u32, flags as u32) },
            || "failed to grab nvme controller lock",
        ) {
            return Err((self, e));
        }
        Ok(LockedController { controller: Some(self) })
    }

    pub fn read_lock(self) -> Result<LockedController<'a>, (Self, NvmeError)> {
        self.lock_impl(ControllerLockLevel::Read, ControllerLockFlags::Block)
    }

    pub fn write_lock(self) -> Result<LockedController<'a>, (Self, NvmeError)> {
        self.lock_impl(ControllerLockLevel::Write, ControllerLockFlags::Block)
    }

    pub fn try_read_lock(
        self,
    ) -> TryLockResult<LockedController<'a>, Self, NvmeError> {
        match self.lock_impl(
            ControllerLockLevel::Read,
            ControllerLockFlags::DontBlock,
        ) {
            Ok(lock) => TryLockResult::Ok(lock),
            Err((c, nvme_error)) => match nvme_error {
                _ if nvme_error.code() == NvmeErrorCode::LockWouldBlock => {
                    TryLockResult::Locked(c)
                }
                e => TryLockResult::Err(e),
            },
        }
    }

    pub fn try_write_lock(
        self,
    ) -> TryLockResult<LockedController<'a>, Self, NvmeError> {
        match self.lock_impl(
            ControllerLockLevel::Write,
            ControllerLockFlags::DontBlock,
        ) {
            Ok(lock) => TryLockResult::Ok(lock),
            Err((c, nvme_error)) => match nvme_error {
                _ if nvme_error.code() == NvmeErrorCode::LockWouldBlock => {
                    TryLockResult::Locked(c)
                }
                e => TryLockResult::Err(e),
            },
        }
    }

    pub fn namespace_discovery(
        &self,
        level: NamespaceDiscoveryLevel,
    ) -> Result<NamespaceDiscovery<'_>, NvmeError> {
        NamespaceDiscovery::new(self, level)
    }
}

impl Drop for Controller<'_> {
    fn drop(&mut self) {
        unsafe { nvme_ctrl_fini(self.inner) }
    }
}

pub struct ControllerDiscovery<'a> {
    nvme: &'a Nvme,
    iter: *mut nvme_ctrl_iter_t,
}

impl<'a> Drop for ControllerDiscovery<'a> {
    fn drop(&mut self) {
        unsafe { nvme_ctrl_discover_fini(self.iter) }
    }
}

impl<'a> ControllerDiscovery<'a> {
    pub(crate) fn new(nvme: &'a Nvme) -> Result<Self, NvmeError> {
        let mut iter = std::ptr::null_mut();
        nvme.check_result(
            unsafe { nvme_ctrl_discover_init(nvme.0, &mut iter) },
            || "failed to init nvme controller discovery",
        )
        .map(|_| ControllerDiscovery { nvme, iter })
    }

    fn internal_step(&self) -> Result<Option<Controller<'a>>, NvmeError> {
        let mut nvme_ctr_disc: *const nvme_ctrl_disc_t = std::ptr::null_mut();
        let state =
            unsafe { nvme_ctrl_discover_step(self.iter, &mut nvme_ctr_disc) };
        match state {
            NVME_ITER_VALID => {
                let di_node_t = unsafe { nvme_ctrl_disc_devi(nvme_ctr_disc) };
                let mut nvme_ctrl: *mut nvme_ctrl_t = std::ptr::null_mut();
                self.nvme
                    .check_result(
                        unsafe {
                            nvme_ctrl_init(
                                self.nvme.0,
                                di_node_t,
                                &mut nvme_ctrl,
                            )
                        },
                        || "failed to init nvme controller",
                    )
                    .map(|_| {
                        Some(Controller { inner: nvme_ctrl, _nvme: self.nvme })
                    })
            }
            NVME_ITER_DONE => Ok(None),
            NVME_ITER_ERROR => Err(self
                .nvme
                .fatal_context("failed to iterate nvme controllers")),
            invalid => unreachable!(
                "invalid nvme controller iteration state ({invalid})",
            ),
        }
    }
}

impl<'a> Iterator for ControllerDiscovery<'a> {
    type Item = Result<Controller<'a>, NvmeError>;

    fn next(&mut self) -> Option<Result<Controller<'a>, NvmeError>> {
        self.internal_step().transpose()
    }
}

impl<'a> LibraryError for Controller<'a> {
    type Error = NvmeError;

    fn get_errmsg(&self) -> String {
        let errmsg = unsafe { nvme_ctrl_errmsg(self.inner) };
        unsafe { CStr::from_ptr(errmsg) }.to_string_lossy().to_string()
    }

    fn get_syserr(&self) -> i32 {
        unsafe { nvme_ctrl_syserr(self.inner) }
    }

    fn current_error(
        &self,
        internal: crate::error::InternalError,
    ) -> Self::Error {
        let raw = unsafe { nvme_ctrl_err(self.inner) };
        NvmeError { code: NvmeErrorCode::from_raw(raw), error: internal }
    }
}

pub struct LockedController<'a> {
    pub(crate) controller: Option<Controller<'a>>,
}

impl<'a> Drop for LockedController<'a> {
    fn drop(&mut self) {
        if let Some(controller) = self.controller.take() {
            unsafe { nvme_ctrl_unlock(controller.inner) }
        }
    }
}

impl<'a> LockedController<'a> {
    pub fn unlock(mut self) -> Controller<'a> {
        self.controller.take().expect("controller invariant violated")
    }

    pub fn format_request(
        &self,
    ) -> Result<FormatRequestBuilder<'_>, NvmeError> {
        let controller =
            self.controller.as_ref().expect("controller is locked");
        let mut req = std::ptr::null_mut();
        controller
            .check_result(
                unsafe { nvme_format_req_init(controller.inner, &mut req) },
                || "failed to create format request",
            )
            .map(|_| FormatRequestBuilder { req, controller: self })
    }
}

impl<'a> Deref for LockedController<'a> {
    type Target = Controller<'a>;

    fn deref(&self) -> &Self::Target {
        self.controller.as_ref().expect("controller is locked")
    }
}

pub struct FormatRequestBuilder<'ctrl> {
    req: *mut nvme_format_req_t,
    controller: &'ctrl LockedController<'ctrl>,
}

impl<'ctrl> Drop for FormatRequestBuilder<'ctrl> {
    fn drop(&mut self) {
        unsafe { nvme_format_req_fini(self.req) }
    }
}

impl<'ctrl> FormatRequestBuilder<'ctrl> {
    pub fn set_lbaf(self, lbaf: u32) -> Result<Self, NvmeError> {
        self.controller
            .check_result(
                unsafe { nvme_format_req_set_lbaf(self.req, lbaf) },
                || format!("failed to set LBA format {lbaf} on format request"),
            )
            .map(|_| self)
    }

    pub fn set_nsid(self, nsid: u32) -> Result<Self, NvmeError> {
        self.controller
            .check_result(
                unsafe { nvme_format_req_set_nsid(self.req, nsid) },
                || format!("failed to set nsid {nsid} on format request"),
            )
            .map(|_| self)
    }

    pub fn set_ses(self, ses: u32) -> Result<Self, NvmeError> {
        self.controller
            .check_result(
                unsafe { nvme_format_req_set_ses(self.req, ses) },
                || format!("failed to set ses {ses} on format request"),
            )
            .map(|_| self)
    }

    pub fn execute(self) -> Result<(), NvmeError> {
        self.controller
            .check_result(unsafe { nvme_format_req_exec(self.req) }, || {
                "failed to execute format request"
            })
    }
}
