//! COM and MF are scoped to the worker that owns their interfaces.
use super::mf;
use anyhow::{Context, Result};
use std::{marker::PhantomData, rc::Rc};
use windows::Win32::{Media::MediaFoundation::*, System::Com::*};

pub(super) struct Runtime {
    mf: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Runtime {
    pub fn new(mf: bool) -> Result<Self> {
        // SAFETY: Each successful initialization is balanced on this thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.context("initialize COM MTA")?;
        let mut runtime = Self {
            mf: false,
            _thread: PhantomData,
        };
        if mf {
            // SAFETY: COM is initialized and MF is balanced by Drop.
            unsafe { mf::MFStartup(MF_VERSION, MFSTARTUP_FULL) }
                .context("start Media Foundation")?;
            runtime.mf = true;
        }
        Ok(runtime)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: Interfaces are dropped before this guard, on the owning thread.
        unsafe {
            if self.mf {
                let _ = mf::MFShutdown();
            }
            CoUninitialize();
        }
    }
}
