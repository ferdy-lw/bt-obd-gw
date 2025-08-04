use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::boxed::Box;
use alloc::sync::Arc;

use super::mutex::Mutex;
use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_STATE};

extern crate alloc;

// pub struct Mutex<T>(RawMutex, UnsafeCell<T>);

// impl<T> Mutex<T> {
//     #[inline(always)]
//     pub const fn new(data: T) -> Self {
//         Self(RawMutex::new(), UnsafeCell::new(data))
//     }

//     #[inline(always)]
//     pub fn lock(&self) -> MutexGuard<'_, T> {
//         MutexGuard::new(self)
//     }

//     #[inline(always)]
//     pub fn get_mut(&mut self) -> &mut T {
//         self.1.get_mut()
//     }
// }

// unsafe impl<T> Sync for Mutex<T> where T: Send {}
// unsafe impl<T> Send for Mutex<T> where T: Send {}

#[allow(dead_code)]
#[allow(clippy::type_complexity)]
pub(crate) struct BtSingleton<A, R> {
    initialized: AtomicBool,
    callback: Mutex<Option<Arc<UnsafeCell<Box<dyn FnMut(A) -> R>>>>>,
    default_result: R,
}

#[allow(dead_code)]
impl<A, R> BtSingleton<A, R>
where
    R: Clone,
{
    pub const fn new(default_result: R) -> Self {
        Self {
            initialized: AtomicBool::new(false),
            callback: Mutex::new(None),
            default_result,
        }
    }

    pub fn release(&self) -> Result<(), EspError> {
        self.unsubscribe();

        self.initialized
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| EspError::from_infallible::<ESP_ERR_INVALID_STATE>())?;

        Ok(())
    }

    pub fn take(&self) -> Result<(), EspError> {
        self.initialized
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| EspError::from_infallible::<ESP_ERR_INVALID_STATE>())?;

        Ok(())
    }

    pub fn subscribe<'d, F>(&self, callback: F)
    where
        F: FnMut(A) -> R + Send + 'd,
    {
        let callback = unsafe {
            core::mem::transmute::<
                Box<dyn FnMut(A) -> R + Send + 'd>,
                Box<dyn FnMut(A) -> R + Send + 'static>,
            >(Box::new(callback))
        };

        *self.callback.lock() = Some(Arc::new(UnsafeCell::new(callback)));
    }

    pub fn unsubscribe(&self) {
        *self.callback.lock() = None;
    }

    /// Safe to use only from within the ESP IDF Bluedroid task
    pub unsafe fn call(&self, arg: A) -> R {
        if let Some(callback) = self
            .callback
            .lock()
            .as_ref()
            .map(|callback| callback.clone())
        {
            ((callback.get()).as_mut().unwrap())(arg)
        } else {
            self.default_result.clone()
        }
    }
}

unsafe impl<A, R> Sync for BtSingleton<A, R> {}
unsafe impl<A, R> Send for BtSingleton<A, R> {}
