// SPDX-License-Identifier: GPL-2.0 or MIT

use kernel::{
    drm::{
        gem,
        DeviceContext, //
    },
    prelude::*, //
};

use crate::driver::TyrDrmDriver;

/// GEM Object inner driver data
#[pin_data]
pub(crate) struct TyrObject {}

impl gem::DriverObject for TyrObject {
    type Driver = TyrDrmDriver;
    type Args = ();

    fn new<Ctx: DeviceContext>(
        _dev: &kernel::drm::Device<TyrDrmDriver, Ctx>,
        _size: usize,
        _args: (),
    ) -> impl PinInit<Self, Error> {
        try_pin_init!(TyrObject {})
    }
}
